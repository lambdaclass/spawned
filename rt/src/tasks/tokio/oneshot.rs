//! Tokio.rs reexports to prevent tokio dependencies within external code

pub use tokio::sync::oneshot::{channel, Receiver, Sender};
