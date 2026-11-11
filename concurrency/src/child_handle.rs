use crate::error::ExitReason;
use crate::link::{LinkTable, Origin, SignalTarget};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// ActorId
// ---------------------------------------------------------------------------

static NEXT_ACTOR_ID: AtomicU64 = AtomicU64::new(1);

/// Unique identity for an actor instance. Used by monitors and links to
/// identify actors without needing the concrete actor type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ActorId(u64);

impl ActorId {
    /// The sender of an exit signal sent from outside any actor
    /// ([`ChildHandle::exit`]). Never an actor's identity, never a parent.
    pub const EXTERNAL: ActorId = ActorId(0);

    #[cfg_attr(not(feature = "lifecycle-trace"), allow(dead_code))]
    pub(crate) fn raw(self) -> u64 {
        self.0
    }
    pub(crate) fn next() -> Self {
        Self(NEXT_ACTOR_ID.fetch_add(1, Ordering::Relaxed))
    }
}

impl std::fmt::Display for ActorId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

// ---------------------------------------------------------------------------
// Completion — abstraction over tasks/threads completion signals
// ---------------------------------------------------------------------------

/// Type-erased completion signal. Wraps the mode-specific completion mechanism
/// so `ChildHandle` works uniformly across tasks and threads.
#[derive(Clone)]
pub(crate) enum Completion {
    /// Tasks mode: watch channel carrying Option<ExitReason>.
    #[cfg(feature = "async")]
    Tasks(spawned_rt::sync::watch::Receiver<Option<ExitReason>>),
    /// Threads mode: watch channel, also waitable without an async runtime.
    Threads(spawned_rt::sync::watch::Receiver<Option<ExitReason>>),
}

// ---------------------------------------------------------------------------
// ChildHandle
// ---------------------------------------------------------------------------

/// Type-erased cancel function. Wraps the mode-specific cancellation token.
pub(crate) type CancelFn = Arc<dyn Fn() + Send + Sync>;

/// Type-erased handle to a running actor. Provides lifecycle operations
/// (administrative stops, exit signals, liveness check, exit reason) without
/// knowing the actor's concrete type.
///
/// Obtained via `ChildHandle::from(actor_ref)`.
///
/// Unlike `ActorRef<A>`, a `ChildHandle` cannot send messages directly — it
/// only provides supervision-related operations (stop, exit, kill, wait,
/// check liveness, link/monitor).
#[derive(Clone)]
pub struct ChildHandle {
    target: Arc<SignalTarget>,
    cancel: CancelFn,
    completion: Completion,
    links: LinkTable,
    mailbox_failures: Option<Arc<crate::mailbox::DropFailures>>,
}

impl std::fmt::Debug for ChildHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChildHandle")
            .field("id", &self.target.id)
            .finish_non_exhaustive()
    }
}

impl ChildHandle {
    /// Create a ChildHandle from an actor's signal target, completion channel
    /// and exit table.
    pub(crate) fn new(target: Arc<SignalTarget>, completion: Completion, links: LinkTable) -> Self {
        let stop = target.clone();
        Self {
            cancel: Arc::new(move || crate::link::normal_stop(&stop.request, || (stop.wake)())),
            target,
            completion,
            links,
            mailbox_failures: None,
        }
    }
    pub(crate) fn with_mailbox_failures(
        mut self,
        failures: Arc<crate::mailbox::DropFailures>,
    ) -> Self {
        self.mailbox_failures = Some(failures);
        self
    }

    /// First cleanup failure after the death point (a panicking state or
    /// queued-message destructor), recorded independently of the final exit
    /// reason, which it never changes. Complete after publication. A
    /// supervisor restarts a Transient child that has one.
    pub fn cleanup_failure(&self) -> Option<String> {
        self.mailbox_failures.as_ref().and_then(|f| f.first())
    }

    /// Deprecated alias of [`cleanup_failure`](Self::cleanup_failure).
    #[deprecated(since = "0.8.0", note = "use `cleanup_failure()`")]
    pub fn mailbox_cleanup_failure(&self) -> Option<String> {
        self.cleanup_failure()
    }

    /// Crate-internal accessors used by `ctx.link()` and `ctx.monitor()`.
    pub(crate) fn signal_target(&self) -> &Arc<SignalTarget> {
        &self.target
    }
    pub(crate) fn links(&self) -> &LinkTable {
        &self.links
    }
    /// Past the death point D: links, monitors, sends and requests report
    /// `NoProc` from now on.
    pub(crate) fn is_past_death(&self) -> bool {
        self.links
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_dead()
    }

