//! Tokio.rs reexports to prevent tokio dependencies within external code
pub mod mpsc;
pub mod oneshot;
pub use tokio::sync::watch;

pub use tokio::task::{AbortHandle, JoinError, JoinSet};
pub use tokio::time::MissedTickBehavior;
pub use tokio::{
    runtime::{Handle, Runtime, RuntimeFlavor},
    task::{
        block_in_place, id as task_id, spawn, spawn_blocking, unconstrained, yield_now, JoinHandle,
    },
    time::{sleep, timeout, Sleep},
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

/// Completes at `deadline` (Tokio's `sleep_until`, taking the
/// `spawned_rt::time::Instant` that is virtual under simulation).
pub fn sleep_until(deadline: std::time::Instant) -> tokio::time::Sleep {
    tokio::time::sleep_until(deadline.into())
}

/// Tokio's `Interval`, with instants as `spawned_rt::time::Instant` so the
/// same code runs on virtual time under `--cfg spawned_sim`.
#[derive(Debug)]
pub struct Interval(tokio::time::Interval);

/// An [`Interval`] whose first tick completes immediately (Tokio's
/// `interval`).
pub fn interval(period: std::time::Duration) -> Interval {
    Interval(tokio::time::interval(period))
}

/// An [`Interval`] whose first tick completes at `start` (Tokio's
/// `interval_at`).
pub fn interval_at(start: std::time::Instant, period: std::time::Duration) -> Interval {
    Interval(tokio::time::interval_at(start.into(), period))
}

impl Interval {
    /// Completes at the next tick and returns its scheduled instant.
    pub async fn tick(&mut self) -> std::time::Instant {
        self.0.tick().await.into_std()
    }
    pub fn poll_tick(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::time::Instant> {
        self.0.poll_tick(cx).map(tokio::time::Instant::into_std)
    }
    pub fn reset(&mut self) {
        self.0.reset()
    }
    pub fn reset_immediately(&mut self) {
        self.0.reset_immediately()
    }
    pub fn reset_at(&mut self, deadline: std::time::Instant) {
        self.0.reset_at(deadline.into())
    }
    pub fn period(&self) -> std::time::Duration {
        self.0.period()
    }
    pub fn missed_tick_behavior(&self) -> MissedTickBehavior {
        self.0.missed_tick_behavior()
    }
    pub fn set_missed_tick_behavior(&mut self, behavior: MissedTickBehavior) {
        self.0.set_missed_tick_behavior(behavior)
    }
}
