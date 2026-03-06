//! non-async replacement for oneshot channels

pub use std::sync::mpsc::{channel, RecvTimeoutError, Receiver, Sender};
