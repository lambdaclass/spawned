//! Tokio.rs reexports to prevent tokio dependencies within external code

pub use tokio::sync::mpsc::{
    UnboundedReceiver as Receiver, UnboundedSender as Sender, error::SendError,
    unbounded_channel as channel,
};
