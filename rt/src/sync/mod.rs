//! Runtime-independent lifecycle primitives. Default async builds retain Tokio's
//! primitives; thread-only builds use std synchronization and removable wakers.
#[cfg(feature = "async")]
pub use crate::tasks::{watch, CancellationToken};
#[cfg(not(feature = "async"))]
pub mod watch;
#[cfg(not(feature = "async"))]
#[derive(Clone)]
pub struct CancellationToken(std::sync::Arc<watch::Sender<bool>>);
#[cfg(not(feature = "async"))]
impl CancellationToken {
    pub fn new() -> Self {
        let (sender, _) = watch::channel(false);
        Self(std::sync::Arc::new(sender))
    }
    pub fn is_cancelled(&self) -> bool {
        *self.0.borrow()
    }
    pub fn cancel(&self) {
        self.0.send_replace(true);
    }
    pub async fn cancelled(&self) {
        let mut receiver = self.0.subscribe();
        loop {
            if *receiver.borrow_and_update() {
                return;
            }
            if receiver.changed().await.is_err() {
                return;
            }
        }
    }
}
#[cfg(not(feature = "async"))]
impl Default for CancellationToken {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(all(test, not(feature = "async")))]
mod tests {
    use super::*;
    use std::{
        future::Future,
        sync::Arc,
        task::{Context, Poll, Wake, Waker},
    };
    struct Probe(std::sync::atomic::AtomicUsize);
    impl Wake for Probe {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
    #[test]
    fn dropping_a_cancel_wait_reclaims_its_executor_waker() {
        let token = CancellationToken::new();
        let probe = Arc::new(Probe(std::sync::atomic::AtomicUsize::new(0)));
        let weak = Arc::downgrade(&probe);
        let waker = Waker::from(probe.clone());
        let mut context = Context::from_waker(&waker);
        let mut wait = Box::pin(token.cancelled());
        assert!(matches!(wait.as_mut().poll(&mut context), Poll::Pending));
        drop(wait);
        drop(waker);
        assert_eq!(
            Arc::strong_count(&probe),
            1,
            "cancelled wait retained its waker"
        );
        token.cancel();
        assert_eq!(probe.0.load(std::sync::atomic::Ordering::SeqCst), 0);
        drop(probe);
        assert!(weak.upgrade().is_none());
        assert!(token.is_cancelled());
        crate::threads::block_on(token.cancelled());
    }
    #[test]
    fn cancellation_wakes_racing_native_waiters() {
        for _ in 0..100 {
            let token = CancellationToken::new();
            let stopping = token.clone();
            let worker = std::thread::spawn(move || stopping.cancel());
            crate::threads::block_on(token.cancelled());
            worker.join().unwrap();
        }
    }
}
