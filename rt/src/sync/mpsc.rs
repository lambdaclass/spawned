//! Tokio.rs reexports to prevent tokio dependencies within external code

pub use std::sync::mpsc::{Receiver, SendError, Sender, channel};
