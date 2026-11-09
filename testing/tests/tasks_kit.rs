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
use std::sync::atomic::{AtomicUsize, Ordering};
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
    assert!(matches!(error, ActorError::ActorStopped));
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
    assert!(matches!(error, ActorError::ActorStopped));
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
    async fn handle(&mut self, _: Bump, _: &Context<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
impl Handler<Get> for Counter {
    async fn handle(&mut self, _: Get, _: &Context<Self>) -> usize {
        self.0.load(Ordering::SeqCst)
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
    for reason in [
        ExitReason::Normal,
        ExitReason::Shutdown,
        ExitReason::Panic("injected".into()),
        ExitReason::Kill,
    ] {
        let actor = Counter(Arc::default()).start();
        faults::exit(&actor.child_handle(), reason.clone());
        let observed = join_within(&actor, WAIT).await;
        match reason {
            ExitReason::Panic(msg) => {
                assert_eq!(
                    observed,
                    ExitReason::Panic(format!("panic in handler: {msg}"))
                )
            }
            other => assert_eq!(observed, other),
        }
    }
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

struct Stuck;
struct Hang;
impl Message for Hang {
    type Result = ();
}
impl Actor for Stuck {}
impl Handler<Hang> for Stuck {
    async fn handle(&mut self, _: Hang, _: &Context<Self>) {
        // Blocking code is never preempted, so shutdown cannot finish in time.
        std::thread::sleep(Duration::from_millis(500));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[should_panic(expected = "ActorSystem did not shut down within")]
async fn bounded_shutdown_reports_unfinished_actors() {
    let test = TestSystem::new();
    let stuck = test.start(Stuck);
    stuck.send(Hang).unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    test.shutdown(Duration::from_millis(50)).await;
}
