//! Tokio.rs reexports to prevent tokio dependencies within external code

pub use tokio::{
    runtime::Runtime,
    sync::mpsc::{
        UnboundedReceiver as Receiver, UnboundedSender as Sender, unbounded_channel as channel,
    },
    task::{JoinHandle, spawn},
};
