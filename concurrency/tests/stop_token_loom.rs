//! Production stop-flag ordering with a latched notifier stand-in. Tokio's
//! own waiter registration/wakeup internals remain a trusted dependency.
#![cfg(spawned_scheduler_loom)]
mod sync {
    pub(crate) use loom::sync::atomic::{AtomicBool, Ordering};
}
#[path = "../src/stop_token/core.rs"]
mod core;
use core::StopFlag;
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
