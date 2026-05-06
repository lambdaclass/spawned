use crate::error::ExitReason;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

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
    Watch(spawned_rt::tasks::watch::Receiver<Option<ExitReason>>),
    /// Threads mode: Mutex + Condvar carrying Option<ExitReason>.
    Condvar(Arc<(Mutex<Option<ExitReason>>, Condvar)>),
}

// ---------------------------------------------------------------------------
// ChildHandle
// ---------------------------------------------------------------------------

/// Type-erased cancel function. Wraps the mode-specific cancellation token.
type CancelFn = Arc<dyn Fn() + Send + Sync>;

/// Type-erased handle to a running actor. Provides lifecycle operations
/// (stop, liveness check, exit reason) without knowing the actor's concrete type.
///
/// Obtained via `ChildHandle::from(actor_ref)`.
///
/// Unlike `ActorRef<A>`, a `ChildHandle` cannot send messages — it only
/// provides supervision-related operations (stop, wait, check liveness).
#[derive(Clone)]
pub struct ChildHandle {
    id: ActorId,
    cancel: CancelFn,
    completion: Completion,
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
    pub(crate) fn from_tasks(
        id: ActorId,
        cancellation_token: spawned_rt::tasks::CancellationToken,
        completion_rx: spawned_rt::tasks::watch::Receiver<Option<ExitReason>>,
    ) -> Self {
        Self {
            id,
            cancel: Arc::new(move || cancellation_token.cancel()),
            completion: Completion::Watch(completion_rx),
        }
    }

