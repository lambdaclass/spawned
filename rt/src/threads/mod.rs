//! IO-threads based module to support shared behavior with task based version.

pub mod mpsc;
pub mod oneshot;

// This cfg is only for cancellation models: instrumented tokens must be
// constructed inside loom::model, not in ordinary actor or signal tests.
#[cfg(spawned_loom)]
use loom::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
#[cfg(not(spawned_loom))]
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::sync::{mpsc as std_mpsc, OnceLock};
pub use std::{
    future::Future,
    thread::{sleep, spawn, JoinHandle},
};

#[cfg(all(feature = "async", not(spawned_sim)))]
use crate::tasks::Runtime;
// Native actors keep real Tokio under simulation: they run on OS threads.
#[cfg(feature = "fmt-subscriber")]
use crate::tracing::init_tracing;
#[cfg(all(feature = "async", spawned_sim))]
use tokio::runtime::Runtime;

/// Global list of Ctrl+C subscribers
static CTRL_C_SUBSCRIBERS: OnceLock<Mutex<Vec<std_mpsc::Sender<()>>>> = OnceLock::new();

/// Run the given function. With the `fmt-subscriber` feature it first
/// installs a default `tracing` subscriber.
pub fn run(f: fn()) {
    #[cfg(feature = "fmt-subscriber")]
    init_tracing();

    f()
}

/// Block on a future using an isolated Tokio runtime with one async worker.
///
/// The future is driven on the calling thread. Spawned async helpers use one
/// additional worker, so they can progress while that thread blocks. Tokio may
/// create further threads for `spawn_blocking`, filesystem or DNS operations.
/// The worker count is explicit and independent of `TOKIO_WORKER_THREADS`.
#[cfg(feature = "async")]
pub fn block_on<F: Future>(future: F) -> F::Output {
    let rt = isolated_runtime();
    rt.block_on(future)
}

// Keep a multithread runtime for block_in_place and independently scheduled
// helpers, but never allocate a machine-sized worker pool for each actor.
#[cfg(feature = "async")]
fn isolated_runtime() -> Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap()
}

/// Spawn blocking is the same as spawn for pure threaded usage.
pub fn spawn_blocking<F, R>(f: F) -> JoinHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    spawn(f)
}

type CancelCallback = Box<dyn FnOnce() + Send>;

/// Registered callbacks: a slab, so a [`CancelRegistration`] removes its own
/// entry in O(1). Each entry carries its registration's sequence number,
/// which tells it apart from a later entry reusing the slot and keeps the
/// firing order the registration order.
#[derive(Default)]
struct Callbacks {
    slots: Vec<Option<(u64, CancelCallback)>>,
    free: Vec<usize>,
    next: u64,
    live: usize,
}

impl Callbacks {
    fn insert(&mut self, callback: CancelCallback) -> (usize, u64) {
        let seq = self.next;
        self.next += 1;
        let entry = Some((seq, callback));
        let index = match self.free.pop() {
            Some(index) => {
                self.slots[index] = entry;
                index
            }
            None => {
                self.slots.push(entry);
                self.slots.len() - 1
            }
        };
        self.live += 1;
        (index, seq)
    }

    fn remove(&mut self, index: usize, seq: u64) -> Option<CancelCallback> {
        let slot = self.slots.get_mut(index)?;
        if !matches!(slot, Some((s, _)) if *s == seq) {
            return None;
        }
        let (_, callback) = slot.take()?;
        self.free.push(index);
        self.live -= 1;
        Some(callback)
    }

    /// Every registered callback, in registration order.
    fn drain(&mut self) -> Vec<CancelCallback> {
        let mut entries: Vec<_> = self.slots.drain(..).flatten().collect();
        self.free.clear();
        self.live = 0;
        entries.sort_unstable_by_key(|(seq, _)| *seq);
        entries.into_iter().map(|(_, callback)| callback).collect()
    }
}

/// A token that can be used to signal cancellation.
///
/// Supports registering callbacks that fire when the token is cancelled:
/// `on_cancel()` keeps its callback until then; `register_on_cancel()`
/// removes it when the returned registration is dropped.
#[derive(Clone)]
pub struct CancellationToken {
    is_cancelled: Arc<AtomicBool>,
    callbacks: Arc<Mutex<Callbacks>>,
}

impl std::fmt::Debug for CancellationToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CancellationToken")
            .field("is_cancelled", &self.is_cancelled())
            .finish()
    }
}

impl Default for CancellationToken {
    fn default() -> Self {
        Self::new()
    }
}

