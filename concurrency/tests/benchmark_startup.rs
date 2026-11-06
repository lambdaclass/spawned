//! Exercise the actual benchmark rendezvous under startup failure.
#[path = "../benches/support/start_gate.rs"]
mod start_gate;
use start_gate::StartGate;
use std::time::Duration;
#[test]
fn every_participant_must_arrive() {
    let gate = StartGate::new(3);
    let a = gate.participant();
    let b = gate.participant();
    let c = gate.participant();
    std::thread::scope(|scope| {
        let first = scope.spawn(move || a.wait());
        let second = scope.spawn(move || b.wait());
        assert_eq!(c.wait(), Ok(()));
        assert_eq!(first.join().unwrap(), Ok(()));
        assert_eq!(second.join().unwrap(), Ok(()));
    });
}
#[test]
fn failed_spawn_or_setup_releases_waiters() {
    let gate = StartGate::new(3);
    let failed = gate.participant();
    let a = gate.participant();
    let b = gate.participant();
    std::thread::scope(|scope| {
        let first = scope.spawn(move || a.wait());
        let second = scope.spawn(move || b.wait());
        drop(failed); // Same captured-closure drop as an OS thread spawn failure.
        assert!(first.join().unwrap().is_err());
        assert!(second.join().unwrap().is_err());
    });
}
#[test]
fn panic_during_setup_aborts_the_rendezvous() {
    let gate = StartGate::new(2);
    let failed = gate.participant();
    let waiter = gate.participant();
    let result = std::panic::catch_unwind(move || {
        let _guard = failed;
        panic!("injected setup failure");
    });
    assert!(result.is_err());
    assert!(waiter.wait().is_err());
}
#[test]
fn missing_participant_has_a_finite_wait() {
    let gate = StartGate::with_timeout(2, Duration::from_millis(10));
    assert!(gate.participant().wait().is_err());
    assert!(gate.participant().wait().is_err());
}
