use crate::child_handle::Completion;
use crate::child_handle::{ActorId, ChildHandle};
use crate::error::NoProc;
use crate::error::{panic_message, ActorError, ExitReason};
use crate::link::{
    self, new_link_table, new_linked_exit_reason, new_trap_exit_flag, Exit, LinkOutcome, LinkTable,
    LinkedExitReason, SendExitFn, SendHeldFn, SignalTarget, TrapExitFlag,
};
use crate::mailbox::tasks as mpsc;
use crate::message::Message;
use crate::monitor::{Down, MonitorCleanup, MonitorRef, MonitorTable};
use crate::reply::{registry::Registry, reply_slot, Caller, Slot, SlotHandle};
use crate::reply::{Deadline, PendingReply, ReplyReceiver, DEFAULT_MAX_OUTSTANDING};
use crate::stop_token::StopToken;
use core::pin::pin;
use futures::future::{self, FutureExt as _};
use spawned_rt::{
    tasks::{self as rt, watch, JoinHandle},
    threads,
};
use std::sync::atomic::Ordering;
#[cfg(test)]
use std::{collections::HashMap, sync::Mutex};
use std::{
    fmt::Debug, future::Future, panic::AssertUnwindSafe, pin::Pin, sync::Arc, time::Duration,
};

pub use crate::response::DEFAULT_REQUEST_TIMEOUT;

/// Cleanup deadline for actors without a supervisor or system grace: the
/// bound on joining an exiting actor's cancelled jobs.
pub const DEFAULT_STOP_DEADLINE: Duration = Duration::from_secs(5);

#[path = "jobs.rs"]
mod jobs;
pub use jobs::{BlockingJobPermit, JobCtx, JobPermit};

mod envelope;
use envelope::*;
mod actor_ref;
mod context;
mod introspect;
use introspect::probe_of;
mod lifecycle;
#[cfg(test)]
use lifecycle::run_actor;

// ---------------------------------------------------------------------------
// Backend
// ---------------------------------------------------------------------------

/// Runtime backend for the actor's message loop (tasks mode only).
///
/// - `Async` — runs on the tokio async runtime (default, lowest overhead)
/// - `Blocking` — runs on tokio's blocking thread pool (for blocking I/O).
///   Each actor occupies a blocking worker until it stops. Actors beyond the
///   available pool capacity remain queued, including their `started()` hook.
/// - `Thread` — runs on a dedicated OS thread (for CPU-bound work or isolation).
///   Each actor has a dedicated root thread plus an isolated Tokio runtime with
///   one async worker. Async helpers can progress while the root blocks, and
///   `block_in_place` is supported. Blocking helpers may create further threads;
///   account for those threads and reactor descriptors when sizing populations.
///   Actor completion does not wait for detached helpers or runtime destruction.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum Backend {
    #[default]
    Async,
    Blocking,
    Thread,
}

// ---------------------------------------------------------------------------
// Actor trait
// ---------------------------------------------------------------------------

/// Trait for defining an actor's lifecycle hooks.
///
/// Implement this trait (typically via `#[actor]`) to define `started()` and
/// `stopped()` callbacks. Message handling is defined separately via [`Handler<M>`].
///
/// Actors must be `Send + Sized + 'static` so they can be moved to a spawned task.
pub trait Actor: Send + Sized + 'static {
    /// Deferred-reply credits per instance (live `ReplyTo` tokens plus running
    /// linked jobs). At the bound `defer()` answers the caller `Overloaded`.
    /// Default 1024; capped at 65535.
    const MAX_OUTSTANDING: usize = DEFAULT_MAX_OUTSTANDING;

    /// Native threads this instance's blocking jobs may run at once
    /// (`pipe_blocking*`). `None`: `min(4, available_parallelism)`.
    ///
    /// The default is small: an actor that runs many blocking jobs at once
    /// (I/O, proofs, signature checks) usually needs more, and under load the
    /// only symptom is `PipeError::NativeBudget` refusals (the first one
    /// per instance is logged as a `native_budget_exhausted` warning). Set it
    /// with `#[actor(max_native_jobs = N)]` or `ChildSpec::max_native_jobs`.
    const MAX_NATIVE_JOBS: Option<usize> = None;

    /// Initialisation, before any message (OTP `init/1`). Returns `()`,
    /// `Result<(), E>`, `Result<Init<Self>, E>` or [`Init<Self>`]: ready, a
    /// continuation, an error, a stop or ignore. The waiting starts
    /// (`start`, `start_link`, `start_monitor`) and [`ActorRef::ready`]
    /// answer with its result. A failed start never runs `stopped()`.
    fn started(
        &mut self,
        _ctx: &Context<Self>,
    ) -> impl Future<Output: Into<Init<Self>> + Send> + Send {
        async {}
    }

    fn stopped(&mut self, _ctx: &Context<Self>) -> impl Future<Output = ()> + Send {
        async {}
    }

    /// Called when a linked actor stops, if this actor has called
    /// `ctx.trap_exit(true)`. Default impl ignores the signal.
    fn exit_received(
        &mut self,
        _exit: Exit,
        _ctx: &Context<Self>,
    ) -> impl Future<Output = ()> + Send {
        async {}
    }
}

