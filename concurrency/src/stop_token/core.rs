//! The flat actor stop flag. A stop publishes before notifying waiters.
//! The notifier must latch cancellation: registering after notify cannot miss it.
use super::sync::{AtomicBool, Ordering};
use std::task::Poll;

#[derive(Default)]
pub(crate) struct StopFlag(AtomicBool);
impl StopFlag {
    #[inline]
    pub(crate) fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    /// Check only after the mailbox acquire. A message sent after stop
    /// carries that stop publication to this check; a pre-receive snapshot
    /// could miss both the stop and the subsequent message.
    #[inline]
    pub(crate) fn after_receive<T>(
        &self,
        received: Poll<Option<T>>,
        hold: impl FnOnce(T),
    ) -> Poll<Option<T>> {
        match received {
            Poll::Ready(Some(item)) => {
                if self.is_cancelled() {
                    hold(item);
                    Poll::Ready(None)
                } else {
                    Poll::Ready(Some(item))
                }
            }
            other => other,
        }
    }

    pub(crate) fn cancel(&self, notify: impl FnOnce()) {
        self.0.store(true, Ordering::Release);
        notify();
    }
}
