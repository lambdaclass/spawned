//! Tokio.rs reexports to prevent tokio dependencies within external code

pub use tokio::sync::oneshot::{Receiver, Sender, channel};
