//! Runtime wrapper to remove dependencies from code. Using this library will
//! allow to set a tokio runtime or any other runtime, once implemented just by
//! changing the enabled feature.
//! May implement the `deterministic` version based on comonware.xyz's runtime:
//! https://github.com/commonwarexyz/monorepo/blob/main/runtime/src/deterministic.rs
//!
//! Currently, only a very limited set of tokio functionality is reexported. We may want to
//! extend this functionality as needed.

mod tokio;

use ::tokio::runtime::Handle;

use crate::tracing::init_tracing;

pub use crate::tasks::tokio::mpsc;
pub use crate::tasks::tokio::oneshot;
pub use crate::tasks::tokio::sleep;
pub use crate::tasks::tokio::{spawn, spawn_blocking, JoinHandle, Runtime};
use std::future::Future;

pub fn run<F: Future>(future: F) -> F::Output {
    init_tracing();

    let rt = Runtime::new().unwrap();
    rt.block_on(future)
}

pub fn block_on<F: Future>(future: F) -> F::Output {
    Handle::current().block_on(future)
}
