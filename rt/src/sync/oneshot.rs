//! Tokio.rs reexports to prevent tokio dependencies within external code

pub use crossbeam::{crossbeam_channel::unbounded as channel, Receiver, Sender};
