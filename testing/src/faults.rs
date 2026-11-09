//! Fault injection through public lifecycle APIs.
//!
//! [`exit`] follows Erlang's `exit(Pid, Reason)`: abnormal reasons are
//! delivered the way a dying linked peer delivers them, so an actor that traps
//! exits receives an [`Exit`](spawned_concurrency::Exit) message instead of
//! stopping (except for `Kill`, which cannot be trapped). Stopping is
//! cooperative: a running handler finishes first, then `stopped()` runs.
//!
//! Message-level faults (delay, discard, crash on a message, a reply slower
//! than the caller's timeout) are configured on [`Mock`](crate::tasks::Mock)
//! and [`Probe`](crate::tasks::Probe).
use spawned_concurrency::{message::Message, threads, ChildHandle, ExitReason};

/// Make `target` exit with `reason`.
///
/// - `Normal`: [`ChildHandle::stop`].
/// - `Shutdown`: [`ChildHandle::shutdown`].
/// - `Panic(msg)`: a short-lived linked actor panics with `msg`; the target
///   exits with `Panic("panic in handler: {msg}")`, as it would if a linked
///   peer crashed. The panic message is printed by the default panic hook.
/// - `Kill`: a short-lived linked actor is aborted; the target exits with
///   `Kill`. Needs the `async` feature (Tokio-free builds panic here).
///
/// Returns immediately; wait for the exit with `ChildHandle::wait_exit_async`,
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
    fuse.context().link(target);
    let _ = fuse.send(Detonate(msg));
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
    std::thread::spawn(move || {
        let runtime = spawned_rt::tasks::control_runtime().expect("kill fuse runtime");
        let fuse = runtime.block_on(async { tasks::ActorStart::start(Aborted) });
        fuse.context().link(&target);
        drop(runtime);
    })
    .join()
    .expect("kill fuse thread");
}

#[cfg(not(feature = "async"))]
fn kill_linked(_: &ChildHandle) {
    panic!("spawned-test: injecting ExitReason::Kill needs the `async` feature");
}