// ---------------------------------------------------------------------------
// Handler trait (per-message, uses RPITIT — NOT object-safe, that's fine)
// ---------------------------------------------------------------------------

/// Per-message handler trait. Implement once for each message type the actor handles.
///
/// `handle` is the envelope dispatch for both `send` and `request`. Answer
/// through `reply`: [`PendingReply::reply`] now, or [`PendingReply::defer`] to
/// answer from a later turn, a job or `stopped()`. A request whose
/// `PendingReply` is dropped unanswered gets `ReplyDropped`. A sent message
/// has a detached `PendingReply` (no caller).
///
/// `#[actor]` generates this from `#[request_handler]`, `#[send_handler]` and
/// `#[deferred_handler]` methods. Manual impls:
///
/// ```ignore
/// impl Handler<GetCount> for Counter {
///     async fn handle(&mut self, _: GetCount, reply: PendingReply<'_, u64>, _: &Context<Self>) {
///         reply.reply(self.count);
///     }
/// }
/// ```
///
/// Uses RPITIT (return-position `impl Trait` in traits), which means this trait
/// is **not** object-safe. For type-erased references, use [`Receiver<M>`] / [`Recipient<M>`].
pub trait Handler<M: Message>: Actor {
    /// Framework adapter for handlers that finish during dispatch. Returning
    /// Some preserves the untouched message and obligation for `handle`.
    #[doc(hidden)]
    #[inline]
    fn __spawned_handle_immediate<'turn>(
        &mut self,
        msg: M,
        reply: PendingReply<'turn, M::Result>,
        _ctx: &Context<Self>,
    ) -> Option<(M, PendingReply<'turn, M::Result>)> {
        Some((msg, reply))
    }

    fn handle(
        &mut self,
        msg: M,
        reply: PendingReply<'_, M::Result>,
        ctx: &Context<Self>,
    ) -> impl Future<Output = ()> + Send;
}

// ---------------------------------------------------------------------------
// Context
// ---------------------------------------------------------------------------

/// Handle passed to every handler and lifecycle hook, providing access to the
/// actor's mailbox and lifecycle controls.
///
/// Clone is cheap: one reference-count increment on the actor's shared state.
pub struct Context<A: Actor> {
    shared: Arc<ActorShared<A>>,
}

/// State shared by every `Context` and `ActorRef` of one actor.
///
/// The handles hold it behind one `Arc` so they stay a single pointer wide:
/// they are cloned into and moved through application futures (RPC contexts,
/// dispatch state machines), and debug builds give every such move its own
/// stack slot, so each extra handle word is paid many times over in callers'
/// stack frames. Cloning a handle is one reference-count increment.
struct ActorShared<A: Actor> {
    id: ActorId,
    sender: mpsc::Sender<Box<dyn Envelope<A> + Send>>,
    cancellation_token: StopToken,
    completion_rx: watch::Receiver<Option<ExitReason>>,
    monitors: MonitorTable,
    trap_exit: TrapExitFlag,
    links: LinkTable,
    linked_reason: LinkedExitReason,
    /// This instance as the target of exit signals and link notices.
    signal: Arc<SignalTarget>,
    mailbox: crate::instrumentation::Mailbox,
    /// The managed scope, set at the commit (unified start, stage 1).
    managed: std::sync::OnceLock<Arc<crate::system::Scope>>,
    /// The backend of an unmanaged instance (inherited by `start_link`).
    backend: Backend,
    /// This instance's reply registry: deferred tokens, credits, turn phase.
    replies: Arc<Registry>,
    /// This instance's pipe-to-self jobs.
    jobs: Arc<crate::reply::jobs::JobSet>,
    /// Becomes `true` when cleanup completes (death plus the bounded job
    /// join); closed if the actor task is dropped. `join()` waits for it.
    cleaned: watch::Receiver<bool>,
}

