use crate::error::ExitReason;
use crate::link::{LinkTable, LinkedExitReason, SendExitFn, TrapExitFlag};
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
    pub(crate) fn next() -> Self {
        Self(NEXT_ACTOR_ID.fetch_add(1, Ordering::Relaxed))
    }
}

impl std::fmt::Display for ActorId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ActorId({})", self.0)
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
    Tasks(spawned_rt::tasks::watch::Receiver<Option<ExitReason>>),
    /// Threads mode: watch channel, also waitable without an async runtime.
    Threads(spawned_rt::tasks::watch::Receiver<Option<ExitReason>>),
}

// ---------------------------------------------------------------------------
// ChildHandle
// ---------------------------------------------------------------------------

/// Type-erased cancel function. Wraps the mode-specific cancellation token.
pub(crate) type CancelFn = Arc<dyn Fn() + Send + Sync>;

/// Type-erased handle to a running actor. Provides lifecycle operations
/// (stop, liveness check, exit reason) without knowing the actor's concrete type.
///
/// Obtained via `ChildHandle::from(actor_ref)`.
///
/// Unlike `ActorRef<A>`, a `ChildHandle` cannot send messages directly — it
/// only provides supervision-related operations (stop, wait, check liveness,
/// link/monitor).
#[derive(Clone)]
pub struct ChildHandle {
    id: ActorId,
    cancel: CancelFn,
    completion: Completion,
    // Link/trap_exit support — carried so `ctx.link()` can wire up signal
    // propagation without knowing the underlying actor type.
    trap_exit: TrapExitFlag,
    links: LinkTable,
    linked_reason: LinkedExitReason,
    send_exit: SendExitFn,
}

