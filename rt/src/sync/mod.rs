pub mod mpsc;
pub mod oneshot;

pub use std::thread::{JoinHandle, sleep, spawn};

use crate::{r#async::Runtime, tracing::init_tracing};

pub fn run(f: fn()) {
    init_tracing();

    f()
}

pub fn block_on<F: Future>(future: F) -> F::Output {
    let rt = Runtime::new().unwrap();
    rt.block_on(future)
}