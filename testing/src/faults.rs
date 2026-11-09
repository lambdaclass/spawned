//! Fault injection through public lifecycle APIs.
//!
//! [`exit`] makes an actor exit with a chosen [`ExitReason`]. It resembles
//! Erlang's `exit(Pid, Reason)` for `Panic` and `Kill`, which arrive the way a
//! dying linked peer delivers them, but **not** for `Normal` and `Shutdown`,
//! which always stop the target (see [`exit`]). Stopping is cooperative: a
//! running handler finishes first, then `stopped()` runs.
//!
//! Message-level faults (delay, discard, crash on a message, a reply slower
//! than the caller's timeout) are configured on [`Mock`](crate::tasks::Mock)
//! and [`Probe`](crate::tasks::Probe).
//!
//! The core has no `ChildHandle::exit(reason)` yet, so `Panic` and `Kill` go
//! through a short-lived linked actor, and the reason the target reports is
//! shaped by that link (see [`exit`]).
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
/// - `Kill`: a short-lived linked actor on a private runtime is dropped
///   unfinished, which publishes `Kill` over the link before `exit` returns;
///   the target exits with `Kill`, which cannot be trapped. Failing to set
///   this up panics here. Needs the `async` feature (Tokio-free builds panic
///   here).
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
        ExitReason::Kill => kill_linked(target),
    }
}

struct Fuse;
impl threads::Actor for Fuse {}
struct Detonate(String);
impl Message for Detonate {
    type Result = ();
}
impl threads::Handler<Detonate> for Fuse {
    fn handle(&mut self, msg: Detonate, _: &threads::Context<Self>) {
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

#[cfg(feature = "async")]
fn kill_linked(target: &ChildHandle) {
    use spawned_concurrency::tasks;
    struct Aborted;
    impl tasks::Actor for Aborted {}
    if already_exited(target) {
        return;
    }
    // A private current-thread runtime owns the fuse's task and is never
    // driven, so the fuse never runs. Shutting the runtime down drops the
    // actor future unfinished, and its cleanup publishes `Kill` to its links
    // on this thread. `shutdown_background` neither blocks nor panics inside
    // another runtime, unlike dropping it, so this works from async code.
    // Setup failures panic here instead of losing the fault.
    let runtime = spawned_rt::tasks::control_runtime()
        .unwrap_or_else(|error| panic!("spawned-test: no runtime for the kill fuse: {error}"));
    let fuse = {
        let _context = runtime.enter();
        tasks::ActorStart::start(Aborted)
    };
    // `link` inserts both edges under their mutexes before returning (or
    // signals the fuse at once if the target exited meanwhile).
    fuse.context().link(target);
    runtime.shutdown_background();
}

#[cfg(not(feature = "async"))]
fn kill_linked(_: &ChildHandle) {
    panic!("spawned-test: injecting ExitReason::Kill needs the `async` feature");
}
