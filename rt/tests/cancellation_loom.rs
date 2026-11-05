#![cfg(spawned_loom)]
//! Uses the actual CancellationToken implementation with Loom synchronization.
use loom::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use spawned_rt::threads::CancellationToken;

#[test]
fn registration_racing_repeated_cancel_fires_once() {
    loom::model(|| {
        let token = CancellationToken::new();
        let count = Arc::new(AtomicUsize::new(0));
        let registering = token.clone();
        let called = count.clone();
        let registration = loom::thread::spawn(move || {
            registering.on_cancel(Box::new(move || {
                called.fetch_add(1, Ordering::SeqCst);
            }));
        });
        let cancelling = token.clone();
        let cancellation = loom::thread::spawn(move || {
            cancelling.cancel();
            cancelling.cancel();
        });
        token.cancel();
        registration.join().unwrap();
        cancellation.join().unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert!(token.is_cancelled());
    });
}

#[test]
fn callbacks_can_register_callbacks_without_deadlocking() {
    loom::model(|| {
        let token = CancellationToken::new();
        let nested = token.clone();
        let count = Arc::new(AtomicUsize::new(0));
        let called = count.clone();
        token.on_cancel(Box::new(move || {
            nested.on_cancel(Box::new(move || {
                called.fetch_add(1, Ordering::SeqCst);
            }));
        }));
        token.cancel();
        assert_eq!(count.load(Ordering::SeqCst), 1);
    });
}
