//! Production stop-flag ordering with a latched notifier stand-in. Tokio's
//! own waiter registration/wakeup internals remain a trusted dependency.
#![cfg(spawned_scheduler_loom)]
mod sync {
    pub(crate) use loom::sync::atomic::{AtomicBool, Ordering};
}
#[deny(clippy::disallowed_types, clippy::disallowed_methods)] // concurrency/clippy.toml
#[path = "../src/stop_token/core.rs"]
mod core;
use self::core::StopFlag;
use loom::sync::atomic::{AtomicUsize, Ordering};
use loom::sync::{Arc, Condvar, Mutex};

#[test]
fn check_register_and_cancel_cannot_lose_a_stop() {
    loom::model(|| {
        let flag = Arc::new(StopFlag::default());
        let notified = Arc::new((Mutex::new(false), Condvar::new()));
        let canceller = {
            let flag = flag.clone();
            let notified = notified.clone();
            loom::thread::spawn(move || {
                flag.cancel(|| {
                    *notified.0.lock().unwrap() = true;
                    notified.1.notify_all();
                })
            })
        };
        if !flag.is_cancelled() {
            let mut ready = notified.0.lock().unwrap();
            while !*ready {
                ready = notified.1.wait(ready).unwrap();
            }
        }
        assert!(flag.is_cancelled(), "a completed waiter must observe stop");
        canceller.join().unwrap();
    });
}

#[test]
fn the_fast_stop_check_observes_prior_reason_publication() {
    loom::model(|| {
        let flag = Arc::new(StopFlag::default());
        let reason = Arc::new(AtomicUsize::new(0));
        let canceller = {
            let flag = flag.clone();
            let reason = reason.clone();
            loom::thread::spawn(move || {
                reason.store(42, Ordering::Relaxed);
                flag.cancel(|| {});
            })
        };
        if flag.is_cancelled() {
            assert_eq!(reason.load(Ordering::Relaxed), 42);
        }
        canceller.join().unwrap();
    });
}

/// The queue acquire carries stop publication to the post-receive check.
/// Ignoring that check reproduces the untraced actor-loop regression.
#[test]
fn a_post_stop_message_is_retained_instead_of_dispatched() {
    loom::model(|| {
        let flag = Arc::new(StopFlag::default());
        let (send, receive) = loom::sync::mpsc::channel();
        let producer = {
            let flag = flag.clone();
            loom::thread::spawn(move || {
                flag.cancel(|| {});
                send.send(42).unwrap();
            })
        };
        let received = std::task::Poll::Ready(Some(receive.recv().unwrap()));
        let mut held = Vec::new();
        assert_eq!(
            flag.after_receive(received, |item| held.push(item)),
            std::task::Poll::Ready(None)
        );
        assert_eq!(held, vec![42]);
        producer.join().unwrap();
    });
}

#[test]
fn receive_racing_stop_never_dispatches_the_later_message() {
    loom::model(|| {
        let flag = Arc::new(StopFlag::default());
        let (send, receive) = loom::sync::mpsc::channel();
        let producer = {
            let flag = flag.clone();
            loom::thread::spawn(move || {
                send.send(1).unwrap();
                flag.cancel(|| {});
                send.send(2).unwrap();
            })
        };
        let mut held = Vec::new();
        let mut handled = Vec::new();
        for _ in 0..2 {
            let received = std::task::Poll::Ready(Some(receive.recv().unwrap()));
            if let std::task::Poll::Ready(Some(item)) =
                flag.after_receive(received, |item| held.push(item))
            {
                handled.push(item);
            }
        }
        assert!(!handled.contains(&2));
        assert_eq!(handled.len() + held.len(), 2);
        producer.join().unwrap();
    });
}