    // Each child_handle() creates this allocation; its clones share it.
    #[cfg(test)]
    pub(crate) fn lifetime_probe(&self) -> std::sync::Weak<dyn Fn() + Send + Sync> {
        Arc::downgrade(&self.cancel)
    }

    /// The actor's unique identity.
    pub fn id(&self) -> ActorId {
        self.target.id
    }

    /// Administrative stop (Spawned's `gen_server:stop`): the actor finishes
    /// its current handler, discards its queue, runs `stopped()` and exits
    /// `Normal`. Untrappable; returns at once (join with `wait_exit_*`).
    pub fn stop(&self) {
        (self.cancel)();
    }

    /// Administrative stop with a chosen reason, as [`stop`](Self::stop).
    /// Not an exit signal: `trap_exit` cannot intercept it and `stopped()`
    /// runs. `stop_with_reason(Kill)` ends `Kill`; use [`kill`](Self::kill)
    /// for the untrappable kill signal that ends `Killed`.
    pub fn stop_with_reason(&self, reason: ExitReason) {
        crate::link::stop_with_reason(&self.target.request, reason, || (self.target.wake)());
    }

    /// Administrative stop with reason `Shutdown`, as [`stop`](Self::stop).
    /// The first requested reason wins over later stops.
    pub fn shutdown(&self) {
        self.stop_with_reason(ExitReason::Shutdown);
    }

    /// Send an exit signal from outside any actor (OTP `exit_signal/2`, the
    /// sender is [`ActorId::EXTERNAL`]):
    ///
    /// - `Kill` is untrappable: the actor ends `Killed` (see [`kill`](Self::kill));
    /// - `Normal` is ignored by a non-trapping actor;
    /// - any other reason ends a non-trapping actor with that reason after
    ///   its current handler, **without** `stopped()`, discarding its queue;
    /// - a trapping actor gets `Exit { from: EXTERNAL, reason }` queued
    ///   behind its messages.
    ///
    /// A signal to an actor past its death point is dropped.
    pub fn exit(&self, reason: ExitReason) {
        self.exit_from(ActorId::EXTERNAL, reason, false);
    }

    /// Untrappable kill signal (`exit(Kill)`): the actor ends `Killed`, also
    /// when it is already exiting (until its death point), and nothing
    /// replaces that reason. The current handler, and a `stopped()` that
    /// already began, still finish (Rust cannot preempt them); a `stopped()`
    /// that has not begun is skipped. Links receive `Killed` (trappable),
    /// monitors `Down { reason: Killed }`.
    pub fn kill(&self) {
        self.exit(ExitReason::Kill);
    }

    /// Exit signal with an explicit sender; `from_parent` marks the internal
    /// entry point of supervisors and systems (E3).
    pub(crate) fn exit_from(&self, from: ActorId, reason: ExitReason, from_parent: bool) {
        crate::link::deliver_signal(&self.target, from, reason, Origin::Direct, from_parent);
    }

    /// Returns `true` if the actor is still running.
    pub fn is_alive(&self) -> bool {
        self.exit_reason().is_none()
    }

    /// Poll the exit reason. Returns `None` until the exit is published.
    /// A completion channel closed without a reason reports
    /// [`ExitReason::Killed`], matching the wait methods (for example, after
    /// runtime shutdown).
    pub fn exit_reason(&self) -> Option<ExitReason> {
        match &self.completion {
            #[cfg(feature = "async")]
            Completion::Tasks(rx) => poll_exit_reason(rx),
            Completion::Threads(rx) => poll_exit_reason(rx),
        }
    }

    /// Block the calling thread until the actor stops and return the exit reason.
    ///
    /// Safe contexts:
    /// - Sync code with no active tokio runtime
    /// - Multi-thread tokio runtime (uses `block_in_place`)
    /// - Threads-mode actors (waits without a Tokio runtime)
    ///
    /// An already-exited actor returns immediately from any context.
    /// **Panics** for a still-running actor if called from within a non-multi-thread tokio runtime (e.g.,
    /// current-thread) on a tasks-mode handle: blocking the only runtime thread
    /// would prevent the actor task from making progress (deadlock). Use
    /// [`wait_exit_async`] from async context instead.
    ///
    /// [`wait_exit_async`]: ChildHandle::wait_exit_async
    pub fn wait_exit_blocking(&self) -> ExitReason {
        match &self.completion {
            #[cfg(feature = "async")]
            Completion::Tasks(rx) => wait_for_tasks_exit_blocking(rx.clone()),
            Completion::Threads(rx) => wait_for_thread_exit_blocking(rx.clone()),
        }
    }

