//! `spawned_test::tasks`: mocks, probes, harness and fault injection.
#![cfg(feature = "async")]
use spawned_concurrency::error::ActorError;
use spawned_concurrency::message::Message;
use spawned_concurrency::supervision::{Backoff, ChildSpec, SupervisorConfig};
use spawned_concurrency::tasks::{self, Actor, ActorStart, Context, Handler, Recipient};
use spawned_concurrency::{actor, protocol, ExitReason, Response};
use spawned_test::faults;
use spawned_test::tasks::{
    exit_within, join_within, restart_child, settle, shutdown_supervisor, wait_until, Mock, Probe,
    TestSystem,
};
use spawned_test::{Outcome, Received};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

const WAIT: Duration = Duration::from_secs(2);

#[protocol]
pub trait InventoryProtocol: Send + Sync {
    fn reserve(&self, item: String) -> Response<bool>;
    fn restock(&self, item: String, quantity: u32) -> Result<(), ActorError>;
    fn audit(&self) -> Result<(), ActorError>;
}

#[derive(Debug, PartialEq)]
pub struct Sold(pub String);
impl Message for Sold {
    type Result = ();
}

/// Code under test: holds a protocol reference and a recipient, unchanged for tests.
pub struct Checkout {
    inventory: InventoryRef,
    sales: Recipient<Sold>,
}

#[protocol]
pub trait CheckoutProtocol: Send + Sync {
    fn buy(&self, item: String) -> Response<bool>;
}

#[actor(protocol = CheckoutProtocol)]
impl Checkout {
    #[request_handler]
    async fn buy(&mut self, msg: checkout_protocol::Buy, _: &Context<Self>) -> bool {
        let reserved = self
            .inventory
            .reserve(msg.item.clone())
            .await
            .unwrap_or(false);
        if reserved {
            let _ = self.sales.send(Sold(msg.item));
        } else {
            let _ = self.inventory.restock(msg.item, 10);
        }
        reserved
    }
}

#[tokio::test]
async fn mock_answers_protocol_calls_and_probe_records_sends() {
    let inventory = Mock::new()
        .on::<inventory_protocol::Reserve>(|msg| msg.item == "apple")
        .on::<inventory_protocol::Restock>(|_| ())
        .start();
    let sales = Probe::<Sold>::new();
    let checkout = Checkout {
        inventory: inventory.to_inventory_ref(),
        sales: sales.recipient(),
    }
    .start();

    assert!(checkout.buy("apple".into()).await.unwrap());
    assert!(!checkout.buy("pear".into()).await.unwrap());

    assert_eq!(sales.expect(WAIT).await, Sold("apple".into()));
    sales.expect_no_message(Duration::from_millis(20)).await;

    let first: inventory_protocol::Reserve = inventory.expect(WAIT).await;
    assert_eq!(first.item, "apple");
    let restock: inventory_protocol::Restock = inventory.expect(WAIT).await;
    assert_eq!((restock.item.as_str(), restock.quantity), ("pear", 10));
    assert_eq!(inventory.count::<inventory_protocol::Reserve>(), 2);
    assert_eq!(
        inventory
            .received()
            .iter()
            .map(|r| r.outcome)
            .collect::<Vec<_>>(),
        vec![Outcome::Handled; 3]
    );
    inventory.assert_no_unexpected();
}

#[tokio::test]
async fn protocol_methods_run_directly_on_the_mock() {
    let mock = Mock::new()
        .on::<inventory_protocol::Reserve>(|_| true)
        .allow_sends()
        .start();
    assert!(mock.reserve("x".into()).await.unwrap());
    mock.restock("x".into(), 1).unwrap();
    mock.audit().unwrap();
    mock.settle().await;
    assert_eq!(mock.count::<inventory_protocol::Audit>(), 1);
    assert_eq!(mock.take_all::<inventory_protocol::Restock>().len(), 1);
}

