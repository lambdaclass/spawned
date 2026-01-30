//! IO-threads based module to support shared behavior with task based version.

pub mod mpsc;
pub mod oneshot;

use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc as std_mpsc, Arc, Mutex, OnceLock,
};
pub use std::{
    future::Future,
    thread::{sleep, spawn, JoinHandle},
};

use crate::{tasks::Runtime, tracing::init_tracing};

/// Global list of Ctrl+C subscribers
static CTRL_C_SUBSCRIBERS: OnceLock<Mutex<Vec<std_mpsc::Sender<()>>>> = OnceLock::new();

pub fn run(f: fn()) {
    init_tracing();

    f()
}

pub fn block_on<F: Future>(future: F) -> F::Output {
    let rt = Runtime::new().unwrap();
    rt.block_on(future)
}

/// Spawn blocking is the same as spawn for pure threaded usage.
pub fn spawn_blocking<F, R>(f: F) -> JoinHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    spawn(f)
}

#[derive(Clone, Debug, Default)]
pub struct CancellationToken {
    is_cancelled: Arc<AtomicBool>,
}

impl CancellationToken {
    pub fn new() -> Self {
        CancellationToken {
            is_cancelled: Arc::new(false.into()),
        }
    }

    pub fn is_cancelled(&mut self) -> bool {
        self.is_cancelled.fetch_and(false, Ordering::SeqCst)
    }

    pub fn cancel(&mut self) {
        self.is_cancelled.fetch_or(true, Ordering::SeqCst);
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
                let mut guard = subs
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                // Notify all subscribers and remove dead ones (where receiver was dropped)
                guard.retain(|tx| tx.send(()).is_ok());
            }
        })
        .expect("Error setting Ctrl+C handler");
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
