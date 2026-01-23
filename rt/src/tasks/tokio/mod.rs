//! Tokio.rs reexports to prevent tokio dependencies within external code
pub mod mpsc;
pub mod oneshot;

pub use tokio::{
    runtime::Runtime,
    task::{id as task_id, spawn, spawn_blocking, JoinHandle},
    time::{sleep, timeout},
};
pub use tokio_stream::wrappers::{BroadcastStream, UnboundedReceiverStream as ReceiverStream};
pub use tokio_util::sync::CancellationToken;

/// Returns a future that completes when Ctrl+C is received.
///
/// This is a thin wrapper around `tokio::signal::ctrl_c()` that panics on error.
///
/// # Example
///
/// ```ignore
/// send_message_on(handle.clone(), rt::ctrl_c(), Msg::Shutdown);
/// ```
pub async fn ctrl_c() {
    tokio::signal::ctrl_c()
        .await
        .expect("Failed to listen for Ctrl+C");
}
