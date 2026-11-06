//! One-owner future-poll gate. A wake during poll becomes a queued continuation.
#[cfg(spawned_loom)]
use loom::sync::atomic::{AtomicUsize, Ordering};
#[cfg(not(spawned_loom))]
use std::sync::atomic::{AtomicUsize, Ordering};
const IDLE: usize = 0;
const QUEUED: usize = 1;
const RUNNING: usize = 2;
const NOTIFIED: usize = 4;
const DONE: usize = 8;

pub(crate) struct Gate(AtomicUsize);
impl Gate {
    pub(crate) fn new() -> Self {
        Self(AtomicUsize::new(IDLE))
    }
    /// True means this caller alone must publish exactly one queue entry.
    pub(crate) fn notify(&self) -> bool {
        let mut state = self.0.load(Ordering::Acquire);
        loop {
            if state & (QUEUED | DONE) != 0 {
                return false;
            }
            let next = if state & RUNNING != 0 {
                state | NOTIFIED
            } else {
                QUEUED
            };
            match self
                .0
                .compare_exchange_weak(state, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return next == QUEUED,
                Err(actual) => state = actual,
            }
        }
    }
    pub(crate) fn claim(&self) -> bool {
        self.0
            .compare_exchange(QUEUED, RUNNING, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
    /// Call only after releasing the future lock. A concurrent wake either set
    /// NOTIFIED or observes IDLE and publishes its own entry; neither is lost.
    pub(crate) fn pending(&self) -> bool {
        let mut state = self.0.load(Ordering::Acquire);
        loop {
            assert!(state & RUNNING != 0);
            let next = if state & NOTIFIED != 0 { QUEUED } else { IDLE };
            match self
                .0
                .compare_exchange_weak(state, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return next == QUEUED,
                Err(actual) => state = actual,
            }
        }
    }
    pub(crate) fn complete(&self) {
        self.0.store(DONE, Ordering::Release);
    }
}

#[cfg(all(test, spawned_loom))]
mod loom_tests {
    use super::*;
    use loom::{sync::Arc, thread};
    #[test]
    fn wake_during_or_after_pending_is_enqueued_once() {
        loom::model(|| {
            let gate = Arc::new(Gate::new());
            let entries = Arc::new(AtomicUsize::new(0));
            assert!(gate.notify());
            assert!(gate.claim());
            let (g, e) = (gate.clone(), entries.clone());
            let wake = thread::spawn(move || {
                if g.notify() {
                    e.fetch_add(1, Ordering::SeqCst);
                }
            });
            if gate.pending() {
                entries.fetch_add(1, Ordering::SeqCst);
            }
            wake.join().unwrap();
            assert_eq!(entries.load(Ordering::SeqCst), 1);
            assert!(gate.claim());
            assert!(!gate.claim());
            gate.complete();
        });
    }
    #[test]
    fn duplicate_wakes_do_not_duplicate_entries() {
        loom::model(|| {
            let gate = Arc::new(Gate::new());
            let entries = Arc::new(AtomicUsize::new(0));
            let (g, e) = (gate.clone(), entries.clone());
            let wake = thread::spawn(move || {
                if g.notify() {
                    e.fetch_add(1, Ordering::SeqCst);
                }
            });
            if gate.notify() {
                entries.fetch_add(1, Ordering::SeqCst);
            }
            wake.join().unwrap();
            assert_eq!(entries.load(Ordering::SeqCst), 1);
            assert!(gate.claim());
            assert!(!gate.claim());
            gate.complete();
        });
    }
    #[test]
    fn coalesced_wake_publishes_data_to_the_next_poll() {
        for already_running in [false, true] {
            loom::model(move || {
                let gate = Arc::new(Gate::new());
                let data = Arc::new(AtomicUsize::new(0));
                assert!(gate.notify());
                if already_running {
                    assert!(gate.claim());
                    assert!(!gate.notify());
                }
                let (g, d) = (gate.clone(), data.clone());
                let wake = thread::spawn(move || {
                    d.store(1, Ordering::Relaxed);
                    g.notify();
                });
                let g = gate.clone();
                let poll = thread::spawn(move || {
                    if already_running {
                        assert!(g.pending());
                    }
                    assert!(g.claim());
                    let observed = data.load(Ordering::Relaxed);
                    g.pending();
                    observed
                });
                wake.join().unwrap();
                let observed = poll.join().unwrap();
                // Either this poll acquired the wake's publication, or a later
                // wake must have scheduled another poll that can observe it.
                assert!(
                    observed == 1 || gate.claim(),
                    "coalesced wake lost data publication"
                );
            });
        }
    }
    #[test]
    fn wake_while_completing_never_resurrects_future() {
        loom::model(|| {
            let gate = Arc::new(Gate::new());
            assert!(gate.notify());
            assert!(gate.claim());
            let g = gate.clone();
            let wake = thread::spawn(move || assert!(!g.notify()));
            gate.complete();
            wake.join().unwrap();
            assert!(!gate.notify());
            assert!(!gate.claim());
        });
    }
}
