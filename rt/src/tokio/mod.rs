//! Tokio.rs reexports to prevent tokio dependencies within external code
pub mod mpsc;
pub mod oneshot;

pub use tokio::{
    time::sleep,
    runtime::Runtime,
    task::{JoinHandle, spawn},
};
