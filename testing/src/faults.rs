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
/// - `Kill`: a short-lived linked actor is aborted on a helper thread; the
///   target exits with `Kill`, which cannot be trapped. Needs the `async`
///   feature (Tokio-free builds panic here).
///
/// Never waits for the target: it returns once the exit is requested (for
/// `Kill`, once the helper thread is spawned), so it is safe to call from
/// async code. Wait for the exit with `ChildHandle::wait_exit_async`,
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
    let fuse = Fuse.start();
    // Registers both link edges synchronously, or, if the target has already
    // exited abnormally, signals that exit to the fuse right away.
    fuse.context().link(target);
    if let Err(error) = fuse.send(Detonate(msg)) {
        // Nothing else knows the fuse, so only the target's link can have
        // stopped it: the target already exited abnormally before the fault.
        // The fuse records that reason (its own exit is linked-reason driven).
        let reason = fuse.child_handle().wait_exit_blocking();
        match reason {
            ExitReason::Normal => panic!(
                "spawned-test: could not inject a panic into {:?}: the fuse stopped \
                 ({error}) without a linked exit",
                target.id()
            ),
            reason => tracing::debug!(
                target = ?target.id(),
                %reason,
                "spawned-test: target had already exited; panic injection not needed"
            ),
        }
    }
}

#[cfg(feature = "async")]
fn kill_linked(target: &ChildHandle) {
    use spawned_concurrency::tasks;
    struct Aborted;
    impl tasks::Actor for Aborted {}
    let target = target.clone();
    // Dropping a runtime that owns the linked actor's task drops the actor
    // future without completing it, which publishes `Kill` to its links. A
    // runtime cannot be dropped inside another, hence the dedicated thread.
    // It is not joined, so async callers never block a runtime worker.
    let spawned = std::thread::Builder::new()
        .name("spawned-test-kill".into())
        .spawn(move || {
            let runtime = match spawned_rt::tasks::control_runtime() {
                Ok(runtime) => runtime,
                Err(error) => {
                    tracing::error!(%error, "spawned-test: no runtime for the kill fuse");
                    return;
                }
            };
            let fuse = runtime.block_on(async { tasks::ActorStart::start(Aborted) });
            // `link` needs no task to run: it inserts both edges into the link
            // tables under their mutexes before returning (or signals the fuse
            // at once if the target already exited). The fuse's cleanup guard
            // is built before its task is spawned, so dropping the runtime
            // publishes `Kill` over that link even if the task never ran.
            fuse.context().link(&target);
            drop(runtime);
        });
    if let Err(error) = spawned {
        panic!("spawned-test: could not spawn the kill fuse thread: {error}");
    }
}

#[cfg(not(feature = "async"))]
fn kill_linked(_: &ChildHandle) {
    panic!("spawned-test: injecting ExitReason::Kill needs the `async` feature");
}
