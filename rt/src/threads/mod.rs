//! IO-threads based module to support shared behavior with task based version.

pub mod mpsc;
pub mod oneshot;

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
