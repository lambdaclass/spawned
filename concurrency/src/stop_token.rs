//! Private, flat actor cancellation: one atomic load on the message path;
//! Tokio's latched cancellation waiter retains the parking/wakeup protocol.
//! No raw token or child token escapes, so every cancellation sets the flag.
use std::sync::Arc;
mod sync {
    pub(crate) use std::sync::atomic::{AtomicBool, Ordering};
}
mod core;
use self::core::StopFlag;

#[derive(Clone, Default)]
pub(crate) struct StopToken(Arc<State>);
#[derive(Default)]
struct State {
    flag: StopFlag,
    waiter: spawned_rt::tasks::CancellationToken,
}
impl StopToken {
    pub(crate) fn new() -> Self {
        Self::default()
    }
    #[inline]
    pub(crate) fn is_cancelled(&self) -> bool {
        self.0.flag.is_cancelled()
    }
    #[inline]
    pub(crate) fn after_receive<T>(
        &self,
        received: std::task::Poll<Option<T>>,
        hold: impl FnOnce(T),
    ) -> std::task::Poll<Option<T>> {
        self.0.flag.after_receive(received, hold)
    }
    pub(crate) fn cancel(&self) {
        self.0.flag.cancel(|| self.0.waiter.cancel());
    }
    pub(crate) async fn cancelled(&self) {
        if !self.is_cancelled() {
            // Tokio latches cancellation, including the gap between the
            // atomic check and the first poll/registration of this future.
            self.0.waiter.cancelled().await;
        }
    }
}

#[cfg(all(test, not(spawned_sim)))]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn cancel_before_first_poll_is_visible_to_later_waiters() {
        let token = StopToken::new();
        let clone = token.clone();
        let waiter = token.cancelled();
        clone.cancel();
        assert!(token.is_cancelled());
        tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_registered_waiter_is_woken_by_a_foreign_thread() {
        for _ in 0..100 {
            let token = StopToken::new();
            let clone = token.clone();
            let waiter = token.cancelled();
            let mut waiter = std::pin::pin!(waiter);
            assert!(futures::poll!(&mut waiter).is_pending());
            let canceller = std::thread::spawn(move || {
                clone.cancel();
                clone.cancel();
            });
            tokio::time::timeout(Duration::from_secs(5), waiter)
                .await
                .unwrap();
            assert!(token.is_cancelled());
            canceller.join().unwrap();
        }
    }
}
