//! Tokio.rs reexports to prevent tokio dependencies within external code

pub use tokio::sync::mpsc::{
    error::SendError, unbounded_channel as channel, UnboundedReceiver as Receiver,
    UnboundedSender as Sender,
};
