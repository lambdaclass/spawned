//! The flat actor stop flag. A stop publishes before notifying waiters.
//! The notifier must latch cancellation: registering after notify cannot miss it.
use super::sync::{AtomicBool, Ordering};

#[derive(Default)]
pub(crate) struct StopFlag(AtomicBool);
impl StopFlag {
    #[inline]
    pub(crate) fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    pub(crate) fn cancel(&self, notify: impl FnOnce()) {
        self.0.store(true, Ordering::Release);
        notify();
    }
}