impl std::fmt::Debug for ChildHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChildHandle")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl ChildHandle {
    /// Create a ChildHandle for tasks mode.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_tasks(
        id: ActorId,
        cancellation_token: spawned_rt::tasks::CancellationToken,
        completion_rx: spawned_rt::tasks::watch::Receiver<Option<ExitReason>>,
        trap_exit: TrapExitFlag,
        links: LinkTable,
        linked_reason: LinkedExitReason,
        send_exit: SendExitFn,
    ) -> Self {
        let stop_reason = linked_reason.clone();
        Self {
            id,
            cancel: Arc::new(move || {
                crate::link::normal_stop(&stop_reason, || cancellation_token.cancel())
            }),
            completion: Completion::Tasks(completion_rx),
            trap_exit,
            links,
            linked_reason,
            send_exit,
        }
    }

    /// Create a ChildHandle for threads mode.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_threads(
        id: ActorId,
        cancellation_token: spawned_rt::threads::CancellationToken,
        completion: spawned_rt::tasks::watch::Receiver<Option<ExitReason>>,
        trap_exit: TrapExitFlag,
        links: LinkTable,
        linked_reason: LinkedExitReason,
        send_exit: SendExitFn,
    ) -> Self {
        let stop_reason = linked_reason.clone();
        Self {
            id,
            cancel: Arc::new(move || {
                crate::link::normal_stop(&stop_reason, || cancellation_token.cancel())
            }),
            completion: Completion::Threads(completion),
            trap_exit,
            links,
            linked_reason,
            send_exit,
        }
    }

    /// Crate-internal accessors used by `ctx.link()` to wire up signal propagation.
    pub(crate) fn trap_exit_flag(&self) -> &TrapExitFlag {
        &self.trap_exit
    }
    pub(crate) fn links(&self) -> &LinkTable {
        &self.links
    }
    pub(crate) fn linked_reason(&self) -> &LinkedExitReason {
        &self.linked_reason
    }
    pub(crate) fn cancel_fn(&self) -> &CancelFn {
        &self.cancel
    }
    pub(crate) fn send_exit_fn(&self) -> &SendExitFn {
        &self.send_exit
    }

    // Each child_handle() creates this allocation; its clones share it.
    #[cfg(test)]
    pub(crate) fn lifetime_probe(&self) -> std::sync::Weak<dyn Fn() + Send + Sync> {
        Arc::downgrade(&self.cancel)
    }

    /// The actor's unique identity.
    pub fn id(&self) -> ActorId {
        self.id
    }

    /// Signal the actor to stop. The actor will finish its current handler,
    /// run `stopped()`, and exit.
    pub fn stop(&self) {
        (self.cancel)();
    }

    /// Returns `true` if the actor is still running.
    pub fn is_alive(&self) -> bool {
        self.exit_reason().is_none()
    }

    /// Poll the exit reason. Returns `None` if the actor is still running.
    /// A completion channel closed without a reason reports [`ExitReason::Kill`],
    /// matching the wait methods (for example, after runtime shutdown).
    pub fn exit_reason(&self) -> Option<ExitReason> {
        match &self.completion {
            Completion::Tasks(rx) | Completion::Threads(rx) => poll_exit_reason(rx),
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
    rx: &spawned_rt::tasks::watch::Receiver<Option<ExitReason>>,
) -> Option<ExitReason> {
    let closed = rx.has_changed().is_err();
    rx.borrow()
        .clone()
        .or_else(|| closed.then_some(ExitReason::Kill))
}

/// Async loop that polls a watch channel until it carries an exit reason.
/// Returns `ExitReason::Kill` if the watch sender is dropped without a reason.
async fn wait_loop(mut rx: spawned_rt::tasks::watch::Receiver<Option<ExitReason>>) -> ExitReason {
    loop {
        if let Some(reason) = rx.borrow_and_update().clone() {
            return reason;
        }
        if rx.changed().await.is_err() {
            return ExitReason::Kill;
        }
    }
}

/// Poll a watch subscription with a thread waker, without entering an executor.
/// This preserves synchronous waits even inside an existing futures executor.
pub(crate) fn wait_for_thread_exit_blocking(
    rx: spawned_rt::tasks::watch::Receiver<Option<ExitReason>>,
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
/// Returns `ExitReason::Kill` if the watch sender is dropped without setting a
/// reason — this means the actor task was aborted externally (e.g., runtime
/// shutdown) without going through the normal exit path.
fn wait_for_tasks_exit_blocking(
    rx: spawned_rt::tasks::watch::Receiver<Option<ExitReason>>,
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
        self.id == other.id
    }
}

impl Eq for ChildHandle {}

impl std::hash::Hash for ChildHandle {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::link::{new_link_table, new_linked_exit_reason, new_trap_exit_flag};

    // Shared tasks-mode fixture: an Idler actor that does nothing and never
    // stops on its own. Used by multiple tests below.
    mod tasks_fixtures {
        use crate::tasks::actor::Actor;
        pub struct Idler;
        impl Actor for Idler {}
    }

    /// Build a threads-mode ChildHandle for unit tests that exercise only the
    /// ChildHandle surface (no real actor).
    fn test_handle(
        token: spawned_rt::threads::CancellationToken,
        completion: spawned_rt::tasks::watch::Receiver<Option<ExitReason>>,
    ) -> ChildHandle {
        let no_op_send_exit: SendExitFn = Arc::new(|_| Ok(()));
        ChildHandle::from_threads(
            ActorId::next(),
            token,
            completion,
            new_trap_exit_flag(),
            new_link_table(),
            new_linked_exit_reason(),
            no_op_send_exit,
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
        assert!(s.starts_with("ActorId("));
    }

    #[test]
    fn child_handle_from_threads_basics() {
        let (completion_tx, completion) = spawned_rt::tasks::watch::channel(None);
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
        let (_completion_tx, completion) = spawned_rt::tasks::watch::channel(None);
        let token = spawned_rt::threads::CancellationToken::new();
        assert!(!token.is_cancelled());
        let handle = test_handle(token.clone(), completion);
        handle.stop();
        assert!(token.is_cancelled());
    }

    #[test]
    fn child_handle_equality_by_id() {
        let (_completion_tx, completion) = spawned_rt::tasks::watch::channel(None);
        let token = spawned_rt::threads::CancellationToken::new();
        let id = ActorId::next();
        let no_op_send_exit: SendExitFn = Arc::new(|_| Ok(()));
        let h1 = ChildHandle::from_threads(
            id,
            token.clone(),
            completion.clone(),
            new_trap_exit_flag(),
            new_link_table(),
            new_linked_exit_reason(),
            no_op_send_exit.clone(),
        );
        let h2 = ChildHandle::from_threads(
            id,
            token,
            completion,
            new_trap_exit_flag(),
            new_link_table(),
            new_linked_exit_reason(),
            no_op_send_exit,
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
            fn handle(&mut self, _msg: Stop, ctx: &Context<Self>) {
                ctx.stop();
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
            async fn handle(&mut self, _msg: Stop, ctx: &Context<Self>) {
                ctx.stop();
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
            fn handle(&mut self, _msg: Boom, _ctx: &Context<Self>) {
                panic!("intentional panic in handler");
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
            async fn handle(&mut self, _msg: Boom, _ctx: &Context<Self>) {
                panic!("intentional panic in handler");
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
    use crate::link::{new_link_table, new_linked_exit_reason, new_trap_exit_flag};
    #[test]
    fn closed_without_reason_is_dead_and_all_waits_agree() {
        for threads in [false, true] {
            let (tx, rx) = spawned_rt::tasks::watch::channel(None);
            let child = if threads {
                ChildHandle::from_threads(
                    ActorId::next(),
                    spawned_rt::threads::CancellationToken::new(),
                    rx,
                    new_trap_exit_flag(),
                    new_link_table(),
                    new_linked_exit_reason(),
                    Arc::new(|_| Ok(())),
                )
            } else {
                ChildHandle::from_tasks(
                    ActorId::next(),
                    spawned_rt::tasks::CancellationToken::new(),
                    rx,
                    new_trap_exit_flag(),
                    new_link_table(),
                    new_linked_exit_reason(),
                    Arc::new(|_| Ok(())),
                )
            };
            assert!(child.is_alive());
            drop(tx);
            assert_eq!(child.exit_reason(), Some(ExitReason::Kill));
            assert!(!child.is_alive());
            assert_eq!(
                futures::executor::block_on(child.wait_exit_async()),
                ExitReason::Kill
            );
            // Already-exited fast path must not enter another executor or reject
            // a current-thread runtime merely because no exit value was published.
            assert_eq!(
                futures::executor::block_on(async { child.wait_exit_blocking() }),
                ExitReason::Kill
            );
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                assert_eq!(child.wait_exit_blocking(), ExitReason::Kill);
            });
        }
    }
    #[test]
    fn closure_preserves_the_final_published_reason() {
        let (tx, rx) = spawned_rt::tasks::watch::channel(None);
        tx.send(Some(ExitReason::Normal)).unwrap();
        drop(tx);
        assert_eq!(poll_exit_reason(&rx), Some(ExitReason::Normal));
    }
}
