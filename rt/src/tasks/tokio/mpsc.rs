//! Tokio.rs reexports to prevent tokio dependencies within external code

pub use tokio::sync::mpsc::{
    channel, error::SendError, unbounded_channel, Receiver, Sender, UnboundedReceiver,
    UnboundedSender,
};
