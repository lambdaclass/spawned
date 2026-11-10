//! Receiver-owned draining on completion, cancellation and unwind.
use std::{
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Condvar, Mutex,
    },
};

#[derive(Default)]
pub(crate) struct DropFailures(Mutex<Option<String>>);
impl DropFailures {
    pub(crate) fn first(&self) -> Option<String> {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
    pub(crate) fn discard<T>(&self, value: T) {
        if let Err(payload) = catch_unwind(AssertUnwindSafe(|| drop(value))) {
            let message = crate::error::panic_message(&*payload);
            self.0
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .get_or_insert(message);
            dispose_panic(payload);
        }
    }
}

// Panic payloads may themselves have user-defined destructors. A second panic
// payload cannot safely be destroyed recursively without an unbounded cascade.
// Any non-zero-sized final payload intentionally retains its box allocation and
// owned storage; leak-sanitized tests use a zero-sized secondary payload instead.
pub(crate) fn dispose_panic(payload: Box<dyn std::any::Any + Send>) {
    if let Err(secondary) = catch_unwind(AssertUnwindSafe(|| drop(payload))) {
        std::mem::forget(secondary);
    }
}

#[path = "mailbox_admission.rs"]
mod admission;
use admission::SendGate;
#[derive(Default)]
struct Admission {
    gate: SendGate,
    failures: Arc<DropFailures>,
}
impl Admission {
    fn enter(&self) -> Option<admission::SendPermit<'_>> {
        self.gate.enter()
    }
    fn close(&self) {
        self.gate.close();
    }
}

// Native sends reserve admission until publication. Tokio already supplies a
// close-and-receive-through-EOF protocol, so async sends keep their original
// channel fast path. Neither mode runs user destructors with a send permit held.
macro_rules! channel {
    ($module:ident, $raw:path, $guard_sends:expr, $drain:expr) => {
        pub(crate) mod $module {
            use super::*;
            pub(crate) use raw::SendError;
            use $raw as raw;
            pub(crate) struct Sender<T> {
                pub(super) inner: raw::Sender<T>,
                pub(super) admission: Arc<Admission>,
            }
            impl<T> Clone for Sender<T> {
                fn clone(&self) -> Self {
                    Self {
                        inner: self.inner.clone(),
                        admission: self.admission.clone(),
                    }
                }
            }
            impl<T> Sender<T> {
                pub(crate) fn send(&self, value: T) -> Result<(), SendError<T>> {
                    if !$guard_sends {
                        return self.inner.send(value);
                    }
                    let Some(permit) = self.admission.enter() else {
                        return Err(SendError(value));
                    };
                    let result = self.inner.send(value);
                    drop(permit);
                    result
                }
                pub(crate) fn failures(&self) -> Arc<DropFailures> {
                    self.admission.failures.clone()
                }
            }
            pub(crate) struct Receiver<T> {
                inner: raw::Receiver<T>,
                admission: Arc<Admission>,
            }
            impl<T> std::ops::Deref for Receiver<T> {
                type Target = raw::Receiver<T>;
                fn deref(&self) -> &Self::Target {
                    &self.inner
                }
            }
            impl<T> std::ops::DerefMut for Receiver<T> {
                fn deref_mut(&mut self) -> &mut Self::Target {
                    &mut self.inner
                }
            }
            impl<T> Drop for Receiver<T> {
                fn drop(&mut self) {
                    if $guard_sends {
                        self.admission.close();
                    }
                    ($drain)(&mut self.inner, &self.admission.failures);
                }
            }
            pub(crate) fn channel<T>() -> (Sender<T>, Receiver<T>) {
                let (send, recv) = raw::channel();
                let admission = Arc::new(Admission::default());
                (
                    Sender {
                        inner: send,
                        admission: admission.clone(),
                    },
                    Receiver {
                        inner: recv,
                        admission,
                    },
                )
            }
        }
    };
}
#[cfg(feature = "async")]
pub(crate) mod tasks {
    use super::*;
    pub(crate) use raw::SendError;
    use spawned_rt::tasks::mpsc as raw;