    /// Async wait until the actor stops and return the exit reason.
    ///
    /// Works with both execution modes without occupying a blocking worker.
    /// Dropping this future releases its subscription immediately.
    pub async fn wait_exit_async(&self) -> ExitReason {
        match &self.completion {
            #[cfg(feature = "async")]
            Completion::Tasks(rx) => wait_loop(rx.clone()).await,
            Completion::Threads(rx) => wait_loop(rx.clone()).await,
        }
    }
}

/// A closed completion sender without a published reason means abnormal termination.
/// Check closure before borrowing: a sender could publish its final reason and
/// close between an earlier None borrow and a later closure check. The final
/// published reason must win over the synthetic Kill.
pub(crate) fn poll_exit_reason(
    rx: &spawned_rt::sync::watch::Receiver<Option<ExitReason>>,
) -> Option<ExitReason> {
    let closed = rx.has_changed().is_err();
    rx.borrow()
        .clone()
        .or_else(|| closed.then_some(ExitReason::Killed))
}

/// Async loop that polls a watch channel until it carries an exit reason.
/// Returns `ExitReason::Killed` if the watch sender is dropped without a reason.
pub(crate) async fn wait_loop(
    mut rx: spawned_rt::sync::watch::Receiver<Option<ExitReason>>,
) -> ExitReason {
    loop {
        if let Some(reason) = rx.borrow_and_update().clone() {
            return reason;
        }
        if rx.changed().await.is_err() {
            return ExitReason::Killed;
        }
    }
}

