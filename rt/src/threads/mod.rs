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

#[cfg(feature = "async")]
use crate::tasks::Runtime;
use crate::tracing::init_tracing;

/// Global list of Ctrl+C subscribers
static CTRL_C_SUBSCRIBERS: OnceLock<Mutex<Vec<std_mpsc::Sender<()>>>> = OnceLock::new();

/// Initialize tracing and run the given function.
pub fn run(f: fn()) {
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
    #[cfg(feature = "backend-experiments")]
    if std::env::var("SPAWNED_EXPERIMENT_RUNTIME").as_deref() == Ok("managed") {
        // Experiment-only bounded shared pool; process lifetime, no caller dependency.
        static POOL: OnceLock<Runtime> = OnceLock::new();
        return POOL
            .get_or_init(|| {
                tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                    .unwrap()
            })
            .block_on(future);
    }
    #[cfg(feature = "backend-experiments")]
    let rt = match std::env::var("SPAWNED_EXPERIMENT_RUNTIME").as_deref() {
        Ok("current") => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap(),
        Ok("one-worker") => tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap(),
        _ => isolated_runtime(),
    };
    #[cfg(not(feature = "backend-experiments"))]
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

/// A token that can be used to signal cancellation.
///
/// Supports registering callbacks via `on_cancel()` that fire when
/// the token is cancelled, enabling efficient waiting patterns.
#[derive(Clone)]
pub struct CancellationToken {
    is_cancelled: Arc<AtomicBool>,
    callbacks: Arc<Mutex<Vec<CancelCallback>>>,
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
            callbacks: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.is_cancelled.load(Ordering::SeqCst)
    }

    pub fn cancel(&self) {
        self.is_cancelled.store(true, Ordering::SeqCst);
        // Fire all registered callbacks
        let callbacks: Vec<_> = self
            .callbacks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
            .collect();
        for cb in callbacks {
            cb();
        }
    }

    /// Register a callback to be invoked when this token is cancelled.
    /// If already cancelled, the callback fires immediately.
    ///
    /// This method is thread-safe: the callback is guaranteed to fire exactly
    /// once, either immediately (if already cancelled) or when `cancel()` is called.
    pub fn on_cancel(&self, callback: CancelCallback) {
        // Hold the lock while checking is_cancelled to avoid a race with cancel().
        // cancel() sets the flag BEFORE acquiring the lock, so if we see
        // is_cancelled=false while holding the lock, cancel() hasn't drained
        // callbacks yet and will drain ours after we release the lock.
        let mut callbacks = self.callbacks.lock().unwrap_or_else(|e| e.into_inner());
        if self.is_cancelled() {
            drop(callbacks);
            callback();
        } else {
            callbacks.push(callback);
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