#[tokio::test]
async fn unconfigured_request_fails_loudly() {
    let mock = Mock::new().allow_sends().start();
    let error = mock.reserve("x".into()).await.unwrap_err();
    assert!(matches!(
        error,
        ActorError::ActorStopped(ExitReason::Panic(_))
    ));
    let ExitReason::Panic(reason) = join_within(&mock, WAIT).await else {
        panic!("mock should panic on an unconfigured request");
    };
    assert!(reason.contains("no rule for message"), "{reason}");
    assert!(reason.contains("Reserve"), "{reason}");
    assert_eq!(
        mock.received(),
        vec![Received {
            type_name: std::any::type_name::<inventory_protocol::Reserve>(),
            outcome: Outcome::Unexpected
        }]
    );
    let check =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| mock.assert_no_unexpected()));
    assert!(check.is_err());
}

#[tokio::test]
async fn unconfigured_send_names_allow_sends() {
    let mock = Mock::new().start();
    mock.audit().unwrap();
    let ExitReason::Panic(reason) = join_within(&mock, WAIT).await else {
        panic!("strict mock should panic on an unconfigured send");
    };
    assert!(reason.contains("allow_sends"), "{reason}");
}

#[tokio::test]
async fn delay_forces_a_request_timeout() {
    let mock = Mock::new()
        .on::<inventory_protocol::Reserve>(|_| true)
        .delay::<inventory_protocol::Reserve>(Duration::from_millis(300))
        .start();
    let error = mock
        .with_request_timeout(Duration::from_millis(20))
        .reserve("x".into())
        .await
        .unwrap_err();
    assert!(matches!(error, ActorError::RequestTimeout));
    // The request was received and is still answered late, as with a slow actor.
    let _: inventory_protocol::Reserve = mock.expect(WAIT).await;
}

#[tokio::test]
async fn discard_and_crash_on_inject_message_faults() {
    let mock = Mock::new()
        .discard::<inventory_protocol::Restock>()
        .crash_on::<inventory_protocol::Audit>("audit backend down")
        .start();
    mock.restock("x".into(), 1).unwrap();
    mock.audit().unwrap();
    assert_eq!(
        join_within(&mock, WAIT).await,
        ExitReason::Panic("panic in handler: audit backend down".into())
    );
    let outcomes: Vec<_> = mock.received().iter().map(|r| r.outcome).collect();
    assert_eq!(outcomes, vec![Outcome::Discarded, Outcome::Crashed]);
    assert!(mock.restock("y".into(), 1).is_err());
}

#[tokio::test]
#[should_panic(expected = "expected no message")]
async fn expect_no_message_reports_pending_messages() {
    let probe = Probe::<Sold>::new();
    probe.recipient().send(Sold("late".into())).unwrap();
    probe.expect_no_message(Duration::from_millis(10)).await;
}

#[tokio::test]
#[should_panic(expected = "expected a `tasks_kit::Sold` within")]
async fn expect_times_out_with_a_report() {
    Probe::<Sold>::new().expect(Duration::from_millis(10)).await;
}

#[derive(Debug)]
struct Quote(u32);
impl Message for Quote {
    type Result = u32;
}

#[tokio::test]
async fn replying_probe_answers_and_closed_probe_rejects() {
    let probe = Probe::<Quote>::replying(|q| q.0 * 2);
    let recipient = probe.recipient();
    assert_eq!(
        tasks::request(&*recipient, Quote(21), WAIT).await.unwrap(),
        42
    );
    assert_eq!(probe.expect(WAIT).await.0, 21);

    let silent = Probe::<Quote>::new();
    let error = tasks::request(&*silent.recipient(), Quote(1), WAIT)
        .await
        .unwrap_err();
    // No actor stands behind a probe: its reason is NoProc.
    assert!(matches!(
        error,
        ActorError::ActorStopped(ExitReason::NoProc)
    ));
    assert_eq!(silent.received()[0].outcome, Outcome::Unexpected);

    probe.close();
    assert!(recipient.send(Quote(1)).is_err());
}

