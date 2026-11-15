//! Fault injection through public lifecycle APIs.
//!
//! [`exit`] sends an exit signal with a chosen [`ExitReason`], exactly as
//! Erlang's `exit_signal(Pid, Reason)` (`ChildHandle::exit`). Stopping is
//! cooperative: a running handler finishes first.
//!
//! Message-level faults (delay, discard, crash on a message, a reply slower
//! than the caller's timeout) are configured on [`Mock`](crate::tasks::Mock)
//! and [`Probe`](crate::tasks::Probe).
use spawned_concurrency::{ChildHandle, ExitReason};

/// Send `target` an exit signal with `reason` from outside any actor
/// ([`ChildHandle::exit`], sender `ActorId::EXTERNAL`):
///
/// - `Kill`: untrappable; the target ends `Killed` ([`ChildHandle::kill`]).
/// - `Normal`: ignored by a non-trapping target; a trapping one receives an
///   [`Exit`](spawned_concurrency::Exit).
/// - any other reason: a non-trapping target exits with exactly that reason
///   after its current handler, without `stopped()`; a trapping one receives
///   an `Exit` carrying it.
///
/// Use [`ChildHandle::stop`] or [`ChildHandle::stop_with_reason`] for an
/// administrative stop that `trap_exit` cannot intercept and that runs
/// `stopped()`.
///
/// A target past its death point is left alone. Never waits and never
/// blocks, so it is safe to call from async code, including a
/// `current_thread` runtime. Wait for the exit with
/// `ChildHandle::wait_exit_async`, [`tasks::exit_within`](crate::tasks::exit_within)
/// or [`threads::exit_within`](crate::threads::exit_within).
pub fn exit(target: &ChildHandle, reason: ExitReason) {
    tracing::debug!(target = ?target.id(), %reason, "spawned-test injecting exit");
    target.exit(reason);
}
