//! Runtime wrapper to remove dependencies from code. Using this library will
//! allow to set a tokio runtime or any other runtime, once implemented just by
//! changing the enabled feature.
//! May implement the `deterministic` version based on comonware.xyz's runtime:
//! https://github.com/commonwarexyz/monorepo/blob/main/runtime/src/deterministic.rs
//!
//! Currently, only a very limited set of tokio functionality is reexported. We may want to
//! extend this functionality as needed.

mod tokio;

use crate::tracing::init_tracing;

pub use crate::tasks::tokio::mpsc;
pub use crate::tasks::tokio::oneshot;
pub use crate::tasks::tokio::sleep;
pub use crate::tasks::tokio::timeout;
pub use crate::tasks::tokio::watch;
pub use crate::tasks::tokio::CancellationToken;
pub use crate::tasks::tokio::{
    block_in_place, spawn, spawn_blocking, task_id, Handle, JoinHandle, Runtime, RuntimeFlavor,
};
pub use crate::tasks::tokio::{BroadcastStream, ReceiverStream};
use std::future::Future;

/// Create a tokio runtime, initialize tracing, and block on the given future.
pub fn run<F: Future>(future: F) -> F::Output {
    init_tracing();

    let rt = Runtime::new().unwrap();
    rt.block_on(future)
}

/// Block on a future using the current tokio runtime handle.
/// Panics if no tokio runtime is active.
pub fn block_on<F: Future>(future: F) -> F::Output {
    Handle::current().block_on(future)
}

/// Block on a future using the current tokio runtime handle.
/// Returns `None` if no tokio runtime is active.
pub fn try_block_on<F: Future>(future: F) -> Option<F::Output> {
    Handle::try_current().ok().map(|h| h.block_on(future))
}

pub use crate::tasks::tokio::ctrl_c;