/// Poll a watch subscription with a thread waker, without entering an executor.
/// This preserves synchronous waits even inside an existing futures executor.
pub(crate) fn wait_for_thread_exit_blocking(
    rx: spawned_rt::sync::watch::Receiver<Option<ExitReason>>,
) -> ExitReason {
    use std::future::Future;
    use std::task::{Context, Poll, Wake, Waker};
    struct ThreadWake(std::thread::Thread);
    impl Wake for ThreadWake {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = Waker::from(Arc::new(ThreadWake(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut wait = std::pin::pin!(wait_loop(rx));
    loop {
        match wait.as_mut().poll(&mut cx) {
            Poll::Ready(reason) => return reason,
            Poll::Pending => std::thread::park(),
        }
    }
}

/// Blocking wait on a tasks-mode watch channel.
///
/// - From a sync context (no tokio runtime): creates a temporary runtime.
/// - From a multi-thread tokio runtime: uses `block_in_place` + `Handle::block_on`.
/// - From any other tokio runtime flavor (e.g., current-thread): **panics**,
///   because blocking the only runtime thread prevents the actor task from
///   making progress (deadlock). Use `wait_exit_async` from async context, or
///   run on a multi-thread runtime.
///
/// Returns `ExitReason::Killed` if the watch sender is dropped without setting a
/// reason — this means the actor task was aborted externally (e.g., runtime
/// shutdown) without going through the normal exit path.
#[cfg(feature = "async")]
fn wait_for_tasks_exit_blocking(
    rx: spawned_rt::sync::watch::Receiver<Option<ExitReason>>,
) -> ExitReason {
    // Fast path: already done — works from any context, no runtime needed
    if let Some(reason) = poll_exit_reason(&rx) {
        return reason;
    }

    let wait = wait_loop(rx);

    match spawned_rt::tasks::Handle::try_current() {
        // No active runtime — create a temporary one
        Err(_) => spawned_rt::threads::block_on(wait),
        // Inside a tokio runtime — check the flavor
        Ok(handle) => match handle.runtime_flavor() {
            spawned_rt::tasks::RuntimeFlavor::MultiThread => {
                spawned_rt::tasks::block_in_place(|| handle.block_on(wait))
            }
            // Any other flavor (current-thread, MultiThreadAlt, future variants):
            // blocking here would deadlock the actor task. RuntimeFlavor is
            // #[non_exhaustive] so we conservatively reject anything other than
            // the explicitly-tested MultiThread variant.
            other => panic!(
                "ChildHandle::wait_exit_blocking() cannot be called from within a \
                non-multi-thread tokio runtime ({other:?}); doing so would deadlock \
                the actor task that this call is waiting for. Use wait_exit_async() \
                from async context instead, or run on a multi-thread runtime."
            ),
        },
    }
}

impl PartialEq for ChildHandle {
    fn eq(&self, other: &Self) -> bool {
        self.target.id == other.target.id
    }
}

impl Eq for ChildHandle {}

impl std::hash::Hash for ChildHandle {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.target.id.hash(state);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::link::{new_link_table, new_linked_exit_reason, new_trap_exit_flag};

    /// A signal target whose wake cancels `token`.
    pub(crate) fn test_target(
        id: ActorId,
        token: spawned_rt::threads::CancellationToken,
    ) -> Arc<SignalTarget> {
        Arc::new(SignalTarget {
            id,
            parent: None,
            trap: new_trap_exit_flag(),
            request: new_linked_exit_reason(),
            wake: Arc::new(move || token.cancel()),
            send_exit: Arc::new(|_| Ok(())),
            send_parent_exit: Arc::new(|_| Ok(())),
            send_held: Arc::new(|_, _| Ok(())),
        })
    }

    // Shared tasks-mode fixture: an Idler actor that does nothing and never
    // stops on its own. Used by multiple tests below.
    #[cfg(feature = "async")]
    mod tasks_fixtures {
        use crate::tasks::actor::Actor;
        pub struct Idler;
        impl Actor for Idler {}
    }

    /// Build a threads-mode ChildHandle for unit tests that exercise only the
    /// ChildHandle surface (no real actor).
    fn test_handle(
        token: spawned_rt::threads::CancellationToken,
        completion: spawned_rt::sync::watch::Receiver<Option<ExitReason>>,
    ) -> ChildHandle {
        ChildHandle::new(
            test_target(ActorId::next(), token),
            Completion::Threads(completion),
            new_link_table(),
        )
    }

    #[test]
    fn actor_id_is_unique() {
        let a = ActorId::next();
        let b = ActorId::next();
        assert_ne!(a, b);
    }

    #[test]
    fn actor_id_display() {
        let id = ActorId::next();
        let s = format!("{id}");
        assert_eq!(s.parse::<u64>().unwrap(), id.0);
        assert_eq!(format!("{id:?}"), format!("ActorId({})", id.0));
    }

    #[test]
    fn child_handle_from_threads_basics() {
        let (completion_tx, completion) = spawned_rt::sync::watch::channel(None);
        let token = spawned_rt::threads::CancellationToken::new();
        let handle = test_handle(token, completion.clone());

        assert!(handle.is_alive());
        assert!(handle.exit_reason().is_none());

        // Simulate actor completion
        completion_tx.send_replace(Some(ExitReason::Normal));

        assert!(!handle.is_alive());
        assert_eq!(handle.exit_reason(), Some(ExitReason::Normal));
    }

    #[test]
    fn child_handle_stop() {
        let (_completion_tx, completion) = spawned_rt::sync::watch::channel(None);
        let token = spawned_rt::threads::CancellationToken::new();
        assert!(!token.is_cancelled());
        let handle = test_handle(token.clone(), completion);
        handle.stop();
        assert!(token.is_cancelled());
    }

    #[test]
    fn child_handle_equality_by_id() {
        let (_completion_tx, completion) = spawned_rt::sync::watch::channel(None);
        let token = spawned_rt::threads::CancellationToken::new();
        let id = ActorId::next();
        let h1 = ChildHandle::new(
            test_target(id, token.clone()),
            Completion::Threads(completion.clone()),
            new_link_table(),
        );
        let h2 = ChildHandle::new(
            test_target(id, token),
            Completion::Threads(completion),
            new_link_table(),
        );
        assert_eq!(h1, h2);
    }

    // --- Integration tests using real actors ---

    #[test]
    fn child_handle_from_threads_actor_ref() {
        use crate::message::Message;
        use crate::threads::actor::{Actor, ActorStart, Context, Handler};

        struct Worker;
        struct Stop;
        impl Message for Stop {
            type Result = ();
        }
        impl Actor for Worker {}
        impl Handler<Stop> for Worker {
            fn handle(
                &mut self,
                _msg: Stop,
                reply: crate::reply::PendingReply<'_, ()>,
                ctx: &Context<Self>,
            ) {
                ctx.stop();
                reply.reply(());
            }
        }

        let actor = Worker.start();
        let handle = actor.child_handle();

        assert!(handle.is_alive());
        assert!(handle.exit_reason().is_none());
        assert_eq!(handle.id(), actor.id());

        actor.send(Stop).unwrap();
        let reason = handle.wait_exit_blocking();
        assert_eq!(reason, ExitReason::Normal);
        assert!(!handle.is_alive());
    }

    #[test]
    #[cfg(feature = "async")]
    fn child_handle_from_tasks_actor_ref() {
        use crate::message::Message;
        use crate::tasks::actor::{Actor, ActorStart, Context, Handler};

        struct Worker;
        struct Stop;
        impl Message for Stop {
            type Result = ();
        }
        impl Actor for Worker {}
        impl Handler<Stop> for Worker {
            async fn handle(
                &mut self,
                _msg: Stop,
                reply: crate::reply::PendingReply<'_, ()>,
                ctx: &Context<Self>,
            ) {
                ctx.stop();
                reply.reply(());
            }
        }

        let runtime = spawned_rt::tasks::Runtime::new().unwrap();
        runtime.block_on(async {
            let actor = Worker.start();
            let handle = actor.child_handle();

            assert!(handle.is_alive());
            assert!(handle.exit_reason().is_none());
            assert_eq!(handle.id(), actor.id());

            actor.send(Stop).unwrap();
            let reason = handle.wait_exit_async().await;
            assert_eq!(reason, ExitReason::Normal);
            assert!(!handle.is_alive());
        });
    }

    #[test]
    #[cfg(feature = "async")]
    fn child_handle_stop_from_tasks_actor() {
        use crate::tasks::actor::ActorStart;
        use tasks_fixtures::Idler;

        let runtime = spawned_rt::tasks::Runtime::new().unwrap();
        runtime.block_on(async {
            let actor = Idler.start();
            let handle = actor.child_handle();

            assert!(handle.is_alive());
            handle.stop();
            let reason = handle.wait_exit_async().await;
            assert_eq!(reason, ExitReason::Normal);
        });
    }

    #[test]
    #[cfg(feature = "async")]
    fn child_handle_wait_blocking_inside_multithread_runtime() {
        use crate::tasks::actor::ActorStart;
        use tasks_fixtures::Idler;

        // Multi-thread runtime — wait_exit_blocking should work via block_in_place
        let runtime = spawned_rt::tasks::Runtime::new().unwrap();
        runtime.block_on(async {
            let actor = Idler.start();
            let handle = actor.child_handle();
            handle.stop();

            let reason = handle.wait_exit_blocking();
            assert_eq!(reason, ExitReason::Normal);
        });
    }

    #[test]
    #[should_panic(expected = "non-multi-thread tokio runtime")]
    #[cfg(feature = "async")]
    fn child_handle_wait_blocking_panics_on_current_thread_runtime() {
        use crate::tasks::actor::ActorStart;
        use tasks_fixtures::Idler;

        let runtime = ::tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let actor = Idler.start();
            let handle = actor.child_handle();
            // Calling wait_exit_blocking from within a current-thread runtime
            // would deadlock the actor task — we panic with a clear message instead.
            let _ = handle.wait_exit_blocking();
        });
    }

    #[test]
    #[cfg(feature = "async")]
    fn child_handle_wait_blocking_fast_path_on_current_thread_runtime() {
        use crate::tasks::actor::ActorStart;
        use tasks_fixtures::Idler;

        let runtime = ::tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        // Fast path: if the actor has already exited, wait_exit_blocking is safe
        // even on a current-thread runtime (no actual blocking happens).
        runtime.block_on(async {
            let actor = Idler.start();
            let handle = actor.child_handle();
            handle.stop();
            // Wait async first so the actor actually exits
            let _ = handle.wait_exit_async().await;
            // Now wait_exit_blocking takes the fast path
            let reason = handle.wait_exit_blocking();
            assert_eq!(reason, ExitReason::Normal);
        });
    }

    // --- Panic observation tests ---

    #[test]
    fn child_handle_observes_panic_in_threads_actor() {
        use crate::message::Message;
        use crate::threads::actor::{Actor, ActorStart, Context, Handler};

        struct Boomer;
        struct Boom;
        impl Message for Boom {
            type Result = ();
        }
        impl Actor for Boomer {}
        impl Handler<Boom> for Boomer {
            fn handle(
                &mut self,
                _msg: Boom,
                _reply: crate::reply::PendingReply<'_, ()>,
                _ctx: &Context<Self>,
            ) {
                {
                    panic!("intentional panic in handler");
                }
            }
        }

        let actor = Boomer.start();
        let handle = actor.child_handle();
        let _ = actor.send(Boom);

        let reason = handle.wait_exit_blocking();
        match reason {
            ExitReason::Panic(msg) => {
                assert!(msg.contains("intentional panic in handler"), "got: {msg}");
            }
            other => panic!("expected Panic, got {other:?}"),
        }
        assert!(!handle.is_alive());
    }

    #[test]
    #[cfg(feature = "async")]
    fn child_handle_observes_panic_in_tasks_actor() {
        use crate::message::Message;
        use crate::tasks::actor::{Actor, ActorStart, Context, Handler};

        struct Boomer;
        struct Boom;
        impl Message for Boom {
            type Result = ();
        }
        impl Actor for Boomer {}
        impl Handler<Boom> for Boomer {
            async fn handle(
                &mut self,
                _msg: Boom,
                _reply: crate::reply::PendingReply<'_, ()>,
                _ctx: &Context<Self>,
            ) {
                {
                    panic!("intentional panic in handler");
                }
            }
        }

        let runtime = spawned_rt::tasks::Runtime::new().unwrap();
        runtime.block_on(async {
            let actor = Boomer.start();
            let handle = actor.child_handle();
            let _ = actor.send(Boom);

            let reason = handle.wait_exit_async().await;
            match reason {
                ExitReason::Panic(msg) => {
                    assert!(msg.contains("intentional panic in handler"), "got: {msg}");
                }
                other => panic!("expected Panic, got {other:?}"),
            }
            assert!(!handle.is_alive());
        });
    }

    #[test]
    #[cfg(feature = "async")]
    fn child_handle_observes_panic_in_started_callback() {
        use crate::tasks::actor::{Actor, ActorStart, Context};

        struct PanicStart;
        impl Actor for PanicStart {
            async fn started(&mut self, _ctx: &Context<Self>) {
                panic!("intentional panic in started");
            }
        }

        let runtime = spawned_rt::tasks::Runtime::new().unwrap();
        runtime.block_on(async {
            let actor = PanicStart.start();
            let handle = actor.child_handle();

            let reason = handle.wait_exit_async().await;
            match reason {
                ExitReason::Panic(msg) => {
                    assert!(msg.contains("intentional panic in started"), "got: {msg}");
                }
                other => panic!("expected Panic, got {other:?}"),
            }
        });
    }
}

#[cfg(test)]
mod closed_completion_tests {
    use super::*;
    use crate::link::new_link_table;
    #[test]
    fn closed_without_reason_is_dead_and_all_waits_agree() {
        #[cfg(not(feature = "async"))]
        let modes = [true];
        #[cfg(feature = "async")]
        let modes = [false, true];
        for threads in modes {
            let (tx, rx) = spawned_rt::sync::watch::channel(None);
            let target = super::tests::test_target(
                ActorId::next(),
                spawned_rt::threads::CancellationToken::new(),
            );
            let completion = if threads {
                Completion::Threads(rx)
            } else {
                #[cfg(not(feature = "async"))]
                unreachable!();
                #[cfg(feature = "async")]
                Completion::Tasks(rx)
            };
            let child = ChildHandle::new(target, completion, new_link_table());
            assert!(child.is_alive());
            drop(tx);
            assert_eq!(child.exit_reason(), Some(ExitReason::Killed));
            assert!(!child.is_alive());
            assert_eq!(
                futures::executor::block_on(child.wait_exit_async()),
                ExitReason::Killed
            );
            // Already-exited fast path must not enter another executor or reject
            // a current-thread runtime merely because no exit value was published.
            assert_eq!(
                futures::executor::block_on(async { child.wait_exit_blocking() }),
                ExitReason::Killed
            );
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                assert_eq!(child.wait_exit_blocking(), ExitReason::Killed);
            });
        }
    }
    #[test]
    fn closure_preserves_the_final_published_reason() {
        let (tx, rx) = spawned_rt::sync::watch::channel(None);
        tx.send_replace(Some(ExitReason::Normal));
        drop(tx);
        assert_eq!(poll_exit_reason(&rx), Some(ExitReason::Normal));
    }
}
