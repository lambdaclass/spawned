//! Runnable ownership gate shared by the prototype and Loom model.
#[cfg(spawned_scheduler_loom)]
use loom::sync::Mutex;
#[cfg(not(spawned_scheduler_loom))]
use std::sync::Mutex;

pub struct ReadyGate(Mutex<bool>);
impl ReadyGate {
    pub fn new() -> Self {
        Self(Mutex::new(false))
    }
    /// Called after publishing a message, without holding the mailbox lock.
    pub fn notify(&self, enqueue_ready: impl FnOnce()) {
        let mut scheduled = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if !*scheduled {
            *scheduled = true;
            enqueue_ready();
        }
    }
    /// Caller owns the mailbox. The lookahead and ownership decision are made
    /// under the same gate used by producer notification, preventing lost wakes.
    pub fn finish(&self, has_work: impl FnOnce() -> bool, enqueue_ready: impl FnOnce()) {
        let mut scheduled = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if has_work() {
            enqueue_ready();
        } else {
            *scheduled = false;
        }
    }
}
