//! non-async replacement for oneshot channels

pub use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
