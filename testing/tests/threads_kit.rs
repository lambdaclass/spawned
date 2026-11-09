//! `spawned_test::threads`: the same kit for native actors, Tokio-free.
use spawned_concurrency::error::ActorError;
use spawned_concurrency::message::Message;
use spawned_concurrency::threads::{self, Actor, ActorStart, Context, Handler, Recipient};
use spawned_concurrency::{actor, protocol, ExitReason, Response};
use spawned_test::faults;
use spawned_test::threads::{exit_within, join_within, settle, wait_until, Mock, Probe};
use spawned_test::Outcome;
use std::panic::AssertUnwindSafe;
use std::sync::mpsc;
use std::time::Duration;

const WAIT: Duration = Duration::from_secs(2);

#[protocol]
pub trait LedgerProtocol: Send + Sync {
    fn balance(&self, account: u32) -> Response<i64>;
    fn record(&self, account: u32, amount: i64) -> Result<(), ActorError>;
}

#[derive(Debug, PartialEq)]
pub struct Overdrawn(pub u32);
impl Message for Overdrawn {
    type Result = ();
}

/// Code under test: a native actor holding a protocol ref and a recipient.
pub struct Teller {
    ledger: LedgerRef,
    alerts: Recipient<Overdrawn>,
}

#[protocol]
pub trait TellerProtocol: Send + Sync {
    fn withdraw(&self, account: u32, amount: i64) -> Response<bool>;
}

#[actor(protocol = TellerProtocol)]
impl Teller {
    #[request_handler]
    fn withdraw(&mut self, msg: teller_protocol::Withdraw, _: &Context<Self>) -> bool {
        let balance = self.ledger.balance(msg.account).unwrap();
        if balance < msg.amount {
            let _ = self.alerts.send(Overdrawn(msg.account));
            return false;
        }
        let _ = self.ledger.record(msg.account, -msg.amount);
        true
    }
}

#[test]
fn native_mock_and_probe() {
    let ledger = Mock::new()
        .on::<ledger_protocol::Balance>(|msg| if msg.account == 1 { 100 } else { 0 })
        .on::<ledger_protocol::Record>(|_| ())
        .start();
    let alerts = Probe::<Overdrawn>::new();
    let teller = Teller {
        ledger: ledger.to_ledger_ref(),
        alerts: alerts.recipient(),
    }
    .start();

    assert!(teller.withdraw(1, 40).unwrap());
    assert!(!teller.withdraw(2, 40).unwrap());

    assert_eq!(alerts.expect(WAIT), Overdrawn(2));
    alerts.expect_no_message(Duration::from_millis(20));
    let record: ledger_protocol::Record = ledger.expect(WAIT);
    assert_eq!((record.account, record.amount), (1, -40));
    assert_eq!(ledger.count::<ledger_protocol::Balance>(), 2);
    ledger.assert_no_unexpected();
}

#[test]
fn native_delay_forces_timeout_and_unconfigured_fails_loudly() {
    let slow = Mock::new()
        .on::<ledger_protocol::Balance>(|_| 1)
        .delay::<ledger_protocol::Balance>(Duration::from_millis(300))
        .start();
    let via_protocol = slow
        .with_request_timeout(Duration::from_millis(20))
        .balance(1);
    assert!(via_protocol.is_err());
    let error = slow
        .request_with_timeout(
            ledger_protocol::Balance { account: 1 },
            Duration::from_millis(20),
        )
        .unwrap_err();
    assert!(matches!(error, ActorError::RequestTimeout));

    let strict = Mock::new().start();
    assert!(strict.balance(1).is_err());
    let ExitReason::Panic(reason) = join_within(&strict, WAIT) else {
        panic!("strict mock should panic");
    };
    assert!(reason.contains("no rule for message"), "{reason}");
    assert_eq!(strict.received()[0].outcome, Outcome::Unexpected);
}

#[test]
fn native_message_faults_and_settle() {
    let mock = Mock::new()
        .discard::<ledger_protocol::Record>()
        .on::<ledger_protocol::Balance>(|_| 5)
        .start();
    mock.record(1, 1).unwrap();
    mock.settle();
    assert_eq!(mock.received()[0].outcome, Outcome::Discarded);
    mock.record(2, 1).unwrap();
    mock.settle_within(WAIT);
    assert_eq!(mock.count::<ledger_protocol::Record>(), 2);
    assert_eq!(
        settle(&mock, ledger_protocol::Balance { account: 1 }, WAIT),
        5
    );

    let crashing = Mock::new()
        .crash_on::<ledger_protocol::Record>("disk full")
        .start();
    crashing.record(1, 1).unwrap();
    assert_eq!(
        join_within(&crashing, WAIT),
        ExitReason::Panic("panic in handler: disk full".into())
    );
}

#[derive(Debug)]
struct Quote(u32);
impl Message for Quote {
    type Result = u32;
}

#[test]
fn native_replying_and_closed_probe() {
    let probe = Probe::<Quote>::replying(|q| q.0 + 1);
    let recipient = probe.recipient();
    assert_eq!(threads::request(&*recipient, Quote(1), WAIT).unwrap(), 2);
    assert_eq!(probe.take_all().len(), 1);
    probe.close();
    assert!(recipient.send(Quote(1)).is_err());
}