impl CancellationToken {
    pub fn new() -> Self {
        CancellationToken {
            is_cancelled: Arc::new(false.into()),
            callbacks: Arc::new(Mutex::new(Callbacks::default())),
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.is_cancelled.load(Ordering::SeqCst)
    }

    pub fn cancel(&self) {
        self.is_cancelled.store(true, Ordering::SeqCst);
        // Fire all registered callbacks
        let callbacks = self
            .callbacks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain();
        for cb in callbacks {
            cb();
        }
    }

    /// Register a callback to be invoked when this token is cancelled.
    /// If already cancelled, the callback fires immediately.
    ///
    /// This method is thread-safe: the callback is guaranteed to fire exactly
    /// once, either immediately (if already cancelled) or when `cancel()` is called.
    /// It is kept until then: when the callback's purpose can end first (a
    /// timer that fires), use [`register_on_cancel`](Self::register_on_cancel).
    pub fn on_cancel(&self, callback: CancelCallback) {
        if let Err(callback) = self.insert(callback) {
            callback();
        }
    }

    /// Like [`on_cancel`](Self::on_cancel), but dropping the returned
    /// registration removes the callback (O(1)) unless `cancel()` already
    /// took it, so a long-lived token can serve any number of short-lived
    /// waiters without retaining their callbacks.
    ///
    /// Once the drop returns the callback will not start; if `cancel()` took
    /// it first, it runs (or is running) on the cancelling thread.
    pub fn register_on_cancel(&self, callback: CancelCallback) -> CancelRegistration {
        match self.insert(callback) {
            Ok((index, seq)) => CancelRegistration {
                entry: Some((self.callbacks.clone(), index, seq)),
            },
            Err(callback) => {
                callback();
                CancelRegistration { entry: None }
            }
        }
    }

    /// Callbacks registered and not yet fired or unregistered.
    pub fn callbacks_registered(&self) -> usize {
        self.callbacks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .live
    }

    /// Store `callback` (its slot and sequence number), or hand it back when
    /// the token is already cancelled.
    fn insert(&self, callback: CancelCallback) -> Result<(usize, u64), CancelCallback> {
        // Hold the lock while checking is_cancelled to avoid a race with cancel().
        // cancel() sets the flag BEFORE acquiring the lock, so if we see
        // is_cancelled=false while holding the lock, cancel() hasn't drained
        // callbacks yet and will drain ours after we release the lock.
        let mut callbacks = self.callbacks.lock().unwrap_or_else(|e| e.into_inner());
        if self.is_cancelled() {
            return Err(callback);
        }
        Ok(callbacks.insert(callback))
    }
}

/// A callback registered with [`CancellationToken::register_on_cancel`].
/// Dropping it unregisters the callback unless `cancel()` already took it.
#[must_use = "dropping the registration unregisters the callback"]
pub struct CancelRegistration {
    entry: Option<(Arc<Mutex<Callbacks>>, usize, u64)>,
}

impl std::fmt::Debug for CancelRegistration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CancelRegistration")
            .field("registered", &self.entry.is_some())
            .finish()
    }
}

impl Drop for CancelRegistration {
    fn drop(&mut self) {
        if let Some((callbacks, index, seq)) = self.entry.take() {
            let removed = callbacks
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(index, seq);
            // The callback's captures are dropped outside the lock.
            drop(removed);
        }
    }
}

/// Returns a closure that blocks until Ctrl+C is received.
///
/// Multiple calls to this function are supported - each returns a closure that
/// will be notified when Ctrl+C is pressed. This allows multiple actors to
/// react to the same signal.
///
/// The signal handler is registered on the first call. Subsequent calls simply
/// add new subscribers to the broadcast list.
///
/// # Example
///
/// ```ignore
/// // Both actors will be notified on Ctrl+C
/// send_message_on(actor1.clone(), rt::ctrl_c(), Msg::Shutdown);
/// send_message_on(actor2.clone(), rt::ctrl_c(), Msg::Shutdown);
/// ```
pub fn ctrl_c() -> impl FnOnce() + Send + 'static {
    // Initialize subscribers list and register handler on first call
    let subscribers = CTRL_C_SUBSCRIBERS.get_or_init(|| {
        ctrlc::set_handler(|| {
            if let Some(subs) = CTRL_C_SUBSCRIBERS.get() {
                let mut guard = subs.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                // Notify all subscribers and remove dead ones (where receiver was dropped)
                guard.retain(|tx| tx.send(()).is_ok());
            }
        })
        .expect("Ctrl+C handler already set. Use ctrl_c() instead of ctrlc::set_handler()");
        Mutex::new(Vec::new())
    });

    // Create a new subscriber channel
    let (tx, rx) = std_mpsc::channel();
    subscribers
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(tx);

    move || {
        let _ = rx.recv();
    }
}

/// Park the caller while polling an executor-independent future. Thread-only
/// builds have no Tokio reactor, timer driver or implicit helper workers.
#[cfg(not(feature = "async"))]
pub fn block_on<F: Future>(future: F) -> F::Output {
    use std::task::{Context, Poll, Wake, Waker};
    struct Parker {
        notified: std::sync::Mutex<bool>,
        signal: std::sync::Condvar,
    }
    impl Parker {
        fn notify(&self) {
            *self.notified.lock().unwrap_or_else(|p| p.into_inner()) = true;
            self.signal.notify_one();
        }
        fn wait(&self) {
            let mut notified = self.notified.lock().unwrap_or_else(|p| p.into_inner());
            while !*notified {
                notified = self
                    .signal
                    .wait(notified)
                    .unwrap_or_else(|p| p.into_inner());
            }
            *notified = false;
        }
    }
    impl Wake for Parker {
        fn wake(self: std::sync::Arc<Self>) {
            self.notify();
        }
        fn wake_by_ref(self: &std::sync::Arc<Self>) {
            self.notify();
        }
    }
    // Each invocation owns its wake permit. Nested blocking waits must not
    // consume a wake intended for their caller's still-pending future.
    let parker = std::sync::Arc::new(Parker {
        notified: std::sync::Mutex::new(false),
        signal: std::sync::Condvar::new(),
    });
    let waker = Waker::from(parker.clone());
    let mut context = Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => parker.wait(),
        }
    }
}

