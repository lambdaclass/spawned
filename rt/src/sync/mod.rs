pub mod mpsc;
pub mod oneshot;

pub use std::thread::{
    sleep,
    spawn,
};

use crate::tracing::init_tracing;

pub fn run(f: fn()) -> () {
    init_tracing();

    f()
}