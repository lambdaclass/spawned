//! Tokio.rs reexports to prevent tokio dependencies within external code
pub mod mpsc;
pub mod oneshot;

pub use tokio::{
    runtime::Runtime,
    task::{JoinHandle, spawn},
};