impl<A: Actor> Clone for Context<A> {
    fn clone(&self) -> Self {
        Self {
            shared: self.shared.clone(),
        }
    }
}

impl<A: Actor> Debug for Context<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Context").finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------
// Receiver trait (object-safe) + Recipient alias
// ---------------------------------------------------------------------------

/// Object-safe trait for sending a single message type to an actor.
///
/// Implemented automatically by `ActorRef<A>` and `Context<A>` for any
/// message type that `A` handles. Sealed: it cannot be implemented outside
/// spawned (a method can then be added without a breaking change).
pub trait Receiver<M: Message>: Send + Sync + crate::__private::Sealed {
    fn send(&self, msg: M) -> Result<(), ActorError>;
    /// Send a request without waiting (`gen_server:send_request/2` with an explicit deadline); its
    /// deadline is fixed now (the slot carries it). On a [`Recipient`],
    /// pass a `Deadline` (`Deadline::after(d)`, or `d.into()`).
    fn send_request(
        &self,
        msg: M,
        deadline: Deadline,
    ) -> Result<ReplyReceiver<M::Result>, ActorError>;
    /// `send_request` for the `request` helper, which arms its own timer:
    /// the deadline does not go to the owner's heap.
    #[doc(hidden)]
    fn send_request_timed(
        &self,
        msg: M,
        deadline: Deadline,
    ) -> Result<ReplyReceiver<M::Result>, ActorError> {
        self.send_request(msg, deadline)
    }
}

/// Type-erased reference for sending a single message type.
///
/// Obtained via `actor_ref.recipient::<M>()` or `ctx.recipient::<M>()`.
pub type Recipient<M> = Arc<dyn Receiver<M>>;

/// Send a request through a type-erased `Receiver` with a custom timeout.
pub async fn request<M: Message>(
    recipient: &dyn Receiver<M>,
    msg: M,
    timeout_duration: Duration,
) -> Result<M::Result, ActorError> {
    let observation = crate::instrumentation::RequestObservation::new();
    // The deadline is computed before enqueueing (send time, as OTP's call).
    // Bind the receiver before awaiting it (see `ActorRef::request_with_timeout`).
    let (deadline, timer) = crate::reply::request_timer(timeout_duration);
    let rx = match recipient.send_request_timed(msg, Deadline::of_ticks(deadline)) {
        Ok(rx) => rx,
        Err(error) => {
            let result = Err(error);
            observation.finish(&result);
            return result;
        }
    };
    let result = rx.into_timed_future_with(timer).await;
    observation.finish(&result);
    result
}

// ---------------------------------------------------------------------------
// ActorRef
// ---------------------------------------------------------------------------

/// External handle to a running actor. Cloneable, `Send + Sync`.
///
/// Use this to send messages, make requests, or wait for the actor to stop.
/// To stop the actor, send an explicit shutdown message through your protocol,
/// or call [`Context::stop`] from within a handler.
pub struct ActorRef<A: Actor> {
    shared: Arc<ActorShared<A>>,
}

impl<A: Actor> Debug for ActorRef<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActorRef").finish_non_exhaustive()
    }
}

impl<A: Actor> Clone for ActorRef<A> {
    fn clone(&self) -> Self {
        Self {
            shared: self.shared.clone(),
        }
    }
}

