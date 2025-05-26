//! Tokio.rs reexports to prevent tokio dependencies within external code

pub use std::sync::mpsc::{channel, Receiver, SendError, Sender};
