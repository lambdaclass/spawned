//! Tokio.rs reexports to prevent tokio dependencies within external code

pub use tokio::sync::{
    broadcast::{channel as broadcast_channel, Receiver as BroadcastReceiver},
    mpsc::{
        channel, error::SendError, unbounded_channel, Receiver, Sender, UnboundedReceiver,
        UnboundedSender,
    },
};
