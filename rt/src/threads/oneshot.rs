//! non-async replacement for oneshot channels

pub use crossbeam::{
    crossbeam_channel::unbounded as channel, crossbeam_channel::RecvTimeoutError, Receiver, Sender,
};
