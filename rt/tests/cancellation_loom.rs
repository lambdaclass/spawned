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

/// A registration dropped while the token is cancelled: the callback runs at
/// most once, its captures are released on either path, and nothing stays
/// registered.
#[test]
fn unregister_racing_cancel_fires_at_most_once_and_leaves_nothing() {
    loom::model(|| {
        let token = CancellationToken::new();
        let count = Arc::new(AtomicUsize::new(0));
        let called = count.clone();
        let registration = token.register_on_cancel(Box::new(move || {
            called.fetch_add(1, Ordering::SeqCst);
        }));
        let unregistering = loom::thread::spawn(move || drop(registration));
        let cancelling = token.clone();
        let cancellation = loom::thread::spawn(move || cancelling.cancel());
        unregistering.join().unwrap();
        cancellation.join().unwrap();
        assert!(count.load(Ordering::SeqCst) <= 1);
        assert_eq!(Arc::strong_count(&count), 1, "the callback was not dropped");
        assert_eq!(token.callbacks_registered(), 0);
    });
}

/// Unregistering one callback while another registration races the cancel:
/// the kept one fires exactly once, the dropped one never after its drop.
#[test]
fn unregister_leaves_other_registrations_to_fire() {
    loom::model(|| {
        let token = CancellationToken::new();
        let kept = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicUsize::new(0));
        let (k, d) = (kept.clone(), dropped.clone());
        let keep = token.register_on_cancel(Box::new(move || {
            k.fetch_add(1, Ordering::SeqCst);
        }));
        let gone = token.register_on_cancel(Box::new(move || {
            d.fetch_add(1, Ordering::SeqCst);
        }));
        drop(gone);
        let cancelling = token.clone();
        let cancellation = loom::thread::spawn(move || cancelling.cancel());
        let registering = token.clone();
        let late = Arc::new(AtomicUsize::new(0));
        let l = late.clone();
        let registration = loom::thread::spawn(move || {
            registering.register_on_cancel(Box::new(move || {
                l.fetch_add(1, Ordering::SeqCst);
            }))
        });
        cancellation.join().unwrap();
        let late_registration = registration.join().unwrap();
        assert_eq!(kept.load(Ordering::SeqCst), 1);
        assert_eq!(dropped.load(Ordering::SeqCst), 0);
        // Registered before or after the cancel, it fired exactly once.
        assert_eq!(late.load(Ordering::SeqCst), 1);
        drop((keep, late_registration));
        assert_eq!(token.callbacks_registered(), 0);
    });
}
