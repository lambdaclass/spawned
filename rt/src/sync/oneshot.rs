//! Tokio.rs reexports to prevent tokio dependencies within external code

pub use crossbeam::{Receiver, Sender, crossbeam_channel::unbounded as channel};
