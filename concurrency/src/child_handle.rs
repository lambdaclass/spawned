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
    /// Safe to call from any context (sync or async). For the Watch variant,
    /// uses the watch channel's built-in blocking recv. For Condvar, blocks
    /// on the condvar directly.
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

/// Blocking wait on a watch channel. Uses `block_in_place` if inside a tokio
/// runtime (safe from multi-threaded runtime), otherwise creates a temporary runtime.
///
/// Returns `ExitReason::Kill` if the watch sender is dropped without setting a
/// reason — this means the actor task was aborted externally (e.g., runtime
/// shutdown) without going through the normal exit path.
fn wait_for_exit_watch_blocking(
    rx: &spawned_rt::tasks::watch::Receiver<Option<ExitReason>>,
) -> ExitReason {
    // Fast path: already done
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

    // If inside a tokio runtime, use block_in_place + block_on to avoid
    // "cannot start a runtime from within a runtime" panic.
    if let Ok(handle) = spawned_rt::tasks::Handle::try_current() {
        spawned_rt::tasks::block_in_place(|| handle.block_on(wait))
    } else {
        spawned_rt::threads::block_on(wait)
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
    fn child_handle_wait_blocking_inside_runtime() {
        use crate::tasks::actor::{Actor, ActorStart};

        struct Idler;
        impl Actor for Idler {}

        let runtime = spawned_rt::tasks::Runtime::new().unwrap();
        runtime.block_on(async {
            let actor = Idler.start();
            let handle = actor.child_handle();
            handle.stop();

            // This is the bug-fix test: wait_exit_blocking inside a tokio runtime
            // should NOT panic (uses block_in_place internally)
            let reason = spawned_rt::tasks::block_in_place(|| handle.wait_exit_blocking());
            assert_eq!(reason, ExitReason::Normal);
        });
    }
}
