//! IO-threads based module to support shared behavior with task based version.

pub mod mpsc;
pub mod oneshot;

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
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

#[derive(Clone, Default)]
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