    /// Create a ChildHandle for threads mode.
    pub(crate) fn from_threads(
        id: ActorId,
        cancellation_token: spawned_rt::threads::CancellationToken,
        completion: Arc<(Mutex<Option<ExitReason>>, Condvar)>,
    ) -> Self {
        Self {
            id,
            cancel: Arc::new(move || cancellation_token.cancel()),
            completion: Completion::Condvar(completion),
        }
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
    pub fn exit_reason(&self) -> Option<ExitReason> {
        match &self.completion {
            Completion::Watch(rx) => rx.borrow().clone(),
            Completion::Condvar(completion) => {
                let (lock, _) = &**completion;
                let guard = lock.lock().unwrap_or_else(|p| p.into_inner());
                guard.clone()
            }
        }
    }

    /// Block the calling thread until the actor stops and return the exit reason.
    ///
    /// Safe contexts:
    /// - Sync code with no active tokio runtime
    /// - Multi-thread tokio runtime (uses `block_in_place`)
    /// - Threads-mode actors (uses Condvar directly)
    ///
    /// **Panics** if called from within a current-thread tokio runtime on a
    /// tasks-mode handle: blocking the only runtime thread would prevent the
    /// actor task from making progress (deadlock). Use [`wait_exit_async`] from
    /// async context instead.
    ///
    /// [`wait_exit_async`]: ChildHandle::wait_exit_async
    pub fn wait_exit_blocking(&self) -> ExitReason {
        match &self.completion {
            Completion::Watch(rx) => {
                // wait_for_exit_watch_blocking is extracted to avoid holding the
                // borrow on self across the loop
                wait_for_exit_watch_blocking(&rx.clone())
            }
            Completion::Condvar(completion) => {
                let (lock, cvar) = &**completion;
                let mut guard = lock.lock().unwrap_or_else(|p| p.into_inner());
                loop {
                    if let Some(reason) = guard.clone() {
                        return reason;
                    }
                    guard = cvar.wait(guard).unwrap_or_else(|p| p.into_inner());
                }
            }
        }
    }

    /// Async wait until the actor stops and return the exit reason.
    ///
    /// Works with both execution modes. For Watch (tasks-mode handles), awaits
    /// the watch channel directly. For Condvar (threads-mode handles), delegates
    /// to a blocking task via `spawn_blocking` to avoid blocking the async runtime.
    ///
    /// **Note:** When used with threads-mode handles, this consumes a thread from
    /// tokio's blocking pool for the duration of the wait.
    pub async fn wait_exit_async(&self) -> ExitReason {
        match &self.completion {
            Completion::Watch(rx) => {
                let mut rx = rx.clone();
                loop {
                    if let Some(reason) = rx.borrow_and_update().clone() {
                        return reason;
                    }
                    if rx.changed().await.is_err() {
                        return ExitReason::Kill;
                    }
                }
            }
            Completion::Condvar(_) => {
                let handle = self.clone();
                spawned_rt::tasks::spawn_blocking(move || handle.wait_exit_blocking())
                    .await
                    .unwrap_or(ExitReason::Kill)
            }
        }
    }
}

/// Blocking wait on a watch channel.
///
/// - From a sync context (no tokio runtime): creates a temporary runtime.
/// - From a multi-thread tokio runtime: uses `block_in_place` + `Handle::block_on`.
/// - From a current-thread tokio runtime: **panics**, because blocking the only
///   runtime thread prevents the actor task from making progress (deadlock).
///   Use `wait_exit_async` from async context, or run on a multi-thread runtime.
///
/// Returns `ExitReason::Kill` if the watch sender is dropped without setting a
/// reason — this means the actor task was aborted externally (e.g., runtime
/// shutdown) without going through the normal exit path.
fn wait_for_exit_watch_blocking(
    rx: &spawned_rt::tasks::watch::Receiver<Option<ExitReason>>,
) -> ExitReason {
    // Fast path: already done — works from any context, no runtime needed
    if let Some(reason) = rx.borrow().clone() {
        return reason;
    }

    let mut rx = rx.clone();
    let wait = async move {
        loop {
            if let Some(reason) = rx.borrow_and_update().clone() {
                return reason;
            }
            if rx.changed().await.is_err() {
                return ExitReason::Kill;
            }
        }
    };

    match spawned_rt::tasks::Handle::try_current() {
        // No active runtime — create a temporary one
        Err(_) => spawned_rt::threads::block_on(wait),
        // Inside a tokio runtime — check the flavor
        Ok(handle) => match handle.runtime_flavor() {
            spawned_rt::tasks::RuntimeFlavor::MultiThread => {
                spawned_rt::tasks::block_in_place(|| handle.block_on(wait))
            }
            // CurrentThread runtime: blocking here would deadlock the actor task.
            // Including future flavors (e.g., MultiThreadAlt) for safety — only
            // MultiThread is known to be safe with block_in_place.
            _ => panic!(
                "ChildHandle::wait_exit_blocking() cannot be called from within a \
                current_thread tokio runtime; doing so would deadlock the actor \
                task that this call is waiting for. Use wait_exit_async() from \
                async context instead, or run on a multi-thread runtime."
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
        let completion = Arc::new((Mutex::new(None), Condvar::new()));
        let token = spawned_rt::threads::CancellationToken::new();
        let handle = ChildHandle::from_threads(ActorId::next(), token, completion.clone());

        assert!(handle.is_alive());
        assert!(handle.exit_reason().is_none());

        // Simulate actor completion
        {
            let (lock, cvar) = &*completion;
            let mut guard = lock.lock().unwrap();
            *guard = Some(ExitReason::Normal);
            cvar.notify_all();
        }

        assert!(!handle.is_alive());
        assert_eq!(handle.exit_reason(), Some(ExitReason::Normal));
    }

    #[test]
    fn child_handle_stop() {
        let completion = Arc::new((Mutex::new(None), Condvar::new()));
        let token = spawned_rt::threads::CancellationToken::new();
        assert!(!token.is_cancelled());
        let handle = ChildHandle::from_threads(ActorId::next(), token.clone(), completion);
        handle.stop();
        assert!(token.is_cancelled());
    }

    #[test]
    fn child_handle_equality_by_id() {
        let completion = Arc::new((Mutex::new(None), Condvar::new()));
        let token = spawned_rt::threads::CancellationToken::new();
        let id = ActorId::next();
        let h1 = ChildHandle::from_threads(id, token.clone(), completion.clone());
        let h2 = ChildHandle::from_threads(id, token, completion);
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
        use crate::tasks::actor::{Actor, ActorStart};

        struct Idler;
        impl Actor for Idler {}

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
        use crate::tasks::actor::{Actor, ActorStart};

        struct Idler;
        impl Actor for Idler {}

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
    #[should_panic(expected = "current_thread tokio runtime")]
    fn child_handle_wait_blocking_panics_on_current_thread_runtime() {
        use crate::tasks::actor::{Actor, ActorStart};

        struct Idler;
        impl Actor for Idler {}

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
        use crate::tasks::actor::{Actor, ActorStart};

        struct Idler;
        impl Actor for Idler {}

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
}
