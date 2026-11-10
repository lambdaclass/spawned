//! Runtime wrapper to remove dependencies from code. Using this library will
//! allow to set a tokio runtime or any other runtime, once implemented just by
//! changing the enabled feature.
//! May implement the `deterministic` version based on comonware.xyz's runtime:
//! <https://github.com/commonwarexyz/monorepo/blob/main/runtime/src/deterministic.rs>
//!
//! Currently, only a very limited set of tokio functionality is reexported. We may want to
//! extend this functionality as needed.

#[cfg_attr(spawned_sim, allow(unused_imports, dead_code))]
mod tokio;

/// Deterministic single-thread executor with virtual time, selected with
/// `RUSTFLAGS="--cfg spawned_sim --cfg tokio_unstable"`. See the module docs for its limits.
#[cfg(spawned_sim)]
pub mod sim;

// The simulator seeds Tokio's thread-local RNG (`select!` branch order,
// `watch` waiter shards) per run, which Tokio only allows under its own cfg.
#[cfg(all(spawned_sim, not(tokio_unstable)))]
compile_error!(
    "--cfg spawned_sim needs --cfg tokio_unstable too: \
     RUSTFLAGS=\"--cfg spawned_sim --cfg tokio_unstable\""
);

#[cfg(feature = "backend-experiments")]
mod dispatch_gate;

#[cfg(feature = "backend-experiments")]
#[doc(hidden)]
pub mod experimental;

use crate::tracing::init_tracing;

pub mod affinity;

#[cfg(spawned_sim)]
pub use crate::tasks::sim::{
    block_in_place, sleep, spawn, spawn_blocking, task_id, timeout, unconstrained, Handle,
    JoinHandle, Runtime, RuntimeFlavor,
};
pub use crate::tasks::tokio::mpsc;
pub use crate::tasks::tokio::oneshot;
#[cfg(not(spawned_sim))]
pub use crate::tasks::tokio::sleep;
#[cfg(not(spawned_sim))]
pub use crate::tasks::tokio::timeout;
pub use crate::tasks::tokio::watch;
pub use crate::tasks::tokio::CancellationToken;
#[cfg(not(spawned_sim))]
pub use crate::tasks::tokio::{
    block_in_place, spawn, spawn_blocking, task_id, unconstrained, Handle, JoinHandle, Runtime,
    RuntimeFlavor,
};
pub use crate::tasks::tokio::{BroadcastStream, ReceiverStream};
use std::future::Future;

/// Build an explicitly sized async runtime. Runtime owners must destroy it
/// outside an async executor thread.
#[cfg(not(spawned_sim))]
pub fn worker_runtime(name: &str, workers: usize) -> std::io::Result<Runtime> {
    ::tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .thread_name(name)
        .enable_all()
        .build()
}

/// Build a single-thread control runtime, driven by its owning native thread.
#[cfg(not(spawned_sim))]
pub fn control_runtime() -> std::io::Result<Runtime> {
    ::tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
}

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

#[cfg(spawned_sim)]
pub use crate::tasks::sim::ctrl_c;
#[cfg(not(spawned_sim))]
pub use crate::tasks::tokio::ctrl_c;

/// Under simulation every runtime is this thread's simulated executor.
#[cfg(spawned_sim)]
pub fn worker_runtime(_name: &str, _workers: usize) -> std::io::Result<Runtime> {
    Runtime::new()
}

/// Under simulation every runtime is this thread's simulated executor.
#[cfg(spawned_sim)]
pub fn control_runtime() -> std::io::Result<Runtime> {
    Runtime::new()
}