struct Idle;
impl Actor for Idle {}
struct Ping;
impl Message for Ping {
    type Result = ();
}
impl Handler<Ping> for Idle {
    fn handle(&mut self, _: Ping, _: &Context<Self>) {}
}

#[test]
fn native_exit_injection() {
    for reason in [
        ExitReason::Normal,
        ExitReason::Shutdown,
        ExitReason::Panic("injected".into()),
    ] {
        let actor = Idle.start();
        settle(&actor, Ping, WAIT);
        faults::exit(&actor.child_handle(), reason.clone());
        let observed = join_within(&actor, WAIT);
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

#[cfg(feature = "async")]
#[test]
fn native_kill_injection() {
    let actor = Idle.start();
    faults::exit(&actor.child_handle(), ExitReason::Kill);
    assert_eq!(join_within(&actor, WAIT), ExitReason::Kill);
}

#[test]
#[should_panic(expected = "did not exit within")]
fn native_exit_within_fails_instead_of_hanging() {
    let actor = Idle.start();
    exit_within(&actor.child_handle(), Duration::from_millis(20));
}

#[test]
#[should_panic(expected = "waiting until never")]
fn native_wait_until_reports_condition() {
    wait_until(Duration::from_millis(10), "never", || false);
}

// Regression: a message is resolved only once its reply exists. The reply
// closure waits on a channel, which holds the in-flight window open
// deterministically.
#[test]
fn native_expect_waits_for_the_reply_closure() {
    let (open, gate) = mpsc::channel::<()>();
    let mock = Mock::new()
        .on::<Quote>(move |q| {
            gate.recv().unwrap();
            q.0 * 10
        })
        .start();
    let caller = {
        let mock = mock.clone();
        std::thread::spawn(move || mock.request_with_timeout(Quote(4), WAIT))
    };
    wait_until(WAIT, "the quote arrived", || mock.count::<Quote>() == 1);
    assert_eq!(mock.received()[0].outcome, Outcome::InFlight);
    assert!(
        mock.take_all::<Quote>().is_empty(),
        "in flight, not handled"
    );
    open.send(()).unwrap();
    assert_eq!(caller.join().unwrap().unwrap(), 40);
    assert_eq!(mock.expect::<Quote>(WAIT).0, 4);
    assert_eq!(mock.received()[0].outcome, Outcome::Handled);
}

#[test]
#[should_panic(expected = "expected no message")]
fn native_expect_no_message_reports_in_flight_messages() {
    let (_open, gate) = mpsc::channel::<()>();
    let mock = Mock::new()
        .on::<Quote>(move |q| {
            let _ = gate.recv();
            q.0
        })
        .start();
    let _reply = mock.request_raw(Quote(1)).unwrap();
    wait_until(WAIT, "the quote arrived", || mock.count::<Quote>() == 1);
    mock.expect_no_message(Duration::from_millis(5));
}

#[test]
fn native_panicking_reply_closure_still_logs_the_message() {
    let mock = Mock::new()
        .on::<Quote>(|_| panic!("reply exploded"))
        .start();
    assert!(mock.request_with_timeout(Quote(7), WAIT).is_err());
    assert_eq!(
        join_within(&mock, WAIT),
        ExitReason::Panic("panic in handler: reply exploded".into())
    );
    assert_eq!(mock.received()[0].outcome, Outcome::Crashed);
    assert_eq!(mock.expect::<Quote>(WAIT).0, 7);

    let probe = Probe::<Quote>::replying(|_| panic!("probe reply exploded"));
    let recipient = probe.recipient();
    let caught = std::panic::catch_unwind(AssertUnwindSafe(|| {
        threads::request(&*recipient, Quote(8), WAIT)
    }));
    assert!(caught.is_err(), "the reply closure panics in the caller");
    assert_eq!(probe.received()[0].outcome, Outcome::Crashed);
    assert_eq!(probe.expect(WAIT).0, 8);
}

// Regression: once `close()` returns, no racing send is logged.
#[test]
fn native_nothing_is_logged_after_close_returns() {
    for _ in 0..50 {
        let probe = Probe::<Quote>::new();
        let senders: Vec<_> = (0..4)
            .map(|_| {
                let recipient = probe.recipient();
                // Each sender stops at its first rejected send.
                std::thread::spawn(move || while recipient.send(Quote(0)).is_ok() {})
            })
            .collect();
        wait_until(WAIT, "senders are running", || !probe.received().is_empty());
        probe.close();
        let at_close = probe.received().len();
        for sender in senders {
            sender.join().unwrap();
        }
        assert_eq!(probe.received().len(), at_close);
    }
}

#[test]
fn native_panic_injection_into_an_exited_target_is_a_no_op() {
    let actor = Idle.start();
    actor.child_handle().shutdown();
    assert_eq!(join_within(&actor, WAIT), ExitReason::Shutdown);
    // The target's link stops the fuse before it detonates: not an error.
    faults::exit(&actor.child_handle(), ExitReason::Panic("late".into()));
}

// Regression: every Kill injection reaches its target.
#[cfg(feature = "async")]
#[test]
fn native_kill_injection_is_reliable() {
    for _ in 0..200 {
        let actor = Idle.start();
        faults::exit(&actor.child_handle(), ExitReason::Kill);
        assert_eq!(join_within(&actor, WAIT), ExitReason::Kill);
    }
}