#[cfg(all(test, not(feature = "async")))]
mod parking_tests {
    #[test]
    fn nested_block_on_does_not_consume_the_outer_futures_wakeup() {
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mut outer_polled = false;
            let value = super::block_on(std::future::poll_fn(|cx| {
                if outer_polled {
                    return std::task::Poll::Ready(42);
                }
                outer_polled = true;
                cx.waker().wake_by_ref();
                let mut inner_polled = false;
                super::block_on(std::future::poll_fn(|inner| {
                    if inner_polled {
                        std::task::Poll::Ready(())
                    } else {
                        inner_polled = true;
                        inner.waker().wake_by_ref();
                        std::task::Poll::Pending
                    }
                }));
                std::task::Poll::Pending
            }));
            tx.send(value).unwrap();
        });
        let result = rx.recv_timeout(std::time::Duration::from_secs(1));
        if result.is_err() {
            // Rescue the old shared-thread-token executor so a regression is a
            // finite assertion failure rather than a hanging test process.
            worker.thread().unpark();
            let _ = rx.recv_timeout(std::time::Duration::from_secs(1));
        }
        let value = result.expect("inner executor consumed the outer wake");
        worker.join().unwrap();
        assert_eq!(value, 42);
    }
}

#[cfg(all(test, not(spawned_loom)))]
mod cancel_registration_tests {
    use super::CancellationToken;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[test]
    fn dropped_registrations_are_removed_and_slots_reused() {
        let token = CancellationToken::new();
        let fired = Arc::new(AtomicUsize::new(0));
        for _ in 0..1000 {
            let f = fired.clone();
            let registration = token.register_on_cancel(Box::new(move || {
                f.fetch_add(1, Ordering::SeqCst);
            }));
            assert_eq!(token.callbacks_registered(), 1);
            drop(registration);
        }
        assert_eq!(token.callbacks_registered(), 0);
        assert_eq!(
            token.callbacks.lock().unwrap().slots.len(),
            1,
            "slot reused"
        );
        assert_eq!(Arc::strong_count(&fired), 1, "callbacks dropped");
        token.cancel();
        assert_eq!(fired.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn callbacks_fire_in_registration_order_across_reused_slots() {
        let token = CancellationToken::new();
        let order = Arc::new(std::sync::Mutex::new(Vec::new()));
        let push = |n| {
            let order = order.clone();
            Box::new(move || order.lock().unwrap().push(n)) as Box<dyn FnOnce() + Send>
        };
        let a = token.register_on_cancel(push(0));
        token.on_cancel(push(1));
        drop(a);
        let _c = token.register_on_cancel(push(2)); // reuses slot 0
        token.on_cancel(push(3));
        token.cancel();
        assert_eq!(*order.lock().unwrap(), vec![1, 2, 3]);
    }

    /// Unregistering from many threads while the token is cancelled: every
    /// callback runs at most once, every kept one exactly once, and nothing
    /// stays registered.
    #[test]
    fn unregister_racing_cancel_stress() {
        for _ in 0..20 {
            let token = CancellationToken::new();
            let workers: Vec<_> = (0..8)
                .map(|_| {
                    let token = token.clone();
                    std::thread::spawn(move || {
                        let mut kept = Vec::new();
                        let mut dropped = Vec::new();
                        for i in 0..2000 {
                            let count = Arc::new(AtomicUsize::new(0));
                            let c = count.clone();
                            let registration = token.register_on_cancel(Box::new(move || {
                                c.fetch_add(1, Ordering::SeqCst);
                            }));
                            if i % 50 == 0 {
                                kept.push((registration, count));
                            } else {
                                drop(registration);
                                // A callback `cancel()` already took may
                                // still run (and drop) on its thread.
                                assert!(count.load(Ordering::SeqCst) <= 1);
                                dropped.push(count);
                            }
                        }
                        (kept, dropped)
                    })
                })
                .collect();
            std::thread::yield_now();
            token.cancel();
            for worker in workers {
                let (kept, dropped) = worker.join().unwrap();
                for (registration, count) in kept {
                    assert_eq!(count.load(Ordering::SeqCst), 1, "kept callback");
                    drop(registration);
                }
                // `cancel()` has returned: every callback it took ran and was
                // dropped; every unregistered one was dropped by its drop.
                for count in dropped {
                    assert!(count.load(Ordering::SeqCst) <= 1);
                    assert_eq!(Arc::strong_count(&count), 1, "callback leaked");
                }
            }
            assert_eq!(token.callbacks_registered(), 0);
        }
    }
}
