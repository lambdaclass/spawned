//! Fault injection through public lifecycle APIs.
//!
//! [`exit`] makes an actor exit with a chosen [`ExitReason`]. It resembles
//! Erlang's `exit(Pid, Reason)` for `Panic`, delivered through a dying linked
//! peer. `Normal`, `Shutdown` and `Kill` are direct requests,
//! which always stop the target (see [`exit`]). Stopping is cooperative: a
//! running handler finishes first, then `stopped()` runs.
//!
//! Message-level faults (delay, discard, crash on a message, a reply slower
//! than the caller's timeout) are configured on [`Mock`](crate::tasks::Mock)
//! and [`Probe`](crate::tasks::Probe).
//!
//! Panic injection uses a short-lived linked actor. Kill uses the core's
//! direct `ChildHandle::stop_with_reason`, including in Tokio-free builds.
use spawned_concurrency::{message::Message, threads, ChildHandle, ExitReason};

/// Make `target` exit with `reason`.
///
/// - `Normal`: [`ChildHandle::stop`]. The target stops even if it traps
///   exits; this differs from Erlang's `exit(Pid, normal)`, which a
///   non-trapping process ignores and a trapping one receives as a message.
/// - `Shutdown`: [`ChildHandle::shutdown`]. Also a direct stop that trapping
///   cannot intercept.
/// - `Panic(msg)`: a short-lived linked actor panics with `msg`. A
///   non-trapping target exits with `Panic("panic in handler: {msg}")` (the
///   wording the core uses for a handler panic), not `Panic(msg)`; a target
///   that traps exits receives an [`Exit`](spawned_concurrency::Exit) carrying
///   that reason instead of stopping. The panic is real, so the default panic
///   hook prints `msg` to stderr. If the target already exited abnormally,
///   its link stops the fuse first; that is logged and `exit` returns (the
///   target is gone, with its own reason). Any other failure to deliver the
///   panic panics here rather than dropping the fault silently.
/// - `Kill`: direct `ChildHandle::stop_with_reason(Kill)`. The target stops
///   even when trapping exits; linked trapping peers receive an Exit notice.
///   Available in both runtime modes without constructing a private runtime.
///
/// A target that has already exited is left alone (logged at debug level). A
/// target that exits `Normal` or `Shutdown` concurrently with a `Panic`
/// injection does not propagate that exit over the link, so the fuse still
/// panics on its own and the default hook prints it; the target is unaffected.
///
/// Never waits for the target and never blocks: it returns once the exit is
/// requested, so it is safe to call from async code, including a
/// `current_thread` runtime. Wait for the exit with `ChildHandle::wait_exit_async`,
/// [`tasks::exit_within`](crate::tasks::exit_within) or
/// [`threads::exit_within`](crate::threads::exit_within).
pub fn exit(target: &ChildHandle, reason: ExitReason) {
    tracing::debug!(target = ?target.id(), %reason, "spawned-test injecting exit");
    match reason {
        ExitReason::Normal => target.stop(),
        ExitReason::Shutdown => target.shutdown(),
        ExitReason::Panic(msg) => panic_linked(target, msg),
        ExitReason::Kill => target.stop_with_reason(ExitReason::Kill),
    }
}

struct Fuse;
impl threads::Actor for Fuse {}
struct Detonate(String);
impl Message for Detonate {
    type Result = ();
}
impl threads::Handler<Detonate> for Fuse {
    fn handle(
        &mut self,
        msg: Detonate,
        _reply: spawned_concurrency::reply::PendingReply<'_, ()>,
        _: &threads::Context<Self>,
    ) {
        panic!("{}", msg.0);
    }
}

fn panic_linked(target: &ChildHandle, msg: String) {
    use threads::ActorStart as _;
    if already_exited(target) {
        return;
    }
    let fuse = Fuse.start();
    // Registers both link edges synchronously, or, if the target has already
    // exited abnormally, signals that exit to the fuse right away.
    fuse.context().link(target);
    if let Err(error) = fuse.send(Detonate(msg)) {
        // Nothing else knows the fuse, so only the target's link can have
        // stopped it: the target exited abnormally before the fault landed.
        // Read the outcome without waiting (the fuse's mailbox can close
        // before its exit reason is published), so async callers never block.
        match fuse.child_handle().exit_reason() {
            Some(ExitReason::Normal) => panic!(
                "spawned-test: could not inject a panic into {:?}: the fuse stopped \
                 ({error}) without a linked exit",
                target.id()
            ),
            reason => tracing::debug!(
                target = ?target.id(),
                fuse_reason = ?reason,
                "spawned-test: target exited before the injected panic; not needed"
            ),
        }
    }
}

/// `true` (and logged) if `target` already exited, so there is nothing to fault.
fn already_exited(target: &ChildHandle) -> bool {
    match target.exit_reason() {
        Some(reason) => {
            tracing::debug!(
                target = ?target.id(),
                %reason,
                "spawned-test: target already exited; fault not injected"
            );
            true
        }
        None => false,
    }
}
