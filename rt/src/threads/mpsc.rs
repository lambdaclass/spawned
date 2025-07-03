//! non-async replacement for mpsc channels

pub use std::sync::mpsc::{
    channel as unbounded_channel, sync_channel as channel, Receiver, SendError, Sender,
};