struct Counter(Arc<AtomicUsize>);
struct Bump;
impl Message for Bump {
    type Result = ();
}
struct Get;
impl Message for Get {
    type Result = usize;
}
impl Actor for Counter {}
impl Handler<Bump> for Counter {
    async fn handle(
        &mut self,
        _: Bump,
        reply: spawned_concurrency::reply::PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
        self.0.fetch_add(1, Ordering::SeqCst);
        reply.reply(());
    }
}
impl Handler<Get> for Counter {
    async fn handle(
        &mut self,
        _: Get,
        reply: spawned_concurrency::reply::PendingReply<'_, usize>,
        _: &Context<Self>,
    ) {
        reply.reply(self.0.load(Ordering::SeqCst));
    }
}

#[tokio::test]
async fn settle_waits_for_earlier_sends() {
    let seen = Arc::new(AtomicUsize::new(0));
    let counter = Counter(seen.clone()).start();
    for _ in 0..100 {
        counter.send(Bump).unwrap();
    }
    assert_eq!(settle(&counter, Get, WAIT).await, 100);
    wait_until(WAIT, "the counter reached 100", || {
        seen.load(Ordering::SeqCst) == 100
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exit_injects_every_reason() {
    // Exact exit_signal/2 semantics: the target ends with the signal's reason,
    // a kill ends it Killed.
    for (reason, expected) in [
        (ExitReason::Shutdown, ExitReason::Shutdown),
        (
            ExitReason::Panic("injected".into()),
            ExitReason::Panic("injected".into()),
        ),
        (ExitReason::Kill, ExitReason::Killed),
    ] {
        let actor = Counter(Arc::default()).start();
        faults::exit(&actor.child_handle(), reason);
        assert_eq!(join_within(&actor, WAIT).await, expected);
    }
    // Normal is ignored by a non-trapping target.
    let actor = Counter(Arc::default()).start();
    faults::exit(&actor.child_handle(), ExitReason::Normal);
    assert_eq!(settle(&actor, Get, WAIT).await, 0);
    assert_eq!(actor.child_handle().exit_reason(), None);
    actor.context().stop();
    assert_eq!(join_within(&actor, WAIT).await, ExitReason::Normal);
}

#[tokio::test]
#[should_panic(expected = "did not exit within")]
async fn exit_within_fails_instead_of_hanging() {
    let actor = Counter(Arc::default()).start();
    exit_within(&actor.child_handle(), Duration::from_millis(20)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supervised_child_restarts_after_injected_crash() {
    let test = TestSystem::new();
    let mut tree = test
        .system()
        .supervisor(SupervisorConfig {
            backoff: Backoff {
                initial: Duration::ZERO,
                maximum: Duration::ZERO,
                jitter: false,
                ..Backoff::default()
            },
            ..SupervisorConfig::default()
        })
        .unwrap();
    let child = tree
        .add(ChildSpec::new("counter", test.group(), |_| async {
            Ok::<_, String>(Counter(Arc::default()))
        }))
        .unwrap();
    let supervisor = tree.start().await.unwrap();
    let first = child.wait_ready().await.unwrap();
    let initial = child.snapshot().generation;

    let second = restart_child(&child, ExitReason::Panic("boom".into()), WAIT).await;
    assert_ne!(first.id(), second.id());
    assert_eq!(child.snapshot().generation, initial + 1);
    let third = restart_child(&child, ExitReason::Kill, WAIT).await;
    assert_eq!(child.snapshot().generation, initial + 2);
    assert_eq!(settle(&third, Get, WAIT).await, 0);

    let report = shutdown_supervisor(&supervisor, WAIT).await;
    assert!(report.failure.is_none());
    test.shutdown(WAIT).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mock_starts_on_a_test_group() {
    let test = TestSystem::new();
    let mock = test.start_mock(Mock::new().on::<inventory_protocol::Reserve>(|_| true));
    assert!(mock.reserve("x".into()).await.unwrap());
    test.shutdown(WAIT).await;
}

struct Stuck(Arc<AtomicBool>);
struct Hang;
impl Message for Hang {
    type Result = ();
}
impl Actor for Stuck {}
impl Handler<Hang> for Stuck {
    async fn handle(
        &mut self,
        _: Hang,
        reply: spawned_concurrency::reply::PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
        self.0.store(true, Ordering::SeqCst);
        // Blocking code is never preempted, so shutdown cannot finish in time.
        std::thread::sleep(Duration::from_millis(500));
        reply.reply(());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[should_panic(expected = "ActorSystem did not shut down within")]
async fn bounded_shutdown_reports_unfinished_actors() {
    let test = TestSystem::new();
    let entered = Arc::new(AtomicBool::new(false));
    let stuck = test.start(Stuck(entered.clone()));
    stuck.send(Hang).unwrap();
    // Shut down only once the handler is inside its blocking sleep, so the
    // report is deterministic. Dropping the system during the unwind starts a
    // background shutdown and does not join the blocked worker.
    wait_until(WAIT, "the handler is blocking", || {
        entered.load(Ordering::SeqCst)
    })
    .await;
    test.shutdown(Duration::from_millis(50)).await;
}

#[protocol]
pub trait LedgerProtocol: Send + Sync {
    fn note(&self, amount: u32) -> Result<(), ActorError>;
    fn flush(&self) -> Response<()>;
}

#[tokio::test]
async fn allow_sends_also_answers_unit_requests_but_allow_send_is_per_type() {
    // A handler cannot tell a send from a request, so `allow_sends` also
    // acknowledges a request whose reply is `()`.
    let lenient = Mock::new().allow_sends().start();
    lenient.note(1).unwrap();
    lenient.flush().await.unwrap();
    lenient.assert_no_unexpected();

    // `allow_send::<M>()` acknowledges only the chosen type.
    let strict = Mock::new().allow_send::<ledger_protocol::Note>().start();
    strict.note(1).unwrap();
    assert!(strict.flush().await.is_err());
    let ExitReason::Panic(reason) = join_within(&strict, WAIT).await else {
        panic!("strict mock should panic on the unacknowledged request");
    };
    assert!(
        reason.contains(
            "acknowledge it with `.allow_send::<tasks_kit::ledger_protocol::Flush>()` or"
        ),
        "{reason}"
    );
    assert_eq!(strict.count::<ledger_protocol::Note>(), 1);
}

#[tokio::test]
#[should_panic(expected = "could not settle")]
async fn settle_within_uses_the_given_bound() {
    let mock = Mock::new()
        .on::<inventory_protocol::Reserve>(|_| true)
        .delay::<inventory_protocol::Reserve>(Duration::from_millis(500))
        .start();
    // Occupy the mock; the fence queues behind the slow reply.
    let _pending = mock
        .actor_ref()
        .request_raw(inventory_protocol::Reserve { item: "x".into() });
    mock.settle_within(Duration::from_millis(20)).await;
}

/// Under paused Tokio time, every async helper waits on the Tokio clock: idle
/// waits auto-advance instead of spinning against the wall clock.
#[tokio::test(start_paused = true)]
async fn helpers_follow_paused_tokio_time() {
    let real = std::time::Instant::now();
    let hour = Duration::from_secs(3600);

    // expect: a message sent after a long virtual sleep still arrives.
    let probe = Probe::<Sold>::new();
    let recipient = probe.recipient();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(600)).await;
        recipient.send(Sold("later".into())).unwrap();
    });
    assert_eq!(probe.expect(hour).await, Sold("later".into()));

    // expect_no_message: an hour-long window ends without real waiting.
    probe.expect_no_message(hour).await;

    // wait_until: the condition becomes true on the virtual clock.
    let start = tokio::time::Instant::now();
    wait_until(hour, "two virtual seconds passed", || {
        start.elapsed() >= Duration::from_secs(2)
    })
    .await;

    // exit_within: an actor stopped after a virtual delay.
    let mock = Mock::new().start();
    let handle = mock.child_handle();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(600)).await;
        handle.stop();
    });
    assert_eq!(join_within(&mock, hour).await, ExitReason::Normal);

    assert!(
        real.elapsed() < Duration::from_secs(5),
        "paused-time helpers took {:?} of real time",
        real.elapsed()
    );
}

// Regression: a delayed reply is logged in flight on arrival and handled only
// once the delay is over, so `expect` cannot run ahead of the caller.
#[tokio::test(start_paused = true)]
async fn delayed_reply_is_handled_only_after_the_delay() {
    let delay = Duration::from_secs(60);
    let mock = Mock::new()
        .on::<inventory_protocol::Reserve>(|_| true)
        .delay::<inventory_protocol::Reserve>(delay)
        .start();
    let caller = {
        let mock = mock.clone();
        tokio::spawn(async move {
            mock.with_request_timeout(delay * 2)
                .reserve("x".into())
                .await
        })
    };
    wait_until(WAIT, "the reservation arrived", || {
        mock.count::<inventory_protocol::Reserve>() == 1
    })
    .await;
    assert_eq!(mock.received()[0].outcome, Outcome::InFlight);
    assert!(mock.take_all::<inventory_protocol::Reserve>().is_empty());
    let asked: inventory_protocol::Reserve = mock.expect(delay * 2).await;
    assert_eq!(asked.item, "x");
    assert_eq!(mock.received()[0].outcome, Outcome::Handled);
    assert!(caller.await.unwrap().unwrap());
}

#[tokio::test(start_paused = true)]
#[should_panic(expected = "expected a `tasks_kit::Sold` within 3600s")]
async fn expect_times_out_on_paused_time() {
    Probe::<Sold>::new().expect(Duration::from_secs(3600)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_child_waits_out_a_restart_in_progress() {
    let test = TestSystem::new();
    let mut tree = test
        .system()
        .supervisor(SupervisorConfig {
            backoff: Backoff {
                initial: Duration::from_millis(100),
                maximum: Duration::from_millis(100),
                jitter: false,
                ..Backoff::default()
            },
            ..SupervisorConfig::default()
        })
        .unwrap();
    let child = tree
        .add(ChildSpec::new("counter", test.group(), |_| async {
            Ok::<_, String>(Counter(Arc::default()))
        }))
        .unwrap();
    let supervisor = tree.start().await.unwrap();
    let first = child.wait_ready().await.unwrap();
    let initial = child.snapshot().generation;

    // Crash the child behind restart_child's back: it is now between
    // generations, so restart_child must fault the replacement instead of
    // panicking or pairing a stale instance with a newer generation.
    faults::exit(&first.child_handle(), ExitReason::Panic("first".into()));
    exit_within(&first.child_handle(), WAIT).await;
    let third = restart_child(&child, ExitReason::Panic("second".into()), WAIT).await;
    let snapshot = child.snapshot();
    assert_eq!(snapshot.generation, initial + 2);
    assert_eq!(snapshot.actor_id, Some(third.id()));
    assert_eq!(settle(&third, Get, WAIT).await, 0);

    let report = shutdown_supervisor(&supervisor, WAIT).await;
    assert!(report.failure.is_none());
    test.shutdown(WAIT).await;
}

// Regression: injecting a panic or a kill never blocks the caller, even on a
// current-thread runtime, where blocking would also stall the target.
#[tokio::test]
async fn exit_injection_never_blocks_a_current_thread_runtime() {
    for reason in [ExitReason::Panic("injected".into()), ExitReason::Kill] {
        let actor = Counter(Arc::default()).start();
        faults::exit(&actor.child_handle(), reason.clone());
        let observed = join_within(&actor, WAIT).await;
        assert!(matches!(
            (&reason, &observed),
            (ExitReason::Panic(_), ExitReason::Panic(_)) | (ExitReason::Kill, ExitReason::Killed)
        ));
        // A second injection into the exited target is a non-blocking no-op.
        faults::exit(&actor.child_handle(), reason);
        assert_eq!(join_within(&actor, WAIT).await, observed);
    }
}

// Regression: `deadline` bounds the whole call, not each of its phases. Each
// phase here waits out a 400 ms backoff, so two phases exceed 600 ms.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[should_panic(expected = "one deadline for the whole call")]
async fn restart_child_has_one_deadline_for_the_whole_call() {
    let test = TestSystem::new();
    let mut tree = test
        .system()
        .supervisor(SupervisorConfig {
            backoff: Backoff {
                initial: Duration::from_millis(400),
                maximum: Duration::from_millis(400),
                jitter: false,
                ..Backoff::default()
            },
            ..SupervisorConfig::default()
        })
        .unwrap();
    let child = tree
        .add(ChildSpec::new("counter", test.group(), |_| async {
            Ok::<_, String>(Counter(Arc::default()))
        }))
        .unwrap();
    let _supervisor = tree.start().await.unwrap();
    let first = child.wait_ready().await.unwrap();
    faults::exit(&first.child_handle(), ExitReason::Panic("first".into()));
    exit_within(&first.child_handle(), WAIT).await;
    restart_child(
        &child,
        ExitReason::Panic("second".into()),
        Duration::from_millis(600),
    )
    .await;
}

// Async helpers name the test line that called them.
#[tokio::test]
#[should_panic(expected = "(expect called at testing/tests/tasks_kit.rs:")]
async fn async_expect_names_its_caller() {
    Probe::<Sold>::new().expect(Duration::from_millis(1)).await;
}

#[tokio::test]
async fn kill_stops_a_trapping_target_and_other_signals_are_trapped() {
    // A kill is untrappable: the trapping target ends Killed and its trapping
    // peer receives the (trappable) Killed notice and survives.
    let target = Counter(Arc::default()).start();
    let observer = Counter(Arc::default()).start();
    target.context().trap_exit(true);
    observer.context().trap_exit(true);
    observer.context().link(&target.child_handle());
    faults::exit(&target.child_handle(), ExitReason::Kill);
    assert_eq!(join_within(&target, WAIT).await, ExitReason::Killed);
    assert_eq!(settle(&observer, Get, WAIT).await, 0);
    assert_eq!(observer.child_handle().exit_reason(), None);
    observer.context().stop();
    assert_eq!(join_within(&observer, WAIT).await, ExitReason::Normal);
    // Any other signal to a trapping target is queued as an Exit.
    let target = Counter(Arc::default()).start();
    target.context().trap_exit(true);
    faults::exit(&target.child_handle(), ExitReason::Shutdown);
    assert_eq!(settle(&target, Get, WAIT).await, 0);
    assert_eq!(target.child_handle().exit_reason(), None);
    target.child_handle().shutdown();
    assert_eq!(join_within(&target, WAIT).await, ExitReason::Shutdown);
}

/// `in_actor` runs a closure in one of the actor's turns, after the messages
/// sent before it, with the state and the `Context`.
#[tokio::test]
async fn in_actor_runs_a_closure_in_a_turn() {
    struct Tally(u32);
    #[actor]
    impl Tally {
        #[send_handler]
        async fn add(&mut self, msg: AddOne, _: &Context<Self>) {
            self.0 += msg.0;
        }
    }
    struct AddOne(u32);
    impl Message for AddOne {
        type Result = ();
    }
    let tally = Tally(0).start();
    tally.send(AddOne(2)).unwrap();
    let (seen, own) = spawned_test::tasks::in_actor(&tally, |state, ctx| {
        state.0 += 1;
        (state.0, ctx.id())
    })
    .await;
    assert_eq!((seen, own), (3, tally.id()));
    tally.context().stop();
    tally.join().await;
}