    // Keep the public actor/context handle as small as the original raw sender.
    // Only sender clones own this state; the receiver must not retain a sender
    // (otherwise dropping every sender could never close the channel).
    struct SendState<T> {
        inner: raw::Sender<T>,
        failures: Arc<DropFailures>,
    }
    pub(crate) struct Sender<T>(Arc<SendState<T>>);
    impl<T> Clone for Sender<T> {
        fn clone(&self) -> Self {
            Self(self.0.clone())
        }
    }
    impl<T> Sender<T> {
        pub(crate) fn send(&self, value: T) -> Result<(), SendError<T>> {
            self.0.inner.send(value)
        }
        pub(crate) fn failures(&self) -> Arc<DropFailures> {
            self.0.failures.clone()
        }
    }
    pub(crate) struct Receiver<T> {
        inner: raw::Receiver<T>,
        failures: Arc<DropFailures>,
    }
    impl<T> std::ops::Deref for Receiver<T> {
        type Target = raw::Receiver<T>;
        fn deref(&self) -> &Self::Target {
            &self.inner
        }
    }
    impl<T> std::ops::DerefMut for Receiver<T> {
        fn deref_mut(&mut self) -> &mut Self::Target {
            &mut self.inner
        }
    }
    impl<T> Drop for Receiver<T> {
        fn drop(&mut self) {
            self.inner.close();
            // Sends remain synchronous and unbounded. EOF waits only for raw
            // reservations to publish, never for this runtime thread to run work.
            // Suppress Tokio's coop deferral while the owning task is destroyed.
            block_on_drop(spawned_rt::tasks::unconstrained(async {
                while let Some(value) = self.inner.recv().await {
                    self.failures.discard(value);
                }
            }));
        }
    }
    pub(crate) fn channel<T>() -> (Sender<T>, Receiver<T>) {
        let (inner, receiver) = raw::channel();
        let failures = Arc::new(DropFailures::default());
        (
            Sender(Arc::new(SendState {
                inner,
                failures: failures.clone(),
            })),
            Receiver {
                inner: receiver,
                failures,
            },
        )
    }
}
channel!(
    threads,
    spawned_rt::threads::mpsc,
    true,
    |rx: &mut raw::Receiver<_>, failures: &DropFailures| {
        while let Ok(value) = rx.try_recv() {
            failures.discard(value);
        }
    }
);