/// A context and its cleanup without spawning, for run-loop unit tests.
#[cfg(test)]
fn test_context<A: Actor>(
    tx: &mpsc::Sender<Box<dyn Envelope<A> + Send>>,
    token: &StopToken,
) -> (Context<A>, crate::actor_cleanup::ActorCleanup<fn()>) {
    let id = ActorId::next();
    let (completion_tx, completion_rx) = watch::channel(None);
    let trap = new_trap_exit_flag();
    let links = new_link_table();
    let request = new_linked_exit_reason();
    let monitors: MonitorTable = Arc::new(crate::monitor::fence::open());
    let signal = signal_target::<A>(
        id,
        None,
        tx,
        trap.clone(),
        request.clone(),
        token.clone(),
        crate::start::StartupCell::new(),
        crate::introspection::Meta::new(
            std::any::type_name::<A>(),
            crate::introspection::Host::Async,
        ),
    );
    let ctx = Context {
        shared: Arc::new(ActorShared {
            id,
            sender: tx.clone(),
            cancellation_token: token.clone(),
            completion_rx,
            monitors: monitors.clone(),
            trap_exit: trap,
            links: links.clone(),
            linked_reason: request.clone(),
            signal,
            mailbox: crate::instrumentation::Mailbox::default(),
            managed: std::sync::OnceLock::new(),
            backend: Backend::Async,
            replies: Registry::new(id, DEFAULT_MAX_OUTSTANDING),
            jobs: Arc::new(crate::reply::jobs::JobSet::new(id, 1)),
            cleaned: watch::channel(false).1,
        }),
    };
    let noop: fn() = || {};
    let cleanup =
        crate::actor_cleanup::ActorCleanup::new(id, links, request, monitors, completion_tx, noop)
            .with_replies(ctx.shared.replies.clone());
    (ctx, cleanup)
}

// ---------------------------------------------------------------------------
// Starting actors (unified start, 0.9)
// ---------------------------------------------------------------------------

mod start;
pub use start::{ActorStart, Init, Name, StartOptions, Starter, Starting};

impl<A: Actor> ActorShared<A> {
    /// The managed scope (set once, at the commit).
    #[inline]
    pub(crate) fn managed(&self) -> Option<&Arc<crate::system::Scope>> {
        self.managed.get()
    }
}

// ---------------------------------------------------------------------------
// send_message_on (utility)
// ---------------------------------------------------------------------------

/// Send a message to an actor when a future completes.
///
/// Spawns a task that races the future against the actor's cancellation token.
/// If the actor stops before the future completes, the message is not sent.
pub fn send_message_on<A, M, U>(ctx: Context<A>, future: U, msg: M) -> JoinHandle<()>
where
    A: Actor + Handler<M>,
    M: Message,
    U: Future + Send + 'static,
    <U as Future>::Output: Send,
{
    let cancellation_token = ctx.cancellation_token();
    let spawn_ctx = ctx.clone();
    let join_handle = spawn_ctx.spawn_framework(async move {
        let is_cancelled = pin!(cancellation_token.cancelled());
        let signal = pin!(future);
        match future::select(is_cancelled, signal).await {
            future::Either::Left(_) => tracing::debug!("Actor stopped"),
            future::Either::Right(_) => {
                if let Err(e) = ctx.send(msg) {
                    tracing::error!("Failed to send message: {e:?}")
                }
            }
        }
    });
    join_handle
}

// ---------------------------------------------------------------------------
// WarnOnBlocking (debug only)
// ---------------------------------------------------------------------------

#[cfg(debug_assertions)]
mod warn_on_block {
    use super::*;
    use std::time::Instant;
    use tracing::warn;

    pin_project_lite::pin_project! {
        pub struct WarnOnBlocking<F: Future>{
            #[pin]
            inner: F
        }
    }

    impl<F: Future> WarnOnBlocking<F> {
        pub fn new(inner: F) -> Self {
            Self { inner }
        }
    }

    impl<F: Future> Future for WarnOnBlocking<F> {
        type Output = F::Output;

        fn poll(
            self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Self::Output> {
            let type_id = std::any::type_name::<F>();
            let task_id = rt::task_id();
            let this = self.project();
            let now = Instant::now();
            let res = this.inner.poll(cx);
            let elapsed = now.elapsed();
            if elapsed > Duration::from_millis(10) {
                warn!(task = ?task_id, future = ?type_id, elapsed = ?elapsed, "Blocking operation detected");
            }
            res
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;

#[cfg(test)]
mod monitor_cleanup_regressions;

#[cfg(test)]
mod early_stop_regression;

#[cfg(test)]
mod idle_stop_regression;

/// Context and request APIs that the behavioural tests above do not reach.
#[cfg(test)]
mod api_coverage;

#[cfg(test)]
mod shared_state_release;

#[cfg(all(test, not(feature = "lifecycle-trace"), not(spawned_sim)))]
mod stop_receive_probe;
