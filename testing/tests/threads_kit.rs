//! `spawned_test::threads`: the same kit for native actors, Tokio-free.
use spawned_concurrency::error::ActorError;
use spawned_concurrency::message::Message;
use spawned_concurrency::threads::{self, Actor, ActorStart, Context, Handler, Recipient};
use spawned_concurrency::{actor, protocol, ExitReason, Response};
use spawned_test::faults;
use spawned_test::threads::{exit_within, join_within, settle, wait_until, Mock, Probe};
use spawned_test::Outcome;
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