#[cfg(feature = "async")]
fn block_on_drop<F: std::future::Future>(future: F) -> F::Output {
    struct ThreadWake(std::thread::Thread);
    impl std::task::Wake for ThreadWake {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = std::task::Waker::from(Arc::new(ThreadWake(std::thread::current())));
    let mut context = std::task::Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        match future.as_mut().poll(&mut context) {
            std::task::Poll::Ready(value) => return value,
            std::task::Poll::Pending => std::thread::park(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn accepted_sends_are_drained_even_when_senders_outlive_receiver() {
        struct Item {
            dropped: Arc<AtomicUsize>,
            accepted: bool,
        }
        impl Drop for Item {
            fn drop(&mut self) {
                if self.accepted {
                    self.dropped.fetch_add(1, Ordering::SeqCst);
                }
            }
        }
        macro_rules! check {
            ($channel:path) => {
                for _ in 0..50 {
                    let (sender, receiver) = $channel();
                    let accepted = Arc::new(AtomicUsize::new(1));
                    let dropped = Arc::new(AtomicUsize::new(0));
                    assert!(sender
                        .send(Item {
                            dropped: dropped.clone(),
                            accepted: true
                        })
                        .is_ok());
                    let gate = Arc::new(std::sync::Barrier::new(3));
                    let publishers: Vec<_> = (0..2)
                        .map(|_| {
                            let sender = sender.clone();
                            let accepted = accepted.clone();
                            let dropped = dropped.clone();
                            let gate = gate.clone();
                            std::thread::spawn(move || {
                                gate.wait();
                                for _ in 0..64 {
                                    match sender.send(Item {
                                        dropped: dropped.clone(),
                                        accepted: true,
                                    }) {
                                        Ok(()) => {
                                            accepted.fetch_add(1, Ordering::SeqCst);
                                        }
                                        Err(mut rejected) => {
                                            rejected.0.accepted = false;
                                        }
                                    }
                                }
                            })
                        })
                        .collect();
                    gate.wait();
                    drop(receiver);
                    for publisher in publishers {
                        publisher.join().unwrap();
                    }
                    assert_eq!(
                        accepted.load(Ordering::SeqCst),
                        dropped.load(Ordering::SeqCst)
                    );
                    assert!(sender.failures().first().is_none());
                }
            };
        }
        check!(threads::channel);
        #[cfg(feature = "async")]
        check!(tasks::channel);
    }

    #[test]
    fn close_waits_for_publication_and_rejects_late_senders() {
        let (sender, receiver) = threads::channel();
        let held = sender.admission.enter().unwrap();
        let (done, completion) = mpsc::channel();
        let owner = std::thread::spawn(move || {
            drop(receiver);
            done.send(()).unwrap();
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !sender.admission.gate.is_closed() {
            assert!(std::time::Instant::now() < deadline, "close did not start");
            std::thread::yield_now();
        }
        assert!(sender.send(1).is_err());
        assert!(completion.try_recv().is_err());
        // Model a producer paused between reservation and raw publication.
        sender.inner.send(2).unwrap();
        drop(held);
        completion.recv_timeout(Duration::from_secs(5)).unwrap();
        owner.join().unwrap();
    }

    #[cfg(feature = "async")]
    #[tokio::test]
    async fn compact_async_senders_close_after_their_last_owner() {
        assert_eq!(
            std::mem::size_of::<tasks::Sender<()>>(),
            std::mem::size_of::<spawned_rt::tasks::mpsc::Sender<()>>()
        );
        let (send, mut receive) = tasks::channel();
        let other = send.clone();
        send.send(1).unwrap();
        drop(send);
        assert_eq!(receive.recv().await, Some(1));
        other.send(2).unwrap();
        drop(other);
        assert_eq!(receive.recv().await, Some(2));
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), receive.recv())
                .await
                .unwrap(),
            None
        );
    }

    #[cfg(feature = "async")]
    #[tokio::test]
    async fn unpolled_tracked_unwind_drains_before_releasing_lease() {
        struct State;
        impl Drop for State {
            fn drop(&mut self) {
                panic!("state destructor");
            }
        }
        struct Bomb(Arc<AtomicUsize>);
        impl Drop for Bomb {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
                panic!("queued destructor");
            }
        }
        let system = crate::system::ActorSystem::new().unwrap();
        let group = system.async_group("unpolled-tracked-drop", 1).unwrap();
        let (scope, lease) = group.reserve(None).unwrap();
        let (sender, receiver) = tasks::channel();
        let dropped = Arc::new(AtomicUsize::new(0));
        for _ in 0..2 {
            assert!(sender.send(Bomb(dropped.clone())).is_ok());
        }
        let payload = (State, receiver);
        let future = async move {
            drop(payload);
        };
        let tracked = crate::system::Tracked { future, lease };
        assert!(catch_unwind(AssertUnwindSafe(|| drop(tracked))).is_err());
        assert_eq!(dropped.load(Ordering::SeqCst), 2);
        assert_eq!(
            sender.failures().first().as_deref(),
            Some("queued destructor")
        );
        tokio::time::timeout(Duration::from_secs(5), scope.joined())
            .await
            .unwrap();
        assert!(system.shutdown(Duration::from_secs(5)).await.complete);
    }
}
