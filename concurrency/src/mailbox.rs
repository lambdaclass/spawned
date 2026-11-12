//! Receiver-owned draining on completion, cancellation and unwind.
use std::{
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Condvar, Mutex,
    },
};

/// An owner's sink for the user payloads its framework code drops (refused
/// or undelivered messages, job outcomes, scheduled emits): each drop is
/// contained, its first failure recorded. From the start of the owner's
/// exit until its death point has answered every caller, drops are held
/// instead ([`hold`](Self::hold), [`release`](Self::release)): no user
/// destructor runs before the last answer, on whatever thread drops it.
#[derive(Default)]
pub(crate) struct DropFailures {
    first: Mutex<Option<String>>,
    held: Mutex<Option<Vec<Box<dyn Send>>>>,
}
impl DropFailures {
    pub(crate) fn first(&self) -> Option<String> {
        self.first.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
    /// Record a cleanup failure; the first one is kept.
    pub(crate) fn record(&self, message: String) {
        self.first
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get_or_insert(message);
    }
    /// The owner's exit began: hold every later drop until `release`.
    pub(crate) fn hold(&self) {
        self.held
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get_or_insert_with(Vec::new);
    }
    /// Every caller was answered: drop what was held (contained), and drop
    /// at once from now on. Idempotent.
    pub(crate) fn release(&self) {
        let held = self.held.lock().unwrap_or_else(|p| p.into_inner()).take();
        for value in held.into_iter().flatten() {
            self.drop_contained(value);
        }
    }
    pub(crate) fn discard<T: Send + 'static>(&self, value: T) {
        let mut held = self.held.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(held) = held.as_mut() {
            // Moved, not dropped: no user code under the lock.
            held.push(Box::new(value));
            return;
        }
        drop(held);
        self.drop_contained(value);
    }
    /// Drop at once (contained): the mailbox's own items, destroyed after
    /// the death point's discharge.
    pub(crate) fn discard_now<T>(&self, value: T) {
        self.drop_contained(value);
    }
    fn drop_contained<T>(&self, value: T) {
        if let Err(payload) = catch_unwind(AssertUnwindSafe(|| drop(value))) {
            self.record(crate::error::panic_message(&*payload));
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
/// Also the death point's fences (reply and exit-signal admission).
pub(crate) use admission::{SendGate, SendPermit};
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

#[cfg(feature = "async")]
pub(crate) mod tasks {
    use super::*;
    pub(crate) use raw::SendError;
    use spawned_rt::tasks::mpsc as raw;

    // Sender clones live in the actor's shared handle state (one per actor,
    // not one per handle), so they carry the raw sender and failure sink
    // inline: no extra allocation per actor and no extra hop per send. Only
    // sender clones own this state; the receiver must not retain a sender
    // (otherwise dropping every sender could never close the channel).
    pub(crate) struct Sender<T> {
        inner: raw::Sender<T>,
        failures: Arc<DropFailures>,
    }
    impl<T> Clone for Sender<T> {
        fn clone(&self) -> Self {
            Self {
                inner: self.inner.clone(),
                failures: self.failures.clone(),
            }
        }
    }
    impl<T> Sender<T> {
        pub(crate) fn send(&self, value: T) -> Result<(), SendError<T>> {
            self.inner.send(value)
        }
        pub(crate) fn failures(&self) -> Arc<DropFailures> {
            self.failures.clone()
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
    impl<T> Receiver<T> {
        /// Close admission and take everything accepted, without dropping
        /// it: the death point answers queued requests before any payload
        /// destructor runs ([`discard_all`](Self::discard_all) then).
        pub(crate) fn drain(&mut self) -> Vec<T> {
            self.inner.close();
            let mut items = Vec::new();
            // Sends remain synchronous and unbounded. EOF waits only for raw
            // reservations to publish, never for this runtime thread to run work.
            // Suppress Tokio's coop deferral while the owning task is destroyed.
            block_on_drop(spawned_rt::tasks::unconstrained(async {
                while let Some(value) = self.inner.recv().await {
                    items.push(value);
                }
            }));
            items
        }
        /// Destroy drained items; a destructor panic is a cleanup failure.
        pub(crate) fn discard_all(&self, items: Vec<T>) {
            for item in items {
                self.failures.discard_now(item);
            }
        }
    }
    impl<T> Drop for Receiver<T> {
        fn drop(&mut self) {
            let items = self.drain();
            self.discard_all(items);
        }
    }
    pub(crate) fn channel<T>() -> (Sender<T>, Receiver<T>) {
        let (inner, receiver) = raw::channel();
        let failures = Arc::new(DropFailures::default());
        (
            Sender {
                inner,
                failures: failures.clone(),
            },
            Receiver {
                inner: receiver,
                failures,
            },
        )
    }
}
// Native sends reserve admission until publication, so closing the receiver
// can wait for in-flight sends and then drain everything that was accepted.
// Tokio already supplies a close-and-receive-through-EOF protocol, so async
// sends (`tasks` above) keep their original channel fast path. Neither mode
// runs user destructors with a send permit held.
pub(crate) mod threads {
    use super::*;
    pub(crate) use raw::SendError;
    use spawned_rt::threads::mpsc as raw;
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
    impl<T> Receiver<T> {
        /// Close admission (waiting for in-flight sends) and take everything
        /// accepted, without dropping it (see the async receiver).
        pub(crate) fn drain(&mut self) -> Vec<T> {
            self.admission.close();
            let mut items = Vec::new();
            while let Ok(value) = self.inner.try_recv() {
                items.push(value);
            }
            items
        }
        /// Destroy drained items; a destructor panic is a cleanup failure.
        pub(crate) fn discard_all(&self, items: Vec<T>) {
            for item in items {
                self.admission.failures.discard_now(item);
            }
        }
    }
    impl<T> Drop for Receiver<T> {
        fn drop(&mut self) {
            let items = self.drain();
            self.discard_all(items);
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
        crate::test_wait::bounded(
            concat!(
                module_path!(),
                "::accepted_sends_are_drained_even_when_senders_outlive_receiver"
            ),
            move || {
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
            },
        );
    }

    #[test]
    fn close_waits_for_publication_and_rejects_late_senders() {
        crate::test_wait::bounded(
            concat!(
                module_path!(),
                "::close_waits_for_publication_and_rejects_late_senders"
            ),
            move || {
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
            },
        );
    }

    #[cfg(feature = "async")]
    #[tokio::test]
    async fn compact_async_senders_close_after_their_last_owner() {
        crate::test_wait::within(
            concat!(
                module_path!(),
                "::compact_async_senders_close_after_their_last_owner"
            ),
            async move {
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
            },
        )
        .await;
    }

    #[cfg(feature = "async")]
    #[tokio::test]
    async fn unpolled_tracked_unwind_drains_before_releasing_lease() {
        crate::test_wait::within(
            concat!(
                module_path!(),
                "::unpolled_tracked_unwind_drains_before_releasing_lease"
            ),
            async move {
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
            },
        )
        .await;
    }
}
