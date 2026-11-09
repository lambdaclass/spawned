//! Shared observation log with a bounded condition wait. Waits are handshakes:
//! the predicate is re-checked on every recorded event, never on a fixed sleep.
#![allow(dead_code)]
use std::{
    sync::{Arc, Condvar, Mutex},
    time::{Duration, Instant},
};

pub struct Recorder<T> {
    events: Mutex<Vec<T>>,
    changed: Condvar,
}

impl<T: Clone + std::fmt::Debug> Recorder<T> {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            events: Mutex::new(Vec::new()),
            changed: Condvar::new(),
        })
    }
    pub fn push(&self, event: T) {
        self.events
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(event);
        self.changed.notify_all();
    }
    pub fn snapshot(&self) -> Vec<T> {
        self.events
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
    /// Wait until `ready` holds over the recorded events, or panic at `limit`.
    pub fn wait_until(&self, limit: Duration, what: &str, ready: impl Fn(&[T]) -> bool) -> Vec<T> {
        let deadline = Instant::now() + limit;
        let mut events = self.events.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            if ready(&events) {
                return events.clone();
            }
            let now = Instant::now();
            assert!(
                now < deadline,
                "timed out waiting for {what}; recorded {:?}",
                *events
            );
            events = self
                .changed
                .wait_timeout(events, deadline - now)
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
    }
}
