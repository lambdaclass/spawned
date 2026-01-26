//! IO-threads based module to support shared behavior with task based version.

pub mod mpsc;
pub mod oneshot;

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
pub use std::{
    future::Future,
    thread::{sleep, spawn, JoinHandle},
};

use crate::{tasks::Runtime, tracing::init_tracing};

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

type CancelCallback = Box<dyn FnOnce() + Send>;

/// A token that can be used to signal cancellation.
///
/// Supports registering callbacks via `on_cancel()` that fire when
/// the token is cancelled, enabling efficient waiting patterns.
#[derive(Clone, Default)]
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
    pub fn on_cancel(&self, callback: CancelCallback) {
        if self.is_cancelled() {
            callback();
        } else {
            self.callbacks
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(callback);
        }
    }
}

/// Returns a closure that blocks until Ctrl+C is received.
///
/// The signal handler is registered immediately when this function is called,
/// not when the returned closure is executed. This ensures no signals are missed
/// due to race conditions if Ctrl+C is pressed before the closure runs.
///
/// # Example
///
/// ```ignore
/// send_message_on(handle.clone(), rt::ctrl_c(), Msg::Shutdown);
/// ```
pub fn ctrl_c() -> impl FnOnce() + Send + 'static {
    let (tx, rx) = std::sync::mpsc::channel();
    ctrlc::set_handler(move || {
        let _ = tx.send(());
    })
    .expect("Error setting Ctrl+C handler");

    move || {
        let _ = rx.recv();
    }
}
