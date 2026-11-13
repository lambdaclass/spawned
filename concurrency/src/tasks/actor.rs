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
use core::pin::pin;
use futures::future::{self, FutureExt as _};
use spawned_rt::{
    tasks::{self as rt, watch, CancellationToken, JoinHandle},
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

    fn started(&mut self, _ctx: &Context<Self>) -> impl Future<Output = ()> + Send {
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
    fn handle(
        &mut self,
        msg: M,
        reply: PendingReply<'_, M::Result>,
        ctx: &Context<Self>,
    ) -> impl Future<Output = ()> + Send;
}

// ---------------------------------------------------------------------------
// Envelope (type-erasure on the actor side)
// ---------------------------------------------------------------------------

trait Envelope<A: Actor>: Send {
    /// The model's mailbox item kind (`Lifecycle.tla`), for the trace.
    #[cfg(feature = "lifecycle-trace")]
    fn trace_kind(&self) -> &'static str {
        "other"
    }
    #[cfg(any(feature = "diagnostics", spawned_sim))]
    fn message_type(&self) -> &'static str {
        std::any::type_name::<Self>()
    }
    #[cfg(feature = "metrics")]
    fn observation(&mut self) -> Option<crate::instrumentation::MessageObservation> {
        None
    }
    #[cfg(feature = "metrics")]
    fn reject(self: Box<Self>) {}

    /// The owner reached its death point with this envelope undispatched:
    /// answer its request now (the final reason), before any payload of the
    /// mailbox is destroyed. No user code runs.
    fn discharge(&mut self, _replies: Option<&Registry>) {}

    /// Dispatch once: the turn's future, and the request's caller if any.
    /// The envelope keeps the request's slot for the whole turn and lends
    /// it to the handler's `PendingReply` (no reference count change per
    /// message); the run loop answers the lent caller at the death point if
    /// the runtime destroys the turn before it finishes.
    fn handle<'a>(&'a mut self, actor: &'a mut A, ctx: &'a Context<A>) -> Dispatch<'a>;
}

/// A dispatched turn: its future and the caller it answers, lent by the
/// envelope for the turn.
type Dispatch<'a> = (
    Pin<Box<dyn Future<Output = ()> + Send + 'a>>,
    Option<&'a dyn Caller>,
);

/// Dispatch `envelope`, catching a panic while the handler builds its
/// future (a manual `Handler` may panic before it returns one): a handler
/// panic like any other.
fn dispatch<'a, A: Actor>(
    envelope: &'a mut (dyn Envelope<A> + Send),
    actor: &'a mut A,
    ctx: &'a Context<A>,
) -> Result<Dispatch<'a>, Box<dyn std::any::Any + Send>> {
    let mut parts = Some((envelope, actor));
    std::panic::catch_unwind(AssertUnwindSafe(move || {
        let (envelope, actor) = parts.take().expect("dispatched once");
        envelope.handle(actor, ctx)
    }))
}

/// Take an envelope's payload at its one dispatch.
fn dispatched<T>(payload: &mut Option<T>) -> T {
    payload.take().expect("an envelope is dispatched once")
}

/// Answer every request queued in a dead owner's mailbox, destroying
/// nothing: the drained envelopes come back, to be destroyed once the death
/// point's wake-ups ran. Admission is closed first, so nothing is queued
/// after.
fn answer_mailbox<A: Actor>(
    rx: &mut mpsc::Receiver<Box<dyn Envelope<A> + Send>>,
    replies: Option<&Registry>,
) -> Vec<Box<dyn Envelope<A> + Send>> {
    let mut queued = rx.drain();
    for envelope in &mut queued {
        envelope.discharge(replies);
    }
    queued
}

/// A never-polled actor's startup payload, dropped with its runtime.
type StartupPayload<A> = (
    A,
    Context<A>,
    mpsc::Receiver<Box<dyn Envelope<A> + Send>>,
    CancellationToken,
);
fn discharge_startup<A: Actor>(payload: &mut StartupPayload<A>) -> Vec<Box<dyn Send>> {
    let replies = payload.1.shared.replies.clone();
    erase(answer_mailbox(&mut payload.2, Some(&replies)))
}
fn erase<T: Send + 'static>(items: Vec<T>) -> Vec<Box<dyn Send>> {
    items
        .into_iter()
        .map(|item| Box::new(item) as Box<dyn Send>)
        .collect()
}

struct MessageEnvelope<M: Message> {
    // Before the message: a plain drop also answers the request first.
    slot: SlotHandle<M::Result>,
    /// The slot lent to the dispatched turn (taken from `slot`).
    lent: Option<Arc<Slot<M::Result>>>,
    msg: Option<M>,
    #[cfg(feature = "metrics")]
    observation: Option<crate::instrumentation::MessageObservation>,
    /// The recorded actor that sent it (0: outside code or a request); its
    /// mailbox item kind is then the model's `msg`.
    #[cfg(feature = "lifecycle-trace")]
    from: u64,
}

impl<A, M> Envelope<A> for MessageEnvelope<M>
where
    A: Actor + Handler<M>,
    M: Message,
{
    /// The model's mailbox item kind (`Lifecycle.tla`), for the trace.
    #[cfg(feature = "lifecycle-trace")]
    fn trace_kind(&self) -> &'static str {
        if self.from != 0 {
            "msg"
        } else {
            crate::lifecycle_trace::life_kind::<M>()
        }
    }
    #[cfg(any(feature = "diagnostics", spawned_sim))]
    fn message_type(&self) -> &'static str {
        std::any::type_name::<M>()
    }
    #[cfg(feature = "metrics")]
    fn observation(&mut self) -> Option<crate::instrumentation::MessageObservation> {
        self.observation.take()
    }
    #[cfg(feature = "metrics")]
    fn reject(mut self: Box<Self>) {
        if let Some(observation) = self.observation.take() {
            observation.reject();
        }
    }
    fn discharge(&mut self, replies: Option<&Registry>) {
        self.slot.dispose(replies);
        drop(std::mem::replace(&mut self.slot, SlotHandle::detached()));
    }
    fn handle<'a>(&'a mut self, actor: &'a mut A, ctx: &'a Context<A>) -> Dispatch<'a> {
        let Self {
            slot, lent, msg, ..
        } = self;
        let msg = dispatched(msg);
        *lent = slot.take();
        let lent: &'a Option<Arc<Slot<M::Result>>> = lent;
        let reply = PendingReply::lent(lent.as_ref(), &ctx.shared.replies);
        (
            Box::pin(actor.handle(msg, reply, ctx)),
            lent.as_ref().map(|slot| slot as &dyn Caller),
        )
    }
}

/// A sent message (no caller): no reply slot to hold, lend or dispose of,
/// so its dispatch builds only a detached `PendingReply`.
struct SendEnvelope<M: Message> {
    msg: Option<M>,
    #[cfg(feature = "metrics")]
    observation: Option<crate::instrumentation::MessageObservation>,
    #[cfg(feature = "lifecycle-trace")]
    from: u64,
}

impl<A, M> Envelope<A> for SendEnvelope<M>
where
    A: Actor + Handler<M>,
    M: Message,
{
    #[cfg(feature = "lifecycle-trace")]
    fn trace_kind(&self) -> &'static str {
        if self.from != 0 {
            "msg"
        } else {
            crate::lifecycle_trace::life_kind::<M>()
        }
    }
    #[cfg(any(feature = "diagnostics", spawned_sim))]
    fn message_type(&self) -> &'static str {
        std::any::type_name::<M>()
    }
    #[cfg(feature = "metrics")]
    fn observation(&mut self) -> Option<crate::instrumentation::MessageObservation> {
        self.observation.take()
    }
    #[cfg(feature = "metrics")]
    fn reject(mut self: Box<Self>) {
        if let Some(observation) = self.observation.take() {
            observation.reject();
        }
    }
    #[inline]
    fn handle<'a>(&'a mut self, actor: &'a mut A, ctx: &'a Context<A>) -> Dispatch<'a> {
        let msg = dispatched(&mut self.msg);
        (
            Box::pin(actor.handle(msg, PendingReply::detached(), ctx)),
            None,
        )
    }
}

impl<A: Actor + Handler<Down>> Envelope<A> for DownEnvelope {
    #[cfg(feature = "lifecycle-trace")]
    fn trace_kind(&self) -> &'static str {
        "down"
    }
    #[cfg(any(feature = "diagnostics", spawned_sim))]
    fn message_type(&self) -> &'static str {
        std::any::type_name::<Down>()
    }
    #[cfg(feature = "metrics")]
    fn observation(&mut self) -> Option<crate::instrumentation::MessageObservation> {
        self.observation.take()
    }
    #[cfg(feature = "metrics")]
    fn reject(mut self: Box<Self>) {
        if let Some(observation) = self.observation.take() {
            observation.reject();
        }
    }
    fn handle<'a>(&'a mut self, actor: &'a mut A, ctx: &'a Context<A>) -> Dispatch<'a> {
        let down = dispatched(&mut self.down);
        let turn = Box::pin(async move {
            fail_point!("monitor.down_before_dispatch", async);
            if crate::monitor::fence::take_queued(&ctx.shared.monitors, &down.monitor_ref) {
                let reply = PendingReply::new(None, &ctx.shared.replies);
                actor.handle(down, reply, ctx).await;
            }
        });
        (turn, None)
    }
}

/// Admit a message. Past the death point (the registry is sealed, or the
/// mailbox is gone) it reports `NoProc`; before it, including while the
/// actor exits, it is queued, and a request is answered at the death point
/// if it is not handled.
fn enqueue<A, M>(
    sender: &mpsc::Sender<Box<dyn Envelope<A> + Send>>,
    _mailbox: &crate::instrumentation::Mailbox,
    replies: &Registry,
    msg: M,
    slot: SlotHandle<M::Result>,
) -> Result<(), ActorError>
where
    A: Actor + Handler<M>,
    M: Message,
{
    let from = crate::lifecycle_trace::life_sender::<M>(replies.owner().raw(), slot.is_detached());
    let envelope: Box<dyn Envelope<A> + Send> = Box::new(MessageEnvelope {
        msg: Some(msg),
        slot,
        lent: None,
        #[cfg(feature = "metrics")]
        observation: Some(_mailbox.enqueued()),
        #[cfg(feature = "lifecycle-trace")]
        from,
    });
    admit::<A, M>(sender, replies, envelope, from)
}

/// Admit a sent message (no caller): no reply slot at all.
fn enqueue_send<A, M>(
    sender: &mpsc::Sender<Box<dyn Envelope<A> + Send>>,
    _mailbox: &crate::instrumentation::Mailbox,
    replies: &Registry,
    msg: M,
) -> Result<(), ActorError>
where
    A: Actor + Handler<M>,
    M: Message,
{
    let from = crate::lifecycle_trace::life_sender::<M>(replies.owner().raw(), true);
    let envelope: Box<dyn Envelope<A> + Send> = Box::new(SendEnvelope {
        msg: Some(msg),
        #[cfg(feature = "metrics")]
        observation: Some(_mailbox.enqueued()),
        #[cfg(feature = "lifecycle-trace")]
        from,
    });
    admit::<A, M>(sender, replies, envelope, from)
}

/// A monitor's `Down`, enqueued by the framework (the waiter at the target's
/// publication, or `monitor` past the target's death point). Its dispatch
/// runs `Handler<Down>` only while the monitor's table entry is still
/// queued: a `demonitor` that returned first flushed it (OTP
/// `demonitor(Ref, [flush])`). A `Down` user code sends is an ordinary
/// message.
struct DownEnvelope {
    down: Option<Down>,
    #[cfg(feature = "metrics")]
    observation: Option<crate::instrumentation::MessageObservation>,
}

fn enqueue_down<A: Actor + Handler<Down>>(
    sender: &mpsc::Sender<Box<dyn Envelope<A> + Send>>,
    _mailbox: &crate::instrumentation::Mailbox,
    replies: &Registry,
    down: Down,
) -> Result<(), ActorError> {
    let envelope: Box<dyn Envelope<A> + Send> = Box::new(DownEnvelope {
        down: Some(down),
        #[cfg(feature = "metrics")]
        observation: Some(_mailbox.enqueued()),
    });
    admit::<A, Down>(sender, replies, envelope, 0)
}

/// Admission of a built envelope (`M` names its kind for the trace).
fn admit<A: Actor, M: 'static>(
    sender: &mpsc::Sender<Box<dyn Envelope<A> + Send>>,
    replies: &Registry,
    envelope: Box<dyn Envelope<A> + Send>,
    _from: u64,
) -> Result<(), ActorError> {
    // Admission and its trace event are one step of the lifecycle trace.
    let owner = replies.owner().raw();
    crate::lifecycle_trace::life_section(owner, || {
        if replies.refuses_admission() {
            #[cfg(feature = "metrics")]
            envelope.reject();
            crate::lifecycle_trace::life_send::<M>(owner, false, _from);
            return Err(ActorError::NoProc);
        }
        match sender.send(envelope) {
            Ok(()) => {
                #[cfg(feature = "metrics")]
                crate::instrumentation::accepted();
                crate::lifecycle_trace::life_send::<M>(owner, true, _from);
                Ok(())
            }
            Err(_error) => {
                #[cfg(feature = "metrics")]
                {
                    _error.0.reject();
                }
                crate::lifecycle_trace::life_send::<M>(owner, false, _from);
                Err(ActorError::NoProc)
            }
        }
    })
}

/// A closure run in one of the actor's turns (`spawned_test::tasks::in_actor`).
type InTurn<A, R> = Box<dyn FnOnce(&mut A, &Context<A>) -> R + Send>;
struct InTurnEnvelope<A: Actor, R: Send + 'static> {
    slot: SlotHandle<R>,
    lent: Option<Arc<Slot<R>>>,
    f: Option<InTurn<A, R>>,
}

impl<A: Actor, R: Send + 'static> Envelope<A> for InTurnEnvelope<A, R> {
    #[cfg(feature = "diagnostics")]
    fn message_type(&self) -> &'static str {
        "InTurn"
    }
    fn discharge(&mut self, replies: Option<&Registry>) {
        self.slot.dispose(replies);
        drop(std::mem::replace(&mut self.slot, SlotHandle::detached()));
    }
    fn handle<'a>(&'a mut self, actor: &'a mut A, ctx: &'a Context<A>) -> Dispatch<'a> {
        let Self { slot, lent, f } = self;
        let f = dispatched(f);
        *lent = slot.take();
        let lent: &'a Option<Arc<Slot<R>>> = lent;
        // Inside the future, so a panicking closure is a handler panic.
        let turn = Box::pin(async move {
            let reply = PendingReply::lent(lent.as_ref(), &ctx.shared.replies);
            let value = f(actor, ctx);
            let _ = reply.reply(value);
        });
        (turn, lent.as_ref().map(|slot| slot as &dyn Caller))
    }
}

// Private lifecycle signal, excluded from application message/handler metrics.
// Nested supervisor adapters have no public mailbox traffic to bury this signal.
struct FailureEnvelope(Option<String>);
impl<A: Actor> Envelope<A> for FailureEnvelope {
    #[cfg(feature = "diagnostics")]
    fn message_type(&self) -> &'static str {
        "Exit"
    }

    fn handle<'a>(&'a mut self, _: &'a mut A, _: &'a Context<A>) -> Dispatch<'a> {
        let failure = dispatched(&mut self.0);
        (
            Box::pin(async move {
                panic!("nested supervisor failed: {failure}");
            }),
            None,
        )
    }
}

/// System envelope that dispatches an `Exit` notification to the actor's
/// `exit_received` callback. Unlike `MessageEnvelope<M>`, this does not require
/// `Handler<Exit>` on the actor — every actor can receive `Exit` via the
/// trait's default-no-op `exit_received` callback.
struct ExitEnvelope {
    exit: Option<Exit>,
}

impl<A: Actor> Envelope<A> for ExitEnvelope {
    /// The model's mailbox item kind (`Lifecycle.tla`), for the trace.
    #[cfg(feature = "lifecycle-trace")]
    fn trace_kind(&self) -> &'static str {
        "exit"
    }
    #[cfg(feature = "diagnostics")]
    fn message_type(&self) -> &'static str {
        "Exit"
    }

    fn handle<'a>(&'a mut self, actor: &'a mut A, ctx: &'a Context<A>) -> Dispatch<'a> {
        let exit = dispatched(&mut self.exit);
        (
            Box::pin(async move {
                actor.exit_received(exit, ctx).await;
            }),
            None,
        )
    }
}

/// A parent exit (E3): when dispatched, exit begins with the parent's reason
/// and `stopped()` runs. `exit_received` never sees it.
struct ParentExitEnvelope {
    exit: Option<Exit>,
}

impl<A: Actor> Envelope<A> for ParentExitEnvelope {
    /// The model's mailbox item kind (`Lifecycle.tla`), for the trace.
    #[cfg(feature = "lifecycle-trace")]
    fn trace_kind(&self) -> &'static str {
        "pexit"
    }
    #[cfg(feature = "diagnostics")]
    fn message_type(&self) -> &'static str {
        "ParentExit"
    }

    fn handle<'a>(&'a mut self, _actor: &'a mut A, ctx: &'a Context<A>) -> Dispatch<'a> {
        let exit = dispatched(&mut self.exit);
        (Box::pin(async move { ctx.parent_exit(exit.reason) }), None)
    }
}

/// An exit signal that arrived while `started()` ran, in its mailbox place
/// (phase 0). If the actor trapped exits when `started()` returned it is an
/// `Exit` (or parent exit); otherwise it was resolved then and is inert.
struct HeldEnvelope {
    exit: Option<Exit>,
    parent: bool,
}

impl<A: Actor> Envelope<A> for HeldEnvelope {
    /// The model's mailbox item kind (`Lifecycle.tla`), for the trace.
    #[cfg(feature = "lifecycle-trace")]
    fn trace_kind(&self) -> &'static str {
        "held"
    }
    #[cfg(feature = "diagnostics")]
    fn message_type(&self) -> &'static str {
        "Exit"
    }

    fn handle<'a>(&'a mut self, actor: &'a mut A, ctx: &'a Context<A>) -> Dispatch<'a> {
        let exit = dispatched(&mut self.exit);
        let parent = self.parent;
        let turn = Box::pin(async move {
            if !link::lock_request(&ctx.shared.linked_reason).held_trapped() {
                return;
            }
            if parent {
                ctx.parent_exit(exit.reason);
            } else {
                actor.exit_received(exit, ctx).await;
            }
        });
        (turn, None)
    }
}

/// Build the signal target of a new actor instance.
fn signal_target<A: Actor>(
    id: ActorId,
    parent: Option<ActorId>,
    sender: &mpsc::Sender<Box<dyn Envelope<A> + Send>>,
    trap: TrapExitFlag,
    request: LinkedExitReason,
    token: CancellationToken,
) -> Arc<SignalTarget> {
    let exit_tx = sender.clone();
    let parent_tx = sender.clone();
    let held_tx = sender.clone();
    let send_exit: SendExitFn = Arc::new(move |exit: Exit| {
        let envelope: Box<dyn Envelope<A> + Send> = Box::new(ExitEnvelope { exit: Some(exit) });
        exit_tx.send(envelope).map_err(|_| ActorError::NoProc)
    });
    let send_parent_exit: SendExitFn = Arc::new(move |exit: Exit| {
        let envelope: Box<dyn Envelope<A> + Send> =
            Box::new(ParentExitEnvelope { exit: Some(exit) });
        parent_tx.send(envelope).map_err(|_| ActorError::NoProc)
    });
    let send_held: SendHeldFn = Arc::new(move |exit: Exit, parent: bool| {
        let envelope: Box<dyn Envelope<A> + Send> = Box::new(HeldEnvelope {
            exit: Some(exit),
            parent,
        });
        held_tx.send(envelope).map_err(|_| ActorError::NoProc)
    });
    Arc::new(SignalTarget {
        id,
        parent,
        trap,
        request,
        wake: Arc::new(move || token.cancel()),
        send_exit,
        send_parent_exit,
        send_held,
    })
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
    cancellation_token: CancellationToken,
    completion_rx: watch::Receiver<Option<ExitReason>>,
    monitors: MonitorTable,
    trap_exit: TrapExitFlag,
    links: LinkTable,
    linked_reason: LinkedExitReason,
    /// This instance as the target of exit signals and link notices.
    signal: Arc<SignalTarget>,
    mailbox: crate::instrumentation::Mailbox,
    managed: Option<Arc<crate::system::Scope>>,
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

impl<A: Actor> Context<A> {
    /// The `Context` of `actor_ref`'s actor (public as `ActorRef::context`).
    pub(crate) fn from_ref(actor_ref: &ActorRef<A>) -> Self {
        Self {
            shared: actor_ref.shared.clone(),
        }
    }

    /// Waiting messages, including in-flight enqueue reservations, excluding
    /// the active handler and internal exit/shutdown control envelopes.
    /// Concurrent observations are approximate; no registry or actor labels.
    #[cfg(feature = "metrics")]
    pub fn mailbox_depth(&self) -> u64 {
        self.shared.mailbox.depth()
    }

    /// Deferred-reply credits in use by this instance: live deferred
    /// tokens plus running linked jobs (bounded by `Actor::MAX_OUTSTANDING`).
    /// Experimental in 0.8 (for tests and diagnostics; the `metrics`
    /// feature exports the same count): it may change before 1.0.
    pub fn deferred_outstanding(&self) -> usize {
        self.shared.replies.outstanding()
    }

    /// The actor's unique identity.
    pub fn id(&self) -> ActorId {
        self.shared.id
    }

    /// Self-exit: the current handler finishes, then `stopped()` is called
    /// and the actor exits `Normal`.
    pub fn stop(&self) {
        self.stop_with_reason(ExitReason::Normal);
    }

    /// Self-exit with an explicit reason (OTP `{stop, Reason, State}`), without
    /// unwinding. The current handler finishes and `stopped()` runs. The first
    /// requested self-exit or stop wins; until the death point a kill
    /// replaces it with `Killed`, an untrapped exit signal with its reason, and
    /// a panic (in the handler or `stopped()`) with `Panic`. `Kill` stays
    /// `Kill` and propagates over links as `Kill` (trappable).
    pub fn stop_with_reason(&self, reason: ExitReason) {
        link::stop_with_reason(self.shared.id, &self.shared.linked_reason, reason, || {
            self.shared.cancellation_token.cancel()
        });
    }

    /// A parent exit being dispatched: normal termination with its reason
    /// (part of the model's `Dispatch`, so not recorded as a stop).
    fn parent_exit(&self, reason: ExitReason) {
        link::request_stop(&self.shared.linked_reason, reason, || {
            self.shared.cancellation_token.cancel()
        });
    }

    /// Send an exit signal from this actor to `target` (OTP
    /// `exit_signal(Target, Reason)`); see [`ChildHandle::exit`] for the
    /// delivery rules. From the target's `start_linked` parent the signal is a
    /// parent exit (a trapping target ends normally, through `stopped()`).
    /// Works until this actor's death point, including from `stopped()`;
    /// inert after it. A signal to this actor's own handle is a signal, not a
    /// self-exit: a non-trapping actor ignores `Normal`.
    pub fn exit(&self, target: &ChildHandle, reason: ExitReason) {
        // Held until delivered: the death point waits for it, so a signal
        // sent before death precedes the death notices (and none is sent
        // after it).
        let Some(_sending) = self.shared.replies.signal_permit() else {
            return;
        };
        if link::lock_request(&self.shared.linked_reason).is_dead() {
            return;
        }
        fail_point!("actor.exit_before_deliver");
        target.exit_from(self.shared.id, reason, false);
    }

    /// Send a fire-and-forget message to this actor.
    pub fn send<M>(&self, msg: M) -> Result<(), ActorError>
    where
        A: Handler<M>,
        M: Message,
    {
        enqueue_send(
            &self.shared.sender,
            &self.shared.mailbox,
            &self.shared.replies,
            msg,
        )
    }

    /// Send a request without waiting (`gen_server:send_request/2` with an explicit deadline) and get
    /// its [`ReplyReceiver`]. The deadline is fixed now: a `Duration` from
    /// now, an `Instant`, or [`Deadline::Never`] for an unbounded wait
    /// ([`DEFAULT_REQUEST_TIMEOUT`] is what [`request`](Self::request)
    /// uses). The handler sees it through `PendingReply::deadline`.
    pub fn send_request<M>(
        &self,
        msg: M,
        deadline: impl Into<Deadline>,
    ) -> Result<ReplyReceiver<M::Result>, ActorError>
    where
        A: Handler<M>,
        M: Message,
    {
        let deadline = deadline.into();
        self.send_request_with(msg, deadline, true)
    }

    /// `send_request`; `raw` puts a deadline in the owner's heap now (the
    /// receiver may be awaited directly). The `request` paths arm their own
    /// timer and pass `false`.
    pub(crate) fn send_request_with<M>(
        &self,
        msg: M,
        deadline: Deadline,
        raw: bool,
    ) -> Result<ReplyReceiver<M::Result>, ActorError>
    where
        A: Handler<M>,
        M: Message,
    {
        let (slot, rx) = reply_slot(deadline, Some(&self.shared.replies), raw);
        enqueue(
            &self.shared.sender,
            &self.shared.mailbox,
            &self.shared.replies,
            msg,
            slot,
        )?;
        Ok(rx)
    }

    /// Send a request and wait for the reply (default 5s timeout).
    pub async fn request<M>(&self, msg: M) -> Result<M::Result, ActorError>
    where
        A: Handler<M>,
        M: Message,
    {
        self.request_with_timeout(msg, DEFAULT_REQUEST_TIMEOUT)
            .await
    }

    /// Send a request and wait for the reply with a custom timeout.
    pub async fn request_with_timeout<M>(
        &self,
        msg: M,
        duration: Duration,
    ) -> Result<M::Result, ActorError>
    where
        A: Handler<M>,
        M: Message,
    {
        let observation = crate::instrumentation::RequestObservation::new();
        // Bind the receiver before awaiting it: awaiting inside a `match` on
        // the `Result` keeps the scrutinee's storage alive beside the awaited
        // receiver and doubles this future's size.
        let rx = match self.send_request_with(msg, Deadline::after(duration), false) {
            Ok(rx) => rx,
            Err(error) => {
                let result = Err(error);
                observation.finish(&result);
                return result;
            }
        };
        let result = rx.into_timed_future_after(duration).await;
        observation.finish(&result);
        result
    }

    /// Get a type-erased `Recipient<M>` for sending a single message type
    /// to this actor.
    pub fn recipient<M>(&self) -> Recipient<M>
    where
        A: Handler<M>,
        M: Message,
    {
        Arc::new(self.clone())
    }

    /// Get an `ActorRef<A>` from this context.
    pub fn actor_ref(&self) -> ActorRef<A> {
        ActorRef {
            shared: self.shared.clone(),
        }
    }

    /// Set up a unidirectional monitor on another actor.
    ///
    /// Returns a [`MonitorRef`] that can be used to cancel the monitor via
    /// [`Context::demonitor`]. When the monitored actor's exit is published,
    /// a [`Down`] with its final reason is delivered via `Handler<Down>`.
    ///
    /// If the target is past its death point, exactly one
    /// `Down { reason: NoProc }` is delivered at once (OTP `noproc`; use
    /// [`ChildHandle::exit_reason`] for the real reason).
    ///
    /// Multiple independent monitors are allowed on the same target — each
    /// call returns a distinct `MonitorRef`.
    ///
    /// Monitors are unidirectional: the monitored actor is unaware of the
    /// monitor and unaffected by it.
    /// Registration after owner shutdown starts (including in `stopped()`)
    /// returns an inert reference and creates no waiter or notification.
    pub fn monitor(&self, target: &ChildHandle) -> MonitorRef
    where
        A: Handler<Down>,
    {
        fail_point!("monitor.before_install");
        let monitor_ref = MonitorRef::next();
        let active = spawned_rt::tasks::CancellationToken::new();

        let observer = self.shared.id.raw();
        let registered = crate::lifecycle_trace::life_section(observer, || {
            let event = |x| {
                crate::lifecycle_trace::LifeEvent::new("Monitor", observer)
                    .s(target.id().raw())
                    .m(monitor_ref.raw())
                    .x(x)
            };
            crate::monitor::fence::register(
                &self.shared.monitors,
                monitor_ref,
                active.clone(),
                // The token check makes registration inert once shutdown
                // starts. The closed table serializes registration with exit
                // cleanup, even on paths that unwind before cancelling the
                // owner token.
                || self.shared.cancellation_token.is_cancelled(),
                // Linearised against the target's death point (E4): past it
                // the only notice is NoProc, delivered under the observer's
                // lock.
                || target.is_past_death(),
                || {
                    crate::lifecycle_trace::life_emit(observer, || event("late"));
                    crate::lifecycle_trace::life_emit(observer, || {
                        crate::lifecycle_trace::LifeEvent::new("Deliver", observer)
                            .s(target.id().raw())
                            .m(monitor_ref.raw())
                            .x("NoProc")
                            .k("down")
                            .o("late")
                    });
                    let _ = enqueue_down(
                        &self.shared.sender,
                        &self.shared.mailbox,
                        &self.shared.replies,
                        Down {
                            monitor_ref,
                            actor: target.id(),
                            reason: ExitReason::NoProc,
                        },
                    );
                },
                || crate::lifecycle_trace::life_emit(observer, || event("active")),
            ) == crate::monitor::fence::Registration::Active
        });
        if !registered {
            return monitor_ref;
        }

        let target = target.clone();
        let actor_ref = self.actor_ref();
        let monitors = self.shared.monitors.clone();
        let registration = crate::monitor::MonitorRegistration(monitors.clone(), monitor_ref);

        self.spawn_framework(async move {
            let _registration = registration;
            let reason = match futures::future::select(
                Box::pin(active.cancelled()),
                Box::pin(target.wait_exit_async()),
            )
            .await
            {
                futures::future::Either::Left(_) => return,
                futures::future::Either::Right((reason, _)) => reason,
            };
            // Receiver-side fence (E6): the entry is removed and the Down
            // enqueued in one critical section, so once `demonitor` returns
            // no Down of this monitor is enqueued.
            crate::lifecycle_trace::life_section(observer, || {
                crate::monitor::fence::deliver(&monitors, &monitor_ref, || {
                    crate::lifecycle_trace::life_emit(observer, || {
                        crate::lifecycle_trace::LifeEvent::new("Deliver", observer)
                            .s(target.id().raw())
                            .m(monitor_ref.raw())
                            .x(crate::lifecycle_trace::life_reason(&reason))
                            .k("down")
                            .o("death")
                    });
                    let _ = enqueue_down(
                        &actor_ref.shared.sender,
                        &actor_ref.shared.mailbox,
                        &actor_ref.shared.replies,
                        Down {
                            monitor_ref,
                            actor: target.id(),
                            reason,
                        },
                    );
                });
            });
        });

        monitor_ref
    }

    /// Cancel a previously-set monitor.
    ///
    /// A fence with OTP's `flush` (`demonitor(Ref, [flush])`): once it
    /// returns, no `Down` of this monitor is enqueued or handled; one already
    /// in the mailbox is skipped. Demonitoring an unknown, delivered or
    /// already cancelled monitor does nothing.
    pub fn demonitor(&self, monitor_ref: MonitorRef) {
        let observer = self.shared.id.raw();
        let active = crate::lifecycle_trace::life_section(observer, || {
            crate::monitor::fence::demonitor(&self.shared.monitors, &monitor_ref, || {
                crate::lifecycle_trace::life_emit(observer, || {
                    crate::lifecycle_trace::LifeEvent::new("Demonitor", observer)
                        .m(monitor_ref.raw())
                });
            })
        });
        // Waking the waiter must happen after releasing the table lock.
        if let Some(active) = active {
            active.cancel();
        }
    }

    /// Set up a bidirectional link with another actor (OTP `link/1`).
    ///
    /// When either side's exit is published, the other receives a link
    /// notice with the final reason: a non-trapping peer ignores `Normal` and
    /// otherwise exits with that reason without running `stopped()`; a
    /// trapping peer gets an `Exit` via [`Actor::exit_received`].
    ///
    /// If the target is past its death point, this actor receives a `NoProc`
    /// notice: an `Exit { reason: NoProc }` when trapping, otherwise it exits
    /// `NoProc` (decision 1). Use [`try_link`](Self::try_link) to get
    /// `Err(NoProc)` instead. An existing link, or a pending notice from that
    /// peer, makes this a no-op. Inert once this actor's exit began (in
    /// `stopped()`, from a helper after the stop, or past its death point):
    /// no record and no notice, as `monitor`.
    ///
    /// [`Actor::exit_received`]: Actor::exit_received
    pub fn link(&self, target: &ChildHandle) {
        link::register_link(
            &self.shared.signal,
            &self.shared.links,
            target.signal_target(),
            target.links(),
            false,
        );
    }

    /// Link as [`link`](Self::link), but report a target past its death
    /// point as `Err(NoProc)` and keep no record, whether or not this actor
    /// traps exits.
    pub fn try_link(&self, target: &ChildHandle) -> Result<(), NoProc> {
        match link::register_link(
            &self.shared.signal,
            &self.shared.links,
            target.signal_target(),
            target.links(),
            true,
        ) {
            LinkOutcome::NoProc => Err(NoProc),
            LinkOutcome::Linked | LinkOutcome::Unchanged => Ok(()),
        }
    }

    /// Remove a link. A fence (OTP `unlink/1`): once it returns, the link has
    /// no further effect on this actor, including a notice already taken at
    /// the peer's death; an `Exit` already queued stays.
    pub fn unlink(&self, target: &ChildHandle) {
        link::unregister_link(
            self.shared.id,
            &self.shared.links,
            target.id(),
            target.links(),
        );
    }

    /// Whether this actor's exit has begun or a signal that will end it is
    /// already pending, for a trapping actor that wants to refuse late work.
    ///
    /// `true` once:
    /// - exit has begun: `ctx.stop()`, an administrative stop
    ///   ([`ChildHandle::stop`](crate::ChildHandle::stop)), a kill, an untrapped exit
    ///   signal or link notice, a panic; it stays `true` in `stopped()`;
    /// - a parent exit is queued for this trapping actor (E3: the
    ///   supervisor's or system's shutdown, or a signal from its
    ///   `start_linked` parent). The parent exit is dispatched after the
    ///   messages queued before it, and handlers of those messages see
    ///   `true`.
    ///
    /// A trapped [`Exit`](crate::Exit) from a linked peer or a non-parent
    /// `exit` is not a pending exit: `exit_received` decides. Signals that
    /// arrive while `started()` runs count once `started()` returns, when
    /// they are resolved. Without trapping, a pending signal has already
    /// begun the exit, so this is also `true`.
    ///
    /// Advisory: a signal can arrive right after it returns `false`. It
    /// takes the actor's exit-request lock; nothing is added per message.
    pub fn exit_pending(&self) -> bool {
        link::lock_request(&self.shared.linked_reason).exit_pending()
    }

    /// Control how exit signals (link notices and `exit` signals) are
    /// handled; read when each signal arrives (OTP `trap_exit`).
    ///
    /// - `false` (default): `Normal` is ignored; any other reason ends this
    ///   actor after its current handler, without `stopped()`.
    /// - `true`: the signal is queued as an `Exit` for
    ///   [`Actor::exit_received`].
    ///
    /// Only a kill is untrappable. A parent's exit (the `start_linked` parent,
    /// the supervisor or the system) ends a trapping actor normally, through
    /// `stopped()`, and never reaches `exit_received`. Set it first thing in
    /// `started()`: signals that arrive while `started()` runs are resolved
    /// when it returns. Handlers are never preempted.
    pub fn trap_exit(&self, enabled: bool) {
        let id = self.shared.id.raw();
        crate::lifecycle_trace::life_section(id, || {
            // Recorded only when it changes (the model's SetTrap).
            if self.shared.trap_exit.swap(enabled, Ordering::AcqRel) != enabled {
                crate::lifecycle_trace::life_emit(id, || {
                    crate::lifecycle_trace::LifeEvent::new("Trap", id)
                        .x(crate::lifecycle_trace::flag(enabled))
                });
            }
        });
    }

    /// Type-erased handle to this actor, without an intermediate `ActorRef`.
    pub(crate) fn child_handle(&self) -> ChildHandle {
        self.shared.child_handle()
    }

    /// Spawn a lifecycle-owned helper on this actor's assigned execution group.
    /// Construct runtime-bound work inside the async block. Closure of the scope
    /// cancels the helper and waits for its future to drop before cleanup ends.
    /// Returns [`crate::system::SystemError::UnmanagedActor`] for actors started
    /// with `.start()`; use `.start_on(&group)` to establish cleanup ownership.
    pub fn spawn_helper<F>(
        &self,
        future: F,
    ) -> Result<crate::system::ScopedTask<F::Output>, crate::system::SystemError>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.shared
            .managed
            .as_ref()
            .ok_or(crate::system::SystemError::UnmanagedActor)?
            .spawn_helper(future)
    }

    /// Register blocking work until the native closure actually finishes.
    /// Cancellation cannot preempt it. Raw nested spawn calls are not implicitly
    /// owned; use this method rather than detaching a blocking task inside a
    /// cancellable async helper when recovery must await its resource cleanup.
    /// Requires `.start_on(&group)`, like [`Self::spawn_helper`].
    pub fn spawn_blocking_helper<F, T>(
        &self,
        work: F,
    ) -> Result<crate::system::ScopedTask<T>, crate::system::SystemError>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        self.shared
            .managed
            .as_ref()
            .ok_or(crate::system::SystemError::UnmanagedActor)?
            .spawn_blocking_helper(work)
    }

    pub fn execution_group(&self) -> Option<crate::system::ExecutionGroup> {
        self.shared.managed.as_ref().map(|s| s.group())
    }

    /// Register this managed actor until its owned cleanup completes. Removal
    /// compares identity and registration generation, preserving replacements.
    pub fn register_name(
        &self,
        name: &str,
    ) -> Result<(), crate::registry::ManagedRegistrationError> {
        self.shared
            .managed
            .as_ref()
            .ok_or(crate::registry::ManagedRegistrationError::OwnerClosed)?
            .register(name, self.shared.id, self.actor_ref())
    }

    pub(crate) fn managed_scope(&self) -> Option<&Arc<crate::system::Scope>> {
        self.shared.managed.as_ref()
    }
    pub(crate) fn spawn_framework<F>(&self, future: F) -> JoinHandle<()>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        match &self.shared.managed {
            Some(scope) => scope.spawn_framework(future),
            None => rt::spawn(future),
        }
    }

    pub(crate) fn cancellation_token(&self) -> CancellationToken {
        self.shared.cancellation_token.clone()
    }
}

impl<A: Actor> crate::__private::Sealed for Context<A> {}
impl<A: Actor> crate::__private::Sealed for ActorRef<A> {}

// Bridge: Context<A> implements Receiver<M> for any M that A handles
impl<A, M> Receiver<M> for Context<A>
where
    A: Actor + Handler<M>,
    M: Message,
{
    fn send(&self, msg: M) -> Result<(), ActorError> {
        Context::send(self, msg)
    }

    fn send_request(
        &self,
        msg: M,
        deadline: Deadline,
    ) -> Result<ReplyReceiver<M::Result>, ActorError> {
        Context::send_request(self, msg, deadline)
    }

    fn send_request_timed(
        &self,
        msg: M,
        deadline: Deadline,
    ) -> Result<ReplyReceiver<M::Result>, ActorError> {
        Context::send_request_with(self, msg, deadline, false)
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
    let rx = match recipient.send_request_timed(msg, Deadline::after(timeout_duration)) {
        Ok(rx) => rx,
        Err(error) => {
            let result = Err(error);
            observation.finish(&result);
            return result;
        }
    };
    let result = rx.into_timed_future_after(timeout_duration).await;
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

impl<A: Actor> ActorRef<A> {
    /// Clone this handle with a timeout override for generated protocol calls.
    /// Per-call override wins over method and protocol defaults. No message is
    /// sent until a protocol method is called; timeout does not cancel its work.
    pub fn with_request_timeout(&self, timeout: Duration) -> crate::response::RequestOptions<Self> {
        crate::response::RequestOptions::new(self.clone(), timeout)
    }

    /// Await this managed instance's initial readiness barrier. Success does not
    /// guarantee it remains alive afterward; ordinary request errors still apply.
    pub async fn ready(&self) -> Result<(), crate::system::SystemError> {
        self.shared
            .managed
            .as_ref()
            .ok_or(crate::system::SystemError::UnmanagedActor)?
            .wait_ready()
            .await
    }
    pub(crate) fn signal_failure(&self, error: String) {
        let _ = self
            .shared
            .sender
            .send(Box::new(FailureEnvelope(Some(error))));
    }
    /// Send a fire-and-forget message to the actor.
    pub fn send<M>(&self, msg: M) -> Result<(), ActorError>
    where
        A: Handler<M>,
        M: Message,
    {
        enqueue_send(
            &self.shared.sender,
            &self.shared.mailbox,
            &self.shared.replies,
            msg,
        )
    }

    /// Send a request without waiting (`gen_server:send_request/2` with an explicit deadline) and get
    /// its [`ReplyReceiver`]. The deadline is fixed now: a `Duration` from
    /// now, an `Instant`, or [`Deadline::Never`] for an unbounded wait
    /// ([`DEFAULT_REQUEST_TIMEOUT`] is what [`request`](Self::request)
    /// uses). The handler sees it through `PendingReply::deadline`.
    pub fn send_request<M>(
        &self,
        msg: M,
        deadline: impl Into<Deadline>,
    ) -> Result<ReplyReceiver<M::Result>, ActorError>
    where
        A: Handler<M>,
        M: Message,
    {
        let deadline = deadline.into();
        self.send_request_with(msg, deadline, true)
    }

    /// `send_request`; `raw` puts a deadline in the owner's heap now (the
    /// receiver may be awaited directly). The `request` paths arm their own
    /// timer and pass `false`.
    pub(crate) fn send_request_with<M>(
        &self,
        msg: M,
        deadline: Deadline,
        raw: bool,
    ) -> Result<ReplyReceiver<M::Result>, ActorError>
    where
        A: Handler<M>,
        M: Message,
    {
        let (slot, rx) = reply_slot(deadline, Some(&self.shared.replies), raw);
        enqueue(
            &self.shared.sender,
            &self.shared.mailbox,
            &self.shared.replies,
            msg,
            slot,
        )?;
        Ok(rx)
    }

    /// Send a request and wait for the reply (default 5s timeout).
    pub async fn request<M>(&self, msg: M) -> Result<M::Result, ActorError>
    where
        A: Handler<M>,
        M: Message,
    {
        self.request_with_timeout(msg, DEFAULT_REQUEST_TIMEOUT)
            .await
    }

    /// Run `f` in one of the actor's turns, with its state and `Context`,
    /// and receive its result. For tests: `spawned_test::tasks::in_actor` (through
    /// `__private`).
    pub(crate) fn in_turn<R, F>(&self, f: F) -> Result<ReplyReceiver<R>, ActorError>
    where
        R: Send + 'static,
        F: FnOnce(&mut A, &Context<A>) -> R + Send + 'static,
    {
        let (slot, rx) = reply_slot(Deadline::Never, Some(&self.shared.replies), false);
        let envelope: Box<dyn Envelope<A> + Send> = Box::new(InTurnEnvelope {
            f: Some(Box::new(f)),
            slot,
            lent: None,
        });
        // Past the death point the actor is gone (E4), as for `enqueue`.
        if self.shared.replies.refuses_admission() {
            return Err(ActorError::NoProc);
        }
        self.shared
            .sender
            .send(envelope)
            .map_err(|_| ActorError::NoProc)?;
        Ok(rx)
    }

    /// Send a request and wait for the reply with a custom timeout.
    pub async fn request_with_timeout<M>(
        &self,
        msg: M,
        duration: Duration,
    ) -> Result<M::Result, ActorError>
    where
        A: Handler<M>,
        M: Message,
    {
        let observation = crate::instrumentation::RequestObservation::new();
        // Bind the receiver before awaiting it: awaiting inside a `match` on
        // the `Result` keeps the scrutinee's storage alive beside the awaited
        // receiver and doubles this future's size.
        let rx = match self.send_request_with(msg, Deadline::after(duration), false) {
            Ok(rx) => rx,
            Err(error) => {
                let result = Err(error);
                observation.finish(&result);
                return result;
            }
        };
        let result = rx.into_timed_future_after(duration).await;
        observation.finish(&result);
        result
    }

    /// Get a type-erased `Recipient<M>` for this actor.
    pub fn recipient<M>(&self) -> Recipient<M>
    where
        A: Handler<M>,
        M: Message,
    {
        Arc::new(self.clone())
    }

    /// Get a `Context<A>` from this ref, for timer setup or stream listeners
    /// from outside the actor.
    pub fn context(&self) -> Context<A> {
        Context::from_ref(self)
    }

    /// Wait until the actor has fully stopped (including `stopped()` callback).
    ///
    /// Returns when cleanup completes: the actor died and its cancelled jobs
    /// were joined (bounded by the cleanup deadline; jobs still running then
    /// are reported, not awaited). A `stopped()` or state destructor that
    /// never returns keeps it waiting; bound it with a timeout, or use
    /// [`wait_exit`](Self::wait_exit) to observe death alone.
    pub async fn join(&self) {
        let _ = self.wait_exit().await;
        let mut cleaned = self.shared.cleaned.clone();
        loop {
            if *cleaned.borrow_and_update() {
                return;
            }
            if cleaned.changed().await.is_err() {
                return;
            }
        }
    }

    /// Poll the exit reason. Returns `None` until the exit is published.
    /// A completion channel closed without a reason reports
    /// [`ExitReason::Killed`], matching the wait methods (for example, after
    /// runtime shutdown).
    pub fn exit_reason(&self) -> Option<ExitReason> {
        crate::child_handle::poll_exit_reason(&self.shared.completion_rx)
    }

    /// Wait until the actor's exit is published and return its final reason.
    pub async fn wait_exit(&self) -> ExitReason {
        crate::child_handle::wait_loop(self.shared.completion_rx.clone()).await
    }

    /// Waiting messages, including in-flight enqueue reservations, excluding
    /// the active handler and internal exit/shutdown control envelopes.
    /// Concurrent observations are approximate; no registry or actor labels.
    #[cfg(feature = "metrics")]
    pub fn mailbox_depth(&self) -> u64 {
        self.shared.mailbox.depth()
    }

    /// The actor's unique identity.
    pub fn id(&self) -> ActorId {
        self.shared.id
    }

    /// Get a type-erased `ChildHandle` for this actor.
    pub fn child_handle(&self) -> ChildHandle {
        self.shared.child_handle()
    }

    /// The shared state, for tests proving it is freed once every handle is gone.
    #[cfg(test)]
    fn shared_weak(&self) -> std::sync::Weak<ActorShared<A>> {
        Arc::downgrade(&self.shared)
    }
}

impl<A: Actor> ActorShared<A> {
    fn child_handle(&self) -> ChildHandle {
        ChildHandle::new(
            self.signal.clone(),
            Completion::Tasks(self.completion_rx.clone()),
            self.links.clone(),
        )
        .with_mailbox_failures(self.sender.failures())
    }
}

impl<A: Actor> From<ActorRef<A>> for ChildHandle {
    fn from(actor_ref: ActorRef<A>) -> Self {
        actor_ref.shared.child_handle()
    }
}

// Bridge: ActorRef<A> implements Receiver<M> for any M that A handles
impl<A, M> Receiver<M> for ActorRef<A>
where
    A: Actor + Handler<M>,
    M: Message,
{
    fn send(&self, msg: M) -> Result<(), ActorError> {
        ActorRef::send(self, msg)
    }

    fn send_request(
        &self,
        msg: M,
        deadline: Deadline,
    ) -> Result<ReplyReceiver<M::Result>, ActorError> {
        ActorRef::send_request(self, msg, deadline)
    }

    fn send_request_timed(
        &self,
        msg: M,
        deadline: Deadline,
    ) -> Result<ReplyReceiver<M::Result>, ActorError> {
        ActorRef::send_request_with(self, msg, deadline, false)
    }
}

// ---------------------------------------------------------------------------
// Actor startup + main loop
// ---------------------------------------------------------------------------

impl<A: Actor> ActorRef<A> {
    fn spawn(actor: A, backend: Backend, parent: Option<ChildHandle>) -> Self {
        Self::spawn_inner(actor, backend, parent, None, None)
            .expect("unmanaged actor dispatch failed")
    }

    pub(crate) fn spawn_managed(
        actor: A,
        scope: Arc<crate::system::Scope>,
        lease: crate::system::Lease,
    ) -> Result<Self, crate::system::SystemError> {
        if !scope.can_start() {
            return Err(crate::system::SystemError::Closed);
        }
        Self::spawn_inner(actor, Backend::Async, None, Some(scope), Some(lease))
    }

    fn spawn_inner(
        actor: A,
        backend: Backend,
        parent: Option<ChildHandle>,
        managed: Option<Arc<crate::system::Scope>>,
        lease: Option<crate::system::Lease>,
    ) -> Result<Self, crate::system::SystemError> {
        let (tx, rx) = mpsc::channel::<Box<dyn Envelope<A> + Send>>();
        let cancellation_token = managed
            .as_ref()
            .map(|s| s.token.clone())
            .unwrap_or_default();
        let (completion_tx, completion_rx) = watch::channel(None);
        let monitors: MonitorTable = Arc::new(crate::monitor::fence::open());
        let trap_exit = new_trap_exit_flag();
        let links = new_link_table();
        let linked_reason = managed
            .as_ref()
            .map(|s| s.cause.clone())
            .unwrap_or_else(new_linked_exit_reason);

        let id = ActorId::next();
        // Phase 0: signals that arrive before started() returns are held.
        link::lock_request(&linked_reason).begin_starting();
        let signal = signal_target::<A>(
            id,
            parent.as_ref().map(ChildHandle::id),
            &tx,
            trap_exit.clone(),
            linked_reason.clone(),
            cancellation_token.clone(),
        );
        let (cleaned_tx, cleaned_rx) = watch::channel(false);
        // A supervised child's `ChildSpec` may override the actor's bounds.
        let (max_outstanding, max_native) = managed
            .as_ref()
            .map(|scope| scope.bounds())
            .unwrap_or_default()
            .resolve(A::MAX_OUTSTANDING, A::MAX_NATIVE_JOBS);
        let actor_ref = ActorRef {
            shared: Arc::new(ActorShared {
                id,
                replies: Registry::new(id, max_outstanding),
                jobs: Arc::new(crate::reply::jobs::JobSet::new(id, max_native)),
                cleaned: cleaned_rx,
                sender: tx,
                cancellation_token: cancellation_token.clone(),
                completion_rx,
                monitors,
                trap_exit,
                links: links.clone(),
                linked_reason: linked_reason.clone(),
                signal,
                mailbox: crate::instrumentation::Mailbox::default(),
                managed: managed.clone(),
            }),
        };
        let ctx = actor_ref.context();

        if let Some(scope) = &managed {
            scope.attach::<A>(actor_ref.child_handle());
            scope.set_jobs(Arc::downgrade(&actor_ref.shared.jobs));
        }

        let parent_id = parent.as_ref().map_or(0, |p| p.id().raw());
        if let Some(parent) = parent {
            ctx.link(&parent);
        }
        // Recorded from here: the start_linked link is the model's initial one.
        crate::lifecycle_trace::life_spawn(id.raw(), parent_id);

        #[cfg(feature = "backend-experiments")]
        let scheduler = if managed.is_some() {
            String::new()
        } else {
            std::env::var("SPAWNED_EXPERIMENT_SCHEDULER").unwrap_or_default()
        };
        #[cfg(feature = "backend-experiments")]
        let turn = (backend == Backend::Async
            && matches!(
                scheduler.as_str(),
                "sharded"
                    | "tokio-batch"
                    | "native-global"
                    | "native-local"
                    | "native-steal"
                    | "native-root"
                    | "native-runnable"
                    | "native-deque"
            ))
        .then(MessageTurn::from_env);
        #[cfg(feature = "backend-experiments")]
        let borrowed_runtime = if backend != Backend::Async {
            rt::experimental::lease_if_initialized()
        } else {
            None
        };
        #[cfg(feature = "backend-experiments")]
        let borrowed_dispatcher = if backend != Backend::Async {
            rt::experimental::dispatcher::lease_if_initialized()
        } else {
            None
        };
        let cleanup_token = cancellation_token.clone();
        let cleanup = crate::actor_cleanup::ActorCleanup::new(
            actor_ref.shared.id,
            links,
            linked_reason,
            actor_ref.shared.monitors.clone(),
            completion_tx,
            move || cleanup_token.cancel(),
        )
        .with_mailbox_failures(actor_ref.shared.sender.failures())
        .with_replies(actor_ref.shared.replies.clone())
        .with_jobs(actor_ref.shared.jobs.clone());
        let startup = crate::actor_cleanup::ActorStartup::new(
            (actor, ctx, rx, cancellation_token),
            cleanup,
            discharge_startup::<A>,
        );
        let job_owner = actor_ref.shared.clone();
        let cleaned_tx = cleaned_tx;
        // Destruction-safe: if the runtime drops this future before it
        // completes, `ActorStartup` (never polled) or `ExitOnDrop` (polled)
        // still reach D before destroying anything, so no caller waits for an
        // instance that can never answer.
        let inner_future = async move {
            let ((actor, ctx, rx, cancellation_token), mut cleanup) = startup.into_parts();
            #[cfg(feature = "backend-experiments")]
            let _borrowed_runtime = borrowed_runtime;
            #[cfg(feature = "backend-experiments")]
            let _borrowed_dispatcher = borrowed_dispatcher;
            run_actor(
                actor,
                ctx,
                rx,
                cancellation_token,
                &mut cleanup,
                #[cfg(feature = "backend-experiments")]
                turn,
            )
            .await;
            let reason = cleanup.die();
            if let Some(scope) = &managed {
                scope.exited(reason);
            }
            // Publication (wait_exit, monitors, link notices) after the
            // state drop, before the job join.
            drop(cleanup);
            // Job cleanup (deferred-replies §5 step 4), after death: join the
            // cancelled jobs, bounded by the cleanup deadline; a job still
            // running then does not undo death and is reported unfinished.
            if job_owner.jobs.running() > 0 {
                let timeout = job_owner
                    .managed
                    .as_ref()
                    .map_or(DEFAULT_STOP_DEADLINE, |scope| scope.cleanup_timeout());
                let remaining = job_owner.replies.cleanup_remaining(timeout);
                let unfinished = job_owner.jobs.join(remaining).await;
                crate::reply::jobs::report_unfinished(job_owner.id, unfinished);
            }
            drop(job_owner);
            let _ = cleaned_tx.send(true);
        };

        if let Some(lease) = lease {
            let scope = actor_ref
                .shared
                .managed
                .as_ref()
                .expect("managed lease without scope");
            scope.dispatch_actor(crate::system::Tracked {
                future: inner_future,
                lease,
            })?;
            return Ok(actor_ref);
        }

        match backend {
            Backend::Async => {
                #[cfg(debug_assertions)]
                let inner_future = warn_on_block::WarnOnBlocking::new(inner_future);
                #[cfg(feature = "backend-experiments")]
                if let Some(policy) = rt::experimental::dispatcher::policy_for_scheduler(&scheduler)
                {
                    use rt::experimental::dispatcher;
                    if let Err(error) = dispatcher::spawn(policy, inner_future) {
                        actor_ref.shared.cancellation_token.cancel();
                        drop(MonitorCleanup(actor_ref.shared.monitors.clone()));
                        tracing::error!("Experimental executor startup failed: {error}");
                    }
                    return Ok(actor_ref);
                }
                #[cfg(feature = "backend-experiments")]
                if scheduler == "sharded" {
                    if let Err(error) = rt::experimental::spawn_sharded(inner_future) {
                        actor_ref.shared.cancellation_token.cancel();
                        drop(MonitorCleanup(actor_ref.shared.monitors.clone()));
                        tracing::error!("Experimental shard startup failed: {error}");
                    }
                    return Ok(actor_ref);
                }
                let _handle = rt::spawn(inner_future);
            }
            Backend::Blocking => {
                let _handle = rt::spawn_blocking(move || rt::block_on(inner_future));
            }
            Backend::Thread => {
                #[cfg(not(feature = "backend-experiments"))]
                let _handle = threads::spawn(move || threads::block_on(inner_future));
                #[cfg(feature = "backend-experiments")]
                {
                    // Experiment-only selection; the default path is unchanged.
                    let mode = std::env::var("SPAWNED_EXPERIMENT_RUNTIME").unwrap_or_default();
                    let shared = rt::Handle::try_current().ok();
                    let _handle = threads::spawn(move || {
                        if mode == "shared" {
                            shared
                                .expect("shared experiment needs a caller runtime")
                                .block_on(inner_future)
                        } else {
                            threads::block_on(inner_future)
                        }
                    });
                }
            }
        }

        Ok(actor_ref)
    }
}

// Batch only between completed handlers: never preempt or concurrently poll an actor.
#[cfg(feature = "backend-experiments")]
struct MessageTurn {
    limit: usize,
    adaptive: bool,
    slice: Option<Duration>,
    completed: usize,
    began: Option<std::time::Instant>,
}
#[cfg(feature = "backend-experiments")]
impl MessageTurn {
    fn from_env() -> Self {
        let batch: usize = std::env::var("SPAWNED_EXPERIMENT_BATCH")
            .unwrap_or_else(|_| "16".into())
            .parse()
            .expect("SPAWNED_EXPERIMENT_BATCH must be an integer");
        assert!(batch <= 64, "experimental batch: 0 (adaptive) or 1..=64");
        let slice_us: u64 = std::env::var("SPAWNED_EXPERIMENT_SLICE_US")
            .unwrap_or_else(|_| "50".into())
            .parse()
            .expect("experimental slice microseconds");
        assert!(slice_us <= 1_000_000);
        Self {
            slice: (slice_us != 0).then_some(Duration::from_micros(slice_us)),
            limit: batch.max(1),
            adaptive: batch == 0,
            completed: 0,
            began: None,
        }
    }
    fn parked(&mut self) {
        self.completed = 0;
        if self.adaptive {
            self.limit = 1;
        }
        self.began = None;
    }
    fn resumed(&mut self) {
        if self.slice.is_some() {
            self.began.get_or_insert_with(std::time::Instant::now);
        }
    }
    async fn after_message(&mut self, empty: bool) {
        if empty {
            self.parked();
            return;
        }
        self.completed += 1;
        if self.completed >= self.limit
            || self
                .began
                .zip(self.slice)
                .is_some_and(|(t, slice)| t.elapsed() >= slice)
        {
            if self.adaptive {
                self.limit = if empty { 1 } else { (self.limit * 2).min(64) };
            }
            self.completed = 0;
            rt::experimental::yield_now().await;
            self.began = self.slice.map(|_| std::time::Instant::now());
        }
    }
}

/// A pending user future of the lifecycle: `started()`, a turn (handler,
/// `exit_received`, a job's `done`) or `stopped()`. If the runtime drops the
/// lifecycle while it is pending (G1), its `Drop` reaches the death point
/// first: the turn's own caller, every deferred token and every queued
/// request are answered and woken before this future and its captures (the
/// message, the handler's locals) are destroyed. A destructor panic of the
/// future is a cleanup failure.
struct DeathFirst<'a, A: Actor, C: Fn()> {
    inner: Option<Pin<Box<dyn Future<Output = ()> + Send + 'a>>>,
    rx: &'a mut mpsc::Receiver<Box<dyn Envelope<A> + Send>>,
    cleanup: &'a mut crate::actor_cleanup::ActorCleanup<C>,
    caller: Option<&'a dyn Caller>,
    /// The run loop already polled `inner` once in this task poll (it was
    /// pending): the next poll returns `Pending` without polling it again.
    polled: bool,
}
impl<'a, A: Actor, C: Fn()> DeathFirst<'a, A, C> {
    fn new(
        inner: Pin<Box<dyn Future<Output = ()> + Send + 'a>>,
        rx: &'a mut mpsc::Receiver<Box<dyn Envelope<A> + Send>>,
        cleanup: &'a mut crate::actor_cleanup::ActorCleanup<C>,
        caller: Option<&'a dyn Caller>,
    ) -> Self {
        Self {
            inner: Some(inner),
            rx,
            cleanup,
            caller,
            polled: false,
        }
    }

    /// A turn the run loop polled once and found pending.
    fn pending(
        inner: Pin<Box<dyn Future<Output = ()> + Send + 'a>>,
        rx: &'a mut mpsc::Receiver<Box<dyn Envelope<A> + Send>>,
        cleanup: &'a mut crate::actor_cleanup::ActorCleanup<C>,
        caller: Option<&'a dyn Caller>,
    ) -> Self {
        let mut turn = Self::new(inner, rx, cleanup, caller);
        turn.polled = true;
        turn
    }
}

/// A boxed turn of the lifecycle.
type TurnFuture<'a> = Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

/// Poll `turn` once, catching its panic.
#[inline]
fn poll_caught(
    turn: &mut TurnFuture<'_>,
    cx: &mut std::task::Context<'_>,
) -> Result<std::task::Poll<()>, Panic> {
    std::panic::catch_unwind(AssertUnwindSafe(|| turn.as_mut().poll(cx)))
}

/// Drop a turn that panicked; a panic of its destructor is disposed of.
fn drop_panicked(turn: Option<TurnFuture<'_>>) {
    if let Err(secondary) = std::panic::catch_unwind(AssertUnwindSafe(|| drop(turn))) {
        crate::mailbox::dispose_panic(secondary);
    }
}
/// A caught panic payload.
type Panic = Box<dyn std::any::Any + Send>;

impl<A: Actor, C: Fn()> Future for DeathFirst<'_, A, C> {
    /// The future's panic, caught: the turn's (or hook's) panic, not a
    /// runtime drop. The future is gone before it is returned.
    type Output = Result<(), Panic>;
    fn poll(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let this = self.get_mut();
        if std::mem::take(&mut this.polled) {
            // Polled (and its waker registered) in this same task poll.
            return std::task::Poll::Pending;
        }
        let inner = this.inner.as_mut().expect("polled after completion");
        match poll_caught(inner, cx) {
            Ok(std::task::Poll::Pending) => std::task::Poll::Pending,
            Ok(std::task::Poll::Ready(())) => {
                this.inner = None;
                std::task::Poll::Ready(Ok(()))
            }
            Err(panic) => {
                drop_panicked(this.inner.take());
                std::task::Poll::Ready(Err(panic))
            }
        }
    }
}
impl<A: Actor, C: Fn()> Drop for DeathFirst<'_, A, C> {
    fn drop(&mut self) {
        let Some(inner) = self.inner.take() else {
            return;
        };
        // Destroyed while pending: the runtime dropped the actor.
        self.cleanup.die_dropped();
        if let Some(caller) = self.caller.take() {
            self.cleanup.answer_turn(caller.retain());
        }
        let queued = answer_mailbox(self.rx, self.cleanup.replies());
        self.cleanup.discharged();
        self.rx.discard_all(queued);
        if let Err(panic) = std::panic::catch_unwind(AssertUnwindSafe(|| drop(inner))) {
            let message = panic_message(&*panic);
            crate::mailbox::dispose_panic(panic);
            self.cleanup
                .cleanup_failed(format!("dropped turn destructor: {message}"));
        }
    }
}

/// The actor's state and mailbox while its lifecycle runs. They are
/// destroyed only after the death point D: normally at the end of
/// [`run_actor`]; if the lifecycle future is dropped mid-way (its runtime was
/// destroyed), here, which reaches D first (`Killed` unless a reason was
/// requested), so tokens and queued requests answer `ActorStopped` before
/// any destructor runs.
struct ExitOnDrop<'c, A: Actor, C: Fn()> {
    actor: Option<A>,
    rx: mpsc::Receiver<Box<dyn Envelope<A> + Send>>,
    cleanup: &'c mut crate::actor_cleanup::ActorCleanup<C>,
}
impl<A: Actor, C: Fn()> ExitOnDrop<'_, A, C> {
    /// D, then the queued requests, then destruction (mailbox, then state).
    fn finish(&mut self, dropped: bool) {
        let Some(actor) = self.actor.take() else {
            return;
        };
        if dropped {
            self.cleanup.die_dropped();
        } else {
            self.cleanup.die();
        }
        let queued = answer_mailbox(&mut self.rx, self.cleanup.replies());
        // Every caller is answered: wake them, then destroy (queued
        // payloads, then state). A destructor panic is a cleanup failure,
        // never the reason.
        self.cleanup.discharged();
        self.rx.discard_all(queued);
        if let Some(failure) = crate::actor_cleanup::finish_actor_state(actor) {
            self.cleanup.cleanup_failed(failure);
        }
    }
}
impl<A: Actor, C: Fn()> Drop for ExitOnDrop<'_, A, C> {
    fn drop(&mut self) {
        self.finish(true);
    }
}

async fn run_actor<A: Actor, C: Fn()>(
    actor: A,
    ctx: Context<A>,
    rx: mpsc::Receiver<Box<dyn Envelope<A> + Send>>,
    cancellation_token: CancellationToken,
    cleanup: &mut crate::actor_cleanup::ActorCleanup<C>,
    #[cfg(feature = "backend-experiments")] turn: Option<MessageTurn>,
) {
    let request = ctx.shared.linked_reason.clone();
    let mut owned = ExitOnDrop {
        actor: Some(actor),
        rx,
        cleanup,
    };
    if let Err(payload) = AssertUnwindSafe(run_actor_lifecycle(
        owned.actor.as_mut().expect("the state lives until death"),
        ctx,
        &mut owned.rx,
        &mut *owned.cleanup,
        cancellation_token,
        #[cfg(feature = "backend-experiments")]
        turn,
    ))
    .catch_unwind()
    .await
    {
        let message = panic_message(&*payload);
        crate::mailbox::dispose_panic(payload);
        link::lock_request(&request).panic(ExitReason::Panic(format!(
            "actor lifecycle unwound: {message}"
        )));
    }
    // The death point D, only now that `stopped()` returned (or was
    // skipped): the reason becomes final, the link table closes, job
    // admission and the registry close (every open token is answered), then
    // the mailbox refuses admission and its queued requests are answered,
    // all before any destructor. Early returns (startup panic, closed scope)
    // and an unwound lifecycle also come here, so jobs never outlive their
    // owner.
    owned.finish(false);
}

async fn run_actor_lifecycle<A: Actor, C: Fn()>(
    actor: &mut A,
    ctx: Context<A>,
    rx: &mut mpsc::Receiver<Box<dyn Envelope<A> + Send>>,
    cleanup: &mut crate::actor_cleanup::ActorCleanup<C>,
    cancellation_token: CancellationToken,
    #[cfg(feature = "backend-experiments")] mut turn: Option<MessageTurn>,
) {
    let _monitor_cleanup = MonitorCleanup(ctx.shared.monitors.clone());
    let request = ctx.shared.linked_reason.clone();
    let trace_id = ctx.shared.id.raw();
    if let Some(scope) = &ctx.shared.managed {
        if !scope.can_start() {
            scope.stop(ExitReason::Shutdown);
            return;
        }
    }
    let start_result = AssertUnwindSafe(async {
        fail_point!("actor.before_started", async);
        DeathFirst::new(
            crate::lifecycle_trace::in_actor(ctx.shared.id.raw(), Box::pin(actor.started(&ctx))),
            &mut *rx,
            &mut *cleanup,
            None,
        )
        .await
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
    })
    .catch_unwind()
    .await;
    if let Err(panic) = start_result {
        let msg = panic_message(&*panic);
        crate::actor_cleanup::log_safely(|| {
            tracing::error!("Panic in started() callback: {msg}");
        });
        // No stopped() after a failed start (L5).
        crate::lifecycle_trace::life_section(trace_id, || {
            crate::lifecycle_trace::life_emit(trace_id, || {
                crate::lifecycle_trace::LifeEvent::new("Panic", trace_id).x("started")
            });
            link::lock_request(&request)
                .panic(ExitReason::Panic(format!("panic in started(): {msg}")));
        });
        cancellation_token.cancel();
        return;
    }
    // Phase 0 ends: resolve signals held during started() with the trap
    // flag read now.
    if link::lock_request(&request).end_starting(ctx.shared.trap_exit.load(Ordering::Acquire)) {
        cancellation_token.cancel();
    }

    fail_point!("actor.after_started_before_ready", async);
    if let Some(scope) = &ctx.shared.managed {
        if !scope.mark_ready() {
            scope.stop(ExitReason::Shutdown);
        }
    }

    // One cancellation future for the whole loop: polled only when the
    // mailbox has nothing, so a busy actor registers no waiter per message.
    let mut cancelled = pin!(cancellation_token.cancelled());
    // The receive checks the token before taking each message (and returns
    // `None` once it is cancelled), so the loop needs no check of its own:
    // `is_cancelled` locks the token's node.
    loop {
        let msg = {
            let recv = pin!(future::poll_fn(|cx| {
                // Stop and closed-mailbox both take the normal lifecycle exit,
                // just like the cancellation arm below. Check before consuming
                // work, including when receive resumes from a pending poll.
                // The check, the receive and its trace event are one step
                // of the lifecycle trace: a recorded stop is seen here.
                let result = crate::lifecycle_trace::life_section(trace_id, || {
                    if cancellation_token.is_cancelled() {
                        return std::task::Poll::Ready(None);
                    }
                    let result = rx.poll_recv(cx);
                    #[cfg(feature = "lifecycle-trace")]
                    if let std::task::Poll::Ready(Some(envelope)) = &result {
                        crate::lifecycle_trace::life_emit(trace_id, || {
                            crate::lifecycle_trace::LifeEvent::new("Dispatch", trace_id)
                                .k(envelope.trace_kind())
                        });
                    }
                    result
                });
                #[cfg(feature = "backend-experiments")]
                if let Some(turn) = &mut turn {
                    if result.is_pending() {
                        turn.parked();
                    } else if matches!(result, std::task::Poll::Ready(Some(_))) {
                        turn.resumed();
                    }
                }
                result
            }));
            let mut recv = recv;
            future::poll_fn(|cx| match recv.as_mut().poll(cx) {
                std::task::Poll::Pending => match cancelled.as_mut().poll(cx) {
                    std::task::Poll::Ready(()) => std::task::Poll::Ready(None),
                    std::task::Poll::Pending => std::task::Poll::Pending,
                },
                ready => ready,
            })
            .await
        };
        match msg {
            Some(envelope) => {
                // Simulator turn budget (rank 8b); compiled only under the cfg.
                #[cfg(spawned_sim)]
                let _sim_turn = rt::sim::turn(std::any::type_name::<A>(), envelope.message_type());
                #[cfg(feature = "diagnostics")]
                let _turn = ctx
                    .shared
                    .managed
                    .as_ref()
                    .map(|scope| scope.begin_turn(envelope.message_type()));
                #[cfg(feature = "metrics")]
                let mut envelope = envelope;

                #[cfg(feature = "metrics")]
                let observation = envelope
                    .observation()
                    .map(|observation| observation.begin());
                ctx.shared.replies.begin_turn();
                // The handler's future is built inside the guarded future: a
                // manual `Handler` may panic before it returns one, and that
                // is a handler panic like any other (threads mode catches the
                // same call). The envelope outlives the turn and lends it the
                // request's slot.
                let mut envelope = envelope;
                let result = match dispatch(&mut *envelope, &mut *actor, &ctx) {
                    Ok((turn, caller)) => {
                        let mut turn =
                            Some(crate::lifecycle_trace::in_actor(ctx.shared.id.raw(), turn));
                        // Most turns finish in their first poll, which runs
                        // here, with no await point before it returns: only
                        // a turn that stays pending is guarded by
                        // `DeathFirst` against the runtime dropping it.
                        let first = future::poll_fn(|cx| {
                            std::task::Poll::Ready(poll_caught(
                                turn.as_mut().expect("polled once"),
                                cx,
                            ))
                        })
                        .await;
                        match first {
                            Ok(std::task::Poll::Ready(())) => {
                                drop(turn);
                                Ok(())
                            }
                            Ok(std::task::Poll::Pending) => {
                                let turn = turn.take().expect("pending turn");
                                DeathFirst::pending(turn, &mut *rx, &mut *cleanup, caller).await
                            }
                            Err(panic) => {
                                drop_panicked(turn.take());
                                Err(panic)
                            }
                        }
                    }
                    Err(panic) => Err(panic),
                };
                ctx.shared.replies.end_turn(result.is_err());
                #[cfg(feature = "diagnostics")]
                drop(_turn);
                #[cfg(feature = "metrics")]
                if let Some(observation) = observation {
                    observation.finish(result.is_err());
                }
                if let Err(panic) = result {
                    let msg = panic_message(&*panic);
                    crate::actor_cleanup::log_safely(|| {
                        tracing::error!("Panic in message handler: {msg}");
                    });
                    // A panic begins exit, or replaces the requested reason
                    // unless a kill was received.
                    crate::lifecycle_trace::life_section(trace_id, || {
                        crate::lifecycle_trace::life_emit(trace_id, || {
                            crate::lifecycle_trace::LifeEvent::new("Panic", trace_id).x("turn")
                        });
                        link::lock_request(&request)
                            .panic(ExitReason::Panic(format!("panic in handler: {msg}")));
                    });
                    break;
                }
                #[cfg(feature = "backend-experiments")]
                if let Some(turn) = &mut turn {
                    // Do not yield on the way out of an already-stopped
                    // handler. After a yield, the receive guard checks again.
                    if cancellation_token.is_cancelled() {
                        break;
                    }
                    turn.after_message(rx.is_empty()).await;
                }
            }
            None => break,
        }
    }

    if let Some(scope) = &ctx.shared.managed {
        scope.end_running();
    }
    cancellation_token.cancel();
    ctx.shared.replies.exiting();
    // Payload drops are held until every caller is answered (DropFailures).
    ctx.shared.sender.failures().hold();
    ctx.shared.jobs.cancel_all();
    // stopped() is skipped when a kill, an untrapped exit signal or link
    // notice began the exit (decision 2); one that already began finishes.
    let (run_hook, skipped) = crate::lifecycle_trace::life_section(trace_id, || {
        let mut req = link::lock_request(&request);
        let run = req.begin_hook();
        let skipped = (!run).then(|| (req.skip_cause(), req.reason().cloned()));
        drop(req);
        crate::lifecycle_trace::life_emit(trace_id, || {
            crate::lifecycle_trace::LifeEvent::new("Hook", trace_id).x(if run {
                "run"
            } else {
                "skip"
            })
        });
        (run, skipped)
    });
    if let Some((cause, reason)) = skipped {
        crate::actor_cleanup::log_stopped_skipped(
            ctx.shared.id,
            std::any::type_name::<A>(),
            cause,
            reason.as_ref(),
        );
    }
    if !run_hook {
        return;
    }
    let stop_result = AssertUnwindSafe(async {
        fail_point!("actor.before_stopped", async);
        DeathFirst::new(
            crate::lifecycle_trace::in_actor(ctx.shared.id.raw(), Box::pin(actor.stopped(&ctx))),
            &mut *rx,
            &mut *cleanup,
            None,
        )
        .await
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
    })
    .catch_unwind()
    .await;
    if let Err(panic) = stop_result {
        let msg = panic_message(&*panic);
        crate::actor_cleanup::log_safely(|| {
            tracing::error!("Panic in stopped() callback: {msg}");
        });
        crate::lifecycle_trace::life_section(trace_id, || {
            crate::lifecycle_trace::life_emit(trace_id, || {
                crate::lifecycle_trace::LifeEvent::new("Panic", trace_id).x("stopped")
            });
            link::lock_request(&request)
                .panic(ExitReason::Panic(format!("panic in stopped(): {msg}")));
        });
    }
}

/// A context and its cleanup without spawning, for run-loop unit tests.
#[cfg(test)]
fn test_context<A: Actor>(
    tx: &mpsc::Sender<Box<dyn Envelope<A> + Send>>,
    token: &CancellationToken,
) -> (Context<A>, crate::actor_cleanup::ActorCleanup<fn()>) {
    let id = ActorId::next();
    let (completion_tx, completion_rx) = watch::channel(None);
    let trap = new_trap_exit_flag();
    let links = new_link_table();
    let request = new_linked_exit_reason();
    let monitors: MonitorTable = Arc::new(crate::monitor::fence::open());
    let signal = signal_target::<A>(id, None, tx, trap.clone(), request.clone(), token.clone());
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
            managed: None,
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
// Actor::start
// ---------------------------------------------------------------------------

/// Extension trait for starting an actor. Automatically implemented for all [`Actor`] types.
pub trait ActorStart: Actor {
    /// Start the actor with the default backend ([`Backend::Async`]).
    fn start(self) -> ActorRef<Self> {
        self.start_with_backend(Backend::default())
    }

    /// Admit this actor to an owned parked async execution group.
    fn start_on(
        self,
        group: &crate::system::ExecutionGroup,
    ) -> Result<ActorRef<Self>, crate::system::SystemError> {
        group.start(self)
    }

    /// Start the actor with a specific [`Backend`].
    fn start_with_backend(self, backend: Backend) -> ActorRef<Self> {
        ActorRef::spawn(self, backend, None)
    }

    /// Start linked to `parent_ctx`'s actor, which becomes this instance's
    /// parent (E3). The link is registered before `started()`. The parent's
    /// exit ends a trapping child normally (its `stopped()` runs; it never
    /// reaches `exit_received`), a non-trapping child as any link notice. A
    /// parent already past its death point gives the child a `NoProc` parent
    /// exit.
    ///
    /// The child is always an unmanaged `Backend::Async` actor, even when
    /// the parent is managed: it is outside the parent's execution group and
    /// its ordered shutdown (`spawn_helper` on it returns `UnmanagedActor`).
    /// The unified start API (planned for 0.9) will inherit the placement.
    fn start_linked<P: Actor>(self, parent_ctx: &Context<P>) -> ActorRef<Self> {
        ActorRef::spawn(self, Backend::Async, Some(parent_ctx.child_handle()))
    }
}

impl<A: Actor> ActorStart for A {}

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
            #[cfg(not(feature = "backend-experiments"))]
            let task_id = rt::task_id();
            #[cfg(feature = "backend-experiments")]
            let task_id = rt::experimental::try_task_id();
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
mod tests {
    use super::*;
    use crate::message::Message;
    use std::{
        sync::{atomic, Arc},
        thread,
        time::Duration,
    };

    // --- Counter actor for basic tests ---

    struct Counter {
        count: u64,
    }

    struct GetCount;
    impl Message for GetCount {
        type Result = u64;
    }

    struct Increment;
    impl Message for Increment {
        type Result = u64;
    }

    struct StopCounter;
    impl Message for StopCounter {
        type Result = u64;
    }

    impl Actor for Counter {}

    impl Handler<GetCount> for Counter {
        async fn handle(
            &mut self,
            _msg: GetCount,
            reply: crate::reply::PendingReply<'_, u64>,
            _ctx: &Context<Self>,
        ) {
            reply.reply(self.count);
        }
    }

    impl Handler<Increment> for Counter {
        async fn handle(
            &mut self,
            _msg: Increment,
            reply: crate::reply::PendingReply<'_, u64>,
            _ctx: &Context<Self>,
        ) {
            let __reply_value: u64 = {
                self.count += 1;
                self.count
            };
            reply.reply(__reply_value);
        }
    }

    impl Handler<StopCounter> for Counter {
        async fn handle(
            &mut self,
            _msg: StopCounter,
            reply: crate::reply::PendingReply<'_, u64>,
            ctx: &Context<Self>,
        ) {
            let __reply_value: u64 = {
                ctx.stop();
                self.count
            };
            reply.reply(__reply_value);
        }
    }

    #[test]
    pub fn backend_default_is_async() {
        assert_eq!(Backend::default(), Backend::Async);
    }

    #[test]
    #[allow(clippy::clone_on_copy)]
    pub fn backend_enum_is_copy_and_clone() {
        let backend = Backend::Async;
        let copied = backend;
        let cloned = backend.clone();
        assert_eq!(backend, copied);
        assert_eq!(backend, cloned);
    }

    #[test]
    pub fn backend_enum_debug_format() {
        assert_eq!(format!("{:?}", Backend::Async), "Async");
        assert_eq!(format!("{:?}", Backend::Blocking), "Blocking");
        assert_eq!(format!("{:?}", Backend::Thread), "Thread");
    }

    #[test]
    pub fn backend_enum_equality() {
        assert_eq!(Backend::Async, Backend::Async);
        assert_eq!(Backend::Blocking, Backend::Blocking);
        assert_eq!(Backend::Thread, Backend::Thread);
        assert_ne!(Backend::Async, Backend::Blocking);
        assert_ne!(Backend::Async, Backend::Thread);
        assert_ne!(Backend::Blocking, Backend::Thread);
    }

    #[test]
    pub fn backend_async_handles_send_and_request() {
        crate::test_wait::bounded(
            concat!(module_path!(), "::backend_async_handles_send_and_request"),
            move || {
                let runtime = rt::Runtime::new().unwrap();
                runtime.block_on(async move {
                    let counter = Counter { count: 0 }.start();

                    let result = counter.request(GetCount).await.unwrap();
                    assert_eq!(result, 0);

                    let result = counter.request(Increment).await.unwrap();
                    assert_eq!(result, 1);

                    // fire-and-forget send; the request below shares its mailbox, so
                    // it is handled after the send without any extra wait.
                    counter.send(Increment).unwrap();

                    let result = counter.request(GetCount).await.unwrap();
                    assert_eq!(result, 2);

                    let final_count = counter.request(StopCounter).await.unwrap();
                    assert_eq!(final_count, 2);
                });
            },
        );
    }

    #[test]
    pub fn backend_blocking_handles_send_and_request() {
        crate::test_wait::bounded(
            concat!(
                module_path!(),
                "::backend_blocking_handles_send_and_request"
            ),
            move || {
                let runtime = rt::Runtime::new().unwrap();
                runtime.block_on(async move {
                    let counter = Counter { count: 0 }.start_with_backend(Backend::Blocking);

                    let result = counter.request(GetCount).await.unwrap();
                    assert_eq!(result, 0);

                    let result = counter.request(Increment).await.unwrap();
                    assert_eq!(result, 1);

                    counter.send(Increment).unwrap();

                    let result = counter.request(GetCount).await.unwrap();
                    assert_eq!(result, 2);

                    let final_count = counter.request(StopCounter).await.unwrap();
                    assert_eq!(final_count, 2);
                });
            },
        );
    }

    #[test]
    pub fn backend_thread_handles_send_and_request() {
        crate::test_wait::bounded(
            concat!(module_path!(), "::backend_thread_handles_send_and_request"),
            move || {
                let runtime = rt::Runtime::new().unwrap();
                runtime.block_on(async move {
                    let counter = Counter { count: 0 }.start_with_backend(Backend::Thread);

                    let result = counter.request(GetCount).await.unwrap();
                    assert_eq!(result, 0);

                    let result = counter.request(Increment).await.unwrap();
                    assert_eq!(result, 1);

                    counter.send(Increment).unwrap();

                    let result = counter.request(GetCount).await.unwrap();
                    assert_eq!(result, 2);

                    let final_count = counter.request(StopCounter).await.unwrap();
                    assert_eq!(final_count, 2);
                });
            },
        );
    }

    #[test]
    pub fn multiple_backends_concurrent() {
        crate::test_wait::bounded(
            concat!(module_path!(), "::multiple_backends_concurrent"),
            move || {
                let runtime = rt::Runtime::new().unwrap();
                runtime.block_on(async move {
                    let async_counter = Counter { count: 0 }.start();
                    let blocking_counter =
                        Counter { count: 100 }.start_with_backend(Backend::Blocking);
                    let thread_counter = Counter { count: 200 }.start_with_backend(Backend::Thread);

                    async_counter.request(Increment).await.unwrap();
                    blocking_counter.request(Increment).await.unwrap();
                    thread_counter.request(Increment).await.unwrap();

                    let async_val = async_counter.request(GetCount).await.unwrap();
                    let blocking_val = blocking_counter.request(GetCount).await.unwrap();
                    let thread_val = thread_counter.request(GetCount).await.unwrap();

                    assert_eq!(async_val, 1);
                    assert_eq!(blocking_val, 101);
                    assert_eq!(thread_val, 201);

                    async_counter.request(StopCounter).await.unwrap();
                    blocking_counter.request(StopCounter).await.unwrap();
                    thread_counter.request(StopCounter).await.unwrap();
                });
            },
        );
    }

    #[test]
    pub fn request_timeout() {
        crate::test_wait::bounded(concat!(module_path!(), "::request_timeout"), move || {
            let runtime = rt::Runtime::new().unwrap();
            runtime.block_on(async move {
                struct SlowActor;
                struct SlowOp;
                impl Message for SlowOp {
                    type Result = ();
                }
                impl Actor for SlowActor {}
                impl Handler<SlowOp> for SlowActor {
                    async fn handle(
                        &mut self,
                        _msg: SlowOp,
                        reply: crate::reply::PendingReply<'_, ()>,
                        _ctx: &Context<Self>,
                    ) {
                        // Never replies, so the timeout is the only possible outcome
                        // however late the runtime polls it.
                        std::future::pending::<()>().await;
                        reply.reply(());
                    }
                }

                let actor = SlowActor.start();
                let result = actor
                    .request_with_timeout(SlowOp, Duration::from_millis(50))
                    .await;
                assert!(matches!(result, Err(ActorError::RequestTimeout)));
            });
        });
    }

    #[test]
    pub fn recipient_type_erasure() {
        crate::test_wait::bounded(
            concat!(module_path!(), "::recipient_type_erasure"),
            move || {
                let runtime = rt::Runtime::new().unwrap();
                runtime.block_on(async move {
                    let counter = Counter { count: 42 }.start();
                    let recipient: Recipient<GetCount> = counter.recipient();

                    let rx = recipient
                        .send_request(GetCount, Deadline::after(DEFAULT_REQUEST_TIMEOUT))
                        .unwrap();
                    let result = rx.await.unwrap();
                    assert_eq!(result, 42);

                    // Also test request helper
                    let result = request(&*recipient, GetCount, Duration::from_secs(5))
                        .await
                        .unwrap();
                    assert_eq!(result, 42);
                });
            },
        );
    }

    // --- SlowShutdownActor for join tests ---

    struct SlowShutdownActor;

    struct StopSlow;
    impl Message for StopSlow {
        type Result = ();
    }

    impl Actor for SlowShutdownActor {
        async fn stopped(&mut self, _ctx: &Context<Self>) {
            thread::sleep(Duration::from_millis(500));
        }
    }

    impl Handler<StopSlow> for SlowShutdownActor {
        async fn handle(
            &mut self,
            _msg: StopSlow,
            reply: crate::reply::PendingReply<'_, ()>,
            ctx: &Context<Self>,
        ) {
            ctx.stop();
            reply.reply(());
        }
    }

    /// Blocks its thread in a callback until the test releases it.
    struct HeldThread {
        entered: Arc<atomic::AtomicBool>,
        release: std::sync::mpsc::Receiver<()>,
        released: Arc<atomic::AtomicBool>,
    }

    impl HeldThread {
        fn new() -> (Self, std::sync::mpsc::Sender<()>) {
            let (tx, release) = std::sync::mpsc::channel();
            let actor = HeldThread {
                entered: Arc::default(),
                release,
                released: Arc::default(),
            };
            (actor, tx)
        }

        fn hold(&mut self) {
            self.entered.store(true, atomic::Ordering::SeqCst);
            let released = self.release.recv_timeout(crate::test_wait::WAIT).is_ok();
            self.released.store(released, atomic::Ordering::SeqCst);
        }
    }

    impl Actor for HeldThread {
        async fn stopped(&mut self, _ctx: &Context<Self>) {
            // Only the join test sends StopSlow; DoBlock already held.
            if !self.entered.load(atomic::Ordering::SeqCst) {
                self.hold();
            }
        }
    }

    impl Handler<StopSlow> for HeldThread {
        async fn handle(
            &mut self,
            _msg: StopSlow,
            reply: crate::reply::PendingReply<'_, ()>,
            ctx: &Context<Self>,
        ) {
            ctx.stop();
            reply.reply(());
        }
    }

    impl Handler<DoBlock> for HeldThread {
        async fn handle(
            &mut self,
            _msg: DoBlock,
            reply: crate::reply::PendingReply<'_, ()>,
            ctx: &Context<Self>,
        ) {
            self.hold();
            ctx.stop();
            reply.reply(());
        }
    }

    #[test]
    pub fn thread_backend_join_does_not_block_runtime() {
        crate::test_wait::bounded(
            concat!(
                module_path!(),
                "::thread_backend_join_does_not_block_runtime"
            ),
            move || {
                // stopped() blocks its OS thread until a task on this current-thread
                // runtime releases it. If join() blocked the runtime, that task could
                // never run and stopped() would give up after the timeout instead.
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();

                runtime.block_on(async move {
                    let (held, release) = HeldThread::new();
                    let released = held.released.clone();
                    let actor = held.start_with_backend(Backend::Thread);
                    actor.send(StopSlow).unwrap();
                    let _releaser = rt::spawn(async move {
                        let _ = release.send(());
                    });

                    actor.join().await;

                    assert!(
                        released.load(atomic::Ordering::SeqCst),
                        "join() blocked the runtime: the releasing task never ran"
                    );
                });
            },
        );
    }

    #[test]
    pub fn multiple_join_callers_all_notified() {
        crate::test_wait::bounded(
            concat!(module_path!(), "::multiple_join_callers_all_notified"),
            move || {
                let runtime = rt::Runtime::new().unwrap();
                runtime.block_on(async move {
                    let actor = SlowShutdownActor.start();
                    let actor_clone1 = actor.clone();
                    let actor_clone2 = actor.clone();

                    let join1 = rt::spawn(async move {
                        actor_clone1.join().await;
                        1u32
                    });
                    let join2 = rt::spawn(async move {
                        actor_clone2.join().await;
                        2u32
                    });

                    rt::sleep(Duration::from_millis(10)).await;

                    actor.send(StopSlow).unwrap();

                    let (r1, r2) = tokio::join!(join1, join2);
                    assert_eq!(r1.unwrap(), 1);
                    assert_eq!(r2.unwrap(), 2);

                    actor.join().await;
                });
            },
        );
    }

    // --- Badly behaved actors for blocking tests ---

    struct BadlyBehavedTask;

    struct DoBlock;
    impl Message for DoBlock {
        type Result = ();
    }

    impl Actor for BadlyBehavedTask {}

    impl Handler<DoBlock> for BadlyBehavedTask {
        async fn handle(
            &mut self,
            _msg: DoBlock,
            reply: crate::reply::PendingReply<'_, ()>,
            ctx: &Context<Self>,
        ) {
            rt::sleep(Duration::from_millis(20)).await;
            thread::sleep(Duration::from_secs(2));
            ctx.stop();
            reply.reply(());
        }
    }

    struct IncrementWell;
    impl Message for IncrementWell {
        type Result = ();
    }

    struct WellBehavedTask {
        pub count: u64,
    }

    impl Actor for WellBehavedTask {}

    impl Handler<GetCount> for WellBehavedTask {
        async fn handle(
            &mut self,
            _msg: GetCount,
            reply: crate::reply::PendingReply<'_, u64>,
            _ctx: &Context<Self>,
        ) {
            reply.reply(self.count);
        }
    }

    impl Handler<StopCounter> for WellBehavedTask {
        async fn handle(
            &mut self,
            _msg: StopCounter,
            reply: crate::reply::PendingReply<'_, u64>,
            ctx: &Context<Self>,
        ) {
            let __reply_value: u64 = {
                ctx.stop();
                self.count
            };
            reply.reply(__reply_value);
        }
    }

    impl Handler<IncrementWell> for WellBehavedTask {
        async fn handle(
            &mut self,
            _msg: IncrementWell,
            reply: crate::reply::PendingReply<'_, ()>,
            ctx: &Context<Self>,
        ) {
            self.count += 1;
            use crate::tasks::send_after;
            send_after(Duration::from_millis(100), ctx.clone(), IncrementWell);
            reply.reply(());
        }
    }

    #[test]
    pub fn badly_behaved_thread_non_blocking() {
        crate::test_wait::bounded(
            concat!(module_path!(), "::badly_behaved_thread_non_blocking"),
            move || {
                // An async actor that blocks its thread starves everything else on
                // it. With one runtime thread this is deterministic; on a multi-thread
                // runtime the well-behaved actor may run on another worker.
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                runtime.block_on(async move {
                    let badboy = BadlyBehavedTask.start();
                    badboy.send(DoBlock).unwrap();
                    let goodboy = WellBehavedTask { count: 0 }.start();
                    goodboy.send(IncrementWell).unwrap();
                    rt::sleep(Duration::from_secs(1)).await;
                    let count = goodboy.request(GetCount).await.unwrap();
                    // The blocking handler held the only thread for 2 s of the 1 s
                    // window, so the 100 ms chain could not get far.
                    assert!(count < 10, "blocked runtime still made progress: {count}");
                    goodboy.request(StopCounter).await.unwrap();
                });
            },
        );
    }

    /// An async actor on a single-threaded runtime keeps making progress while
    /// an actor on `backend` is blocked inside a handler. With only one runtime
    /// thread, a blocked handler on that thread would stop all progress.
    fn blocked_handler_does_not_stall_runtime(backend: Backend) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            let (held, release) = HeldThread::new();
            let entered = held.entered.clone();
            let badboy = held.start_with_backend(backend);
            badboy.send(DoBlock).unwrap();
            let entered = crate::test_wait::wait_for_async(
                || async { entered.load(atomic::Ordering::SeqCst) },
                |entered| *entered,
            )
            .await;
            assert!(entered, "blocking handler never started");

            let goodboy = WellBehavedTask { count: 0 }.start();
            goodboy.send(IncrementWell).unwrap();
            let count = crate::test_wait::wait_for_async(
                || goodboy.request(GetCount),
                |count| matches!(count, Ok(n) if *n >= 5),
            )
            .await
            .unwrap();
            assert!(count >= 5, "runtime stalled behind a blocked handler");

            release.send(()).unwrap();
            badboy.join().await;
            goodboy.request(StopCounter).await.unwrap();
        });
    }

    #[test]
    pub fn badly_behaved_thread() {
        blocked_handler_does_not_stall_runtime(Backend::Blocking);
    }

    #[test]
    pub fn backend_thread_isolates_blocking_work() {
        blocked_handler_does_not_stall_runtime(Backend::Thread);
    }

    // --- Panic recovery tests ---

    #[test]
    pub fn panic_in_started_stops_actor() {
        crate::test_wait::bounded(
            concat!(module_path!(), "::panic_in_started_stops_actor"),
            move || {
                let runtime = rt::Runtime::new().unwrap();
                runtime.block_on(async move {
                    struct PanicOnStart;
                    struct Ping;
                    impl Message for Ping {
                        type Result = ();
                    }
                    impl Actor for PanicOnStart {
                        async fn started(&mut self, _ctx: &Context<Self>) {
                            panic!("boom in started");
                        }
                    }
                    impl Handler<Ping> for PanicOnStart {
                        async fn handle(
                            &mut self,
                            _msg: Ping,
                            reply: crate::reply::PendingReply<'_, ()>,
                            _ctx: &Context<Self>,
                        ) {
                            reply.reply(());
                        }
                    }

                    let actor = PanicOnStart.start();
                    actor.join().await;
                    let result = actor.send(Ping);
                    assert!(result.is_err());
                });
            },
        );
    }

    #[test]
    pub fn panic_in_handler_stops_actor() {
        crate::test_wait::bounded(
            concat!(module_path!(), "::panic_in_handler_stops_actor"),
            move || {
                let runtime = rt::Runtime::new().unwrap();
                runtime.block_on(async move {
                    struct PanicOnMsg;
                    struct Explode;
                    impl Message for Explode {
                        type Result = ();
                    }
                    struct Check;
                    impl Message for Check {
                        type Result = u32;
                    }
                    impl Actor for PanicOnMsg {}
                    impl Handler<Explode> for PanicOnMsg {
                        async fn handle(
                            &mut self,
                            _msg: Explode,
                            _reply: crate::reply::PendingReply<'_, ()>,
                            _ctx: &Context<Self>,
                        ) {
                            {
                                panic!("boom in handler");
                            }
                        }
                    }
                    impl Handler<Check> for PanicOnMsg {
                        async fn handle(
                            &mut self,
                            _msg: Check,
                            reply: crate::reply::PendingReply<'_, u32>,
                            _ctx: &Context<Self>,
                        ) {
                            reply.reply(42);
                        }
                    }

                    let actor = PanicOnMsg.start();
                    actor.send(Explode).unwrap();
                    actor.join().await;
                    let result = actor.request(Check).await;
                    assert!(result.is_err());
                });
            },
        );
    }

    /// Review P1 (Codex 9): a manual `Handler` may be a plain `fn` that
    /// panics before it returns its future. That is a handler panic like
    /// any other: the turn ends, `stopped()` runs, and the caller and
    /// `wait_exit` see the handler-panic reason. The future used to be built
    /// outside the turn's `catch_unwind`, so the panic unwound the whole
    /// lifecycle (no `stopped()`, an "actor lifecycle unwound" reason).
    #[test]
    pub fn panic_building_the_handler_future_is_a_handler_panic() {
        crate::test_wait::bounded(
            concat!(
                module_path!(),
                "::panic_building_the_handler_future_is_a_handler_panic"
            ),
            move || {
                let runtime = rt::Runtime::new().unwrap();
                runtime.block_on(async move {
                    struct Eager(Arc<atomic::AtomicBool>);
                    struct Explode;
                    impl Message for Explode {
                        type Result = ();
                    }
                    impl Actor for Eager {
                        async fn stopped(&mut self, _ctx: &Context<Self>) {
                            self.0.store(true, Ordering::SeqCst);
                        }
                    }
                    impl Handler<Explode> for Eager {
                        #[allow(unreachable_code, clippy::diverging_sub_expression)]
                        fn handle(
                            &mut self,
                            _msg: Explode,
                            _reply: crate::reply::PendingReply<'_, ()>,
                            _ctx: &Context<Self>,
                        ) -> impl Future<Output = ()> + Send {
                            panic!("boom building the handler future");
                            async {}
                        }
                    }

                    let stopped = Arc::new(atomic::AtomicBool::new(false));
                    let actor = Eager(stopped.clone()).start();
                    let reply = actor.request(Explode).await;
                    let reason = actor.wait_exit().await;
                    let ExitReason::Panic(message) = &reason else {
                        panic!("unexpected reason {reason:?}");
                    };
                    assert!(
                        message.starts_with("panic in handler: boom building"),
                        "not a handler panic: {message}"
                    );
                    assert!(
                        matches!(&reply, Err(ActorError::ActorStopped(r)) if *r == reason),
                        "caller saw {reply:?}"
                    );
                    assert!(stopped.load(Ordering::SeqCst), "stopped() was skipped");
                });
            },
        );
    }

    #[test]
    pub fn panic_in_stopped_still_completes() {
        crate::test_wait::bounded(
            concat!(module_path!(), "::panic_in_stopped_still_completes"),
            move || {
                let runtime = rt::Runtime::new().unwrap();
                runtime.block_on(async move {
                    struct PanicOnStop;
                    struct StopMe;
                    impl Message for StopMe {
                        type Result = ();
                    }
                    impl Actor for PanicOnStop {
                        async fn stopped(&mut self, _ctx: &Context<Self>) {
                            panic!("boom in stopped");
                        }
                    }
                    impl Handler<StopMe> for PanicOnStop {
                        async fn handle(
                            &mut self,
                            _msg: StopMe,
                            reply: crate::reply::PendingReply<'_, ()>,
                            ctx: &Context<Self>,
                        ) {
                            ctx.stop();
                            reply.reply(());
                        }
                    }

                    let actor = PanicOnStop.start();
                    actor.send(StopMe).unwrap();
                    actor.join().await;
                });
            },
        );
    }

    #[test]
    pub fn send_message_on_delivers() {
        crate::test_wait::bounded(
            concat!(module_path!(), "::send_message_on_delivers"),
            move || {
                let runtime = rt::Runtime::new().unwrap();
                runtime.block_on(async move {
                    let counter = Counter { count: 0 }.start();
                    let ctx = counter.context();
                    send_message_on(ctx, rt::sleep(Duration::from_millis(10)), Increment);
                    let count = crate::test_wait::wait_for_async(
                        || counter.request(GetCount),
                        |count| matches!(count, Ok(1)),
                    )
                    .await
                    .unwrap();
                    assert_eq!(count, 1);
                });
            },
        );
    }

    #[test]
    pub fn send_message_on_cancelled() {
        crate::test_wait::bounded(
            concat!(module_path!(), "::send_message_on_cancelled"),
            move || {
                let runtime = rt::Runtime::new().unwrap();
                runtime.block_on(async move {
                    let counter = Counter { count: 0 }.start();
                    let ctx = counter.context();
                    send_message_on(ctx, rt::sleep(Duration::from_millis(200)), Increment);
                    // Stop actor before the future resolves
                    let final_count = counter.request(StopCounter).await.unwrap();
                    assert_eq!(final_count, 0, "message should not have been delivered");
                    counter.join().await;
                });
            },
        );
    }

    // --- ExitReason tests ---

    #[test]
    pub fn exit_reason_normal_on_clean_stop() {
        crate::test_wait::bounded(
            concat!(module_path!(), "::exit_reason_normal_on_clean_stop"),
            move || {
                let runtime = rt::Runtime::new().unwrap();
                runtime.block_on(async move {
                    let actor = Counter { count: 0 }.start();
                    actor.request(StopCounter).await.unwrap();
                    let reason = actor.wait_exit().await;
                    assert!(matches!(reason, ExitReason::Normal));
                });
            },
        );
    }

    #[test]
    pub fn exit_reason_panic_in_started() {
        crate::test_wait::bounded(
            concat!(module_path!(), "::exit_reason_panic_in_started"),
            move || {
                struct PanicStart;
                struct Ping;
                impl Message for Ping {
                    type Result = ();
                }
                impl Actor for PanicStart {
                    async fn started(&mut self, _ctx: &Context<Self>) {
                        panic!("boom in started");
                    }
                }
                impl Handler<Ping> for PanicStart {
                    async fn handle(
                        &mut self,
                        _msg: Ping,
                        reply: crate::reply::PendingReply<'_, ()>,
                        _ctx: &Context<Self>,
                    ) {
                        reply.reply(());
                    }
                }

                let runtime = rt::Runtime::new().unwrap();
                runtime.block_on(async move {
            let actor = PanicStart.start();
            let reason = actor.wait_exit().await;
            assert!(
                matches!(reason, ExitReason::Panic(ref msg) if msg.contains("boom in started"))
            );
        });
            },
        );
    }

    #[test]
    pub fn exit_reason_panic_in_handler() {
        crate::test_wait::bounded(
            concat!(module_path!(), "::exit_reason_panic_in_handler"),
            move || {
                struct PanicHandler;
                struct Explode;
                impl Message for Explode {
                    type Result = ();
                }
                impl Actor for PanicHandler {}
                impl Handler<Explode> for PanicHandler {
                    async fn handle(
                        &mut self,
                        _msg: Explode,
                        _reply: crate::reply::PendingReply<'_, ()>,
                        _ctx: &Context<Self>,
                    ) {
                        {
                            panic!("boom in handler");
                        }
                    }
                }

                let runtime = rt::Runtime::new().unwrap();
                runtime.block_on(async move {
            let actor = PanicHandler.start();
            let _ = actor.send(Explode);
            let reason = actor.wait_exit().await;
            assert!(
                matches!(reason, ExitReason::Panic(ref msg) if msg.contains("boom in handler"))
            );
        });
            },
        );
    }

    #[test]
    pub fn exit_reason_poll_returns_none_while_running() {
        crate::test_wait::bounded(
            concat!(
                module_path!(),
                "::exit_reason_poll_returns_none_while_running"
            ),
            move || {
                let runtime = rt::Runtime::new().unwrap();
                runtime.block_on(async move {
                    let actor = Counter { count: 0 }.start();
                    assert!(actor.exit_reason().is_none());
                    actor.request(StopCounter).await.unwrap();
                    actor.join().await;
                    assert!(actor.exit_reason().is_some());
                });
            },
        );
    }

    // --- Monitor tests ---

    struct GetDowns;
    impl Message for GetDowns {
        type Result = Vec<crate::monitor::Down>;
    }

    /// Actor that exposes `monitor`/`demonitor` via messages, so tests can
    /// drive it from outside. Records all received Down messages.
    struct Watcher {
        downs: Arc<Mutex<Vec<crate::monitor::Down>>>,
    }

    struct StartMonitor(crate::ChildHandle);
    impl Message for StartMonitor {
        type Result = crate::monitor::MonitorRef;
    }
    struct CallDemonitor(crate::monitor::MonitorRef);
    impl Message for CallDemonitor {
        type Result = ();
    }
    /// Monitor, then demonitor in the same turn.
    struct MonitorThenDemonitor(crate::ChildHandle);
    impl Message for MonitorThenDemonitor {
        type Result = ();
    }

    impl Actor for Watcher {}

    impl Handler<StartMonitor> for Watcher {
        async fn handle(
            &mut self,
            msg: StartMonitor,
            reply: crate::reply::PendingReply<'_, crate::monitor::MonitorRef>,
            ctx: &Context<Self>,
        ) {
            reply.reply(ctx.monitor(&msg.0));
        }
    }

    impl Handler<MonitorThenDemonitor> for Watcher {
        async fn handle(
            &mut self,
            msg: MonitorThenDemonitor,
            reply: crate::reply::PendingReply<'_, ()>,
            ctx: &Context<Self>,
        ) {
            let monitor = ctx.monitor(&msg.0);
            ctx.demonitor(monitor);
            reply.reply(());
        }
    }

    impl Handler<CallDemonitor> for Watcher {
        async fn handle(
            &mut self,
            msg: CallDemonitor,
            reply: crate::reply::PendingReply<'_, ()>,
            ctx: &Context<Self>,
        ) {
            ctx.demonitor(msg.0);
            reply.reply(());
        }
    }

    impl Handler<crate::monitor::Down> for Watcher {
        async fn handle(
            &mut self,
            msg: crate::monitor::Down,
            reply: crate::reply::PendingReply<'_, ()>,
            _ctx: &Context<Self>,
        ) {
            self.downs.lock().unwrap().push(msg);
            reply.reply(());
        }
    }

    impl Handler<GetDowns> for Watcher {
        async fn handle(
            &mut self,
            _msg: GetDowns,
            reply: crate::reply::PendingReply<'_, Vec<crate::monitor::Down>>,
            _ctx: &Context<Self>,
        ) {
            reply.reply(self.downs.lock().unwrap().clone());
        }
    }

    #[test]
    pub fn monitor_running_actor_delivers_down_on_exit() {
        crate::test_wait::bounded(
            concat!(
                module_path!(),
                "::monitor_running_actor_delivers_down_on_exit"
            ),
            move || {
                let runtime = rt::Runtime::new().unwrap();
                runtime.block_on(async move {
                    let target = Counter { count: 0 }.start();
                    let target_handle = target.child_handle();

                    let watcher = Watcher {
                        downs: Arc::new(Mutex::new(Vec::new())),
                    }
                    .start();

                    let monitor_ref = watcher.request(StartMonitor(target_handle)).await.unwrap();

                    // Stop the target — Down should be delivered
                    target.request(StopCounter).await.unwrap();
                    target.join().await;

                    let downs = crate::test_wait::wait_for_async(
                        || watcher.request(GetDowns),
                        |downs| downs.as_ref().map_or(true, |d| !d.is_empty()),
                    )
                    .await
                    .unwrap();
                    assert_eq!(downs.len(), 1);
                    assert_eq!(downs[0].monitor_ref, monitor_ref);
                    assert!(matches!(downs[0].reason, ExitReason::Normal));
                });
            },
        );
    }

    /// OTP `demonitor(Ref, [flush])`: a `Down` already in the mailbox
    /// (here the `NoProc` a monitor of a dead target queues at once) is
    /// never handled once `demonitor` returned.
    #[test]
    pub fn demonitor_flushes_a_queued_down() {
        crate::test_wait::bounded(
            concat!(module_path!(), "::demonitor_flushes_a_queued_down"),
            move || {
                let runtime = rt::Runtime::new().unwrap();
                runtime.block_on(async move {
                    let target = Counter { count: 0 }.start();
                    target.request(StopCounter).await.unwrap();
                    target.join().await;
                    let watcher = Watcher {
                        downs: Arc::new(Mutex::new(Vec::new())),
                    }
                    .start();
                    watcher
                        .request(MonitorThenDemonitor(target.child_handle()))
                        .await
                        .unwrap();
                    // FIFO: the queued Down is dispatched before this request.
                    let downs = watcher.request(GetDowns).await.unwrap();
                    assert!(downs.is_empty(), "a flushed Down was handled: {downs:?}");
                });
            },
        );
    }

    #[test]
    pub fn monitor_already_dead_actor_delivers_down_immediately() {
        crate::test_wait::bounded(
            concat!(
                module_path!(),
                "::monitor_already_dead_actor_delivers_down_immediately"
            ),
            move || {
                let runtime = rt::Runtime::new().unwrap();
                runtime.block_on(async move {
                    let target = Counter { count: 0 }.start();
                    target.request(StopCounter).await.unwrap();
                    target.join().await;
                    let target_handle = target.child_handle();

                    let watcher = Watcher {
                        downs: Arc::new(Mutex::new(Vec::new())),
                    }
                    .start();

                    let _ = watcher.request(StartMonitor(target_handle)).await.unwrap();

                    let downs = crate::test_wait::wait_for_async(
                        || watcher.request(GetDowns),
                        |downs| downs.as_ref().map_or(true, |d| !d.is_empty()),
                    )
                    .await
                    .unwrap();
                    assert_eq!(downs.len(), 1);
                });
            },
        );
    }

    #[test]
    pub fn demonitor_before_target_dies_suppresses_down() {
        crate::test_wait::bounded(
            concat!(
                module_path!(),
                "::demonitor_before_target_dies_suppresses_down"
            ),
            move || {
                let runtime = rt::Runtime::new().unwrap();
                runtime.block_on(async move {
                    let target = Counter { count: 0 }.start();
                    let target_handle = target.child_handle();

                    let watcher = Watcher {
                        downs: Arc::new(Mutex::new(Vec::new())),
                    }
                    .start();

                    let monitor_ref = watcher.request(StartMonitor(target_handle)).await.unwrap();
                    watcher.request(CallDemonitor(monitor_ref)).await.unwrap();

                    // Now stop the target
                    target.request(StopCounter).await.unwrap();
                    target.join().await;
                    rt::sleep(Duration::from_millis(50)).await;

                    let downs = watcher.request(GetDowns).await.unwrap();
                    assert!(downs.is_empty(), "expected no Down, got {:?}", downs.len());
                });
            },
        );
    }

    #[test]
    pub fn multiple_monitors_each_get_own_ref_and_down() {
        crate::test_wait::bounded(
            concat!(
                module_path!(),
                "::multiple_monitors_each_get_own_ref_and_down"
            ),
            move || {
                let runtime = rt::Runtime::new().unwrap();
                runtime.block_on(async move {
                    let target = Counter { count: 0 }.start();
                    let target_handle = target.child_handle();

                    let watcher = Watcher {
                        downs: Arc::new(Mutex::new(Vec::new())),
                    }
                    .start();

                    let r1 = watcher
                        .request(StartMonitor(target_handle.clone()))
                        .await
                        .unwrap();
                    let r2 = watcher.request(StartMonitor(target_handle)).await.unwrap();
                    assert_ne!(r1, r2);

                    target.request(StopCounter).await.unwrap();
                    target.join().await;
                    let downs = crate::test_wait::wait_for_async(
                        || watcher.request(GetDowns),
                        |downs| downs.as_ref().map_or(true, |d| d.len() >= 2),
                    )
                    .await
                    .unwrap();
                    assert_eq!(downs.len(), 2);
                    let refs: Vec<_> = downs.iter().map(|d| d.monitor_ref).collect();
                    assert!(refs.contains(&r1));
                    assert!(refs.contains(&r2));
                });
            },
        );
    }

    #[test]
    pub fn monitor_table_is_cleaned_up_after_target_dies() {
        crate::test_wait::bounded(
            concat!(
                module_path!(),
                "::monitor_table_is_cleaned_up_after_target_dies"
            ),
            move || {
                // Watcher should remove its entry from the monitor table after the
                // target dies, so the table doesn't accumulate stale entries.
                struct Inspect;
                impl Message for Inspect {
                    type Result = usize;
                }
                impl Handler<Inspect> for Watcher {
                    async fn handle(
                        &mut self,
                        _msg: Inspect,
                        reply: crate::reply::PendingReply<'_, usize>,
                        ctx: &Context<Self>,
                    ) {
                        reply.reply(
                            ctx.shared
                                .monitors
                                .lock()
                                .unwrap()
                                .as_ref()
                                .map_or(0, HashMap::len),
                        );
                    }
                }

                let runtime = rt::Runtime::new().unwrap();
                runtime.block_on(async move {
                    let target = Counter { count: 0 }.start();
                    let watcher = Watcher {
                        downs: Arc::new(Mutex::new(Vec::new())),
                    }
                    .start();

                    let _ = watcher
                        .request(StartMonitor(target.child_handle()))
                        .await
                        .unwrap();
                    assert_eq!(watcher.request(Inspect).await.unwrap(), 1);

                    target.request(StopCounter).await.unwrap();
                    target.join().await;
                    let entries = crate::test_wait::wait_for_async(
                        || watcher.request(Inspect),
                        |entries| !matches!(entries, Ok(n) if *n > 0),
                    )
                    .await;
                    assert_eq!(
                        entries.unwrap(),
                        0,
                        "monitor table should be empty after target died"
                    );
                });
            },
        );
    }

    #[test]
    pub fn monitor_observes_panic_reason() {
        crate::test_wait::bounded(
            concat!(module_path!(), "::monitor_observes_panic_reason"),
            move || {
                struct PanicMsg;
                impl Message for PanicMsg {
                    type Result = ();
                }
                struct PanicMe;
                impl Actor for PanicMe {}
                impl Handler<PanicMsg> for PanicMe {
                    async fn handle(
                        &mut self,
                        _msg: PanicMsg,
                        _reply: crate::reply::PendingReply<'_, ()>,
                        _ctx: &Context<Self>,
                    ) {
                        {
                            panic!("intentional panic");
                        }
                    }
                }

                let runtime = rt::Runtime::new().unwrap();
                runtime.block_on(async move {
                    let target = PanicMe.start();
                    let target_handle = target.child_handle();

                    let watcher = Watcher {
                        downs: Arc::new(Mutex::new(Vec::new())),
                    }
                    .start();

                    let _ = watcher.request(StartMonitor(target_handle)).await.unwrap();
                    let _ = target.send(PanicMsg);

                    let downs = crate::test_wait::wait_for_async(
                        || watcher.request(GetDowns),
                        |downs| downs.as_ref().map_or(true, |d| !d.is_empty()),
                    )
                    .await
                    .unwrap();
                    assert_eq!(downs.len(), 1);
                    assert!(matches!(downs[0].reason, ExitReason::Panic(_)));
                });
            },
        );
    }

    #[test]
    pub fn monitoring_actor_stops_before_target_does_not_panic() {
        crate::test_wait::bounded(
            concat!(
                module_path!(),
                "::monitoring_actor_stops_before_target_does_not_panic"
            ),
            move || {
                // Regression: if the monitoring actor stops while the target is still
                // alive, the watcher must not crash when the target eventually dies.
                // The watcher's send() will fail silently (mailbox closed), and the
                // watcher exits cleanly.
                let runtime = rt::Runtime::new().unwrap();
                runtime.block_on(async move {
                    let target = Counter { count: 0 }.start();
                    let target_handle = target.child_handle();

                    let watcher = Watcher {
                        downs: Arc::new(Mutex::new(Vec::new())),
                    }
                    .start();

                    let _ = watcher.request(StartMonitor(target_handle)).await.unwrap();

                    // Stop the monitoring actor first
                    let watcher_handle = watcher.child_handle();
                    watcher_handle.stop();
                    watcher_handle.wait_exit_async().await;

                    // Now stop the target — the watcher should clean up without panicking
                    target.request(StopCounter).await.unwrap();
                    target.join().await;
                    // Give the orphaned watcher task time to attempt delivery
                    rt::sleep(Duration::from_millis(50)).await;
                    // If we got here without panicking, the test passes.
                });
            },
        );
    }

    // --- Link tests ---

    /// Trapping actor that records `Exit` notifications.
    struct TrapActor {
        exits: Arc<Mutex<Vec<Exit>>>,
        trap: bool,
    }

    struct GetExits;
    impl Message for GetExits {
        type Result = Vec<Exit>;
    }
    struct LinkTo(crate::ChildHandle);
    impl Message for LinkTo {
        type Result = ();
    }

    impl Actor for TrapActor {
        async fn started(&mut self, ctx: &Context<Self>) {
            ctx.trap_exit(self.trap);
        }
        async fn exit_received(&mut self, exit: Exit, _ctx: &Context<Self>) {
            self.exits.lock().unwrap().push(exit);
        }
    }

    impl Handler<LinkTo> for TrapActor {
        async fn handle(
            &mut self,
            msg: LinkTo,
            reply: crate::reply::PendingReply<'_, ()>,
            ctx: &Context<Self>,
        ) {
            ctx.link(&msg.0);
            reply.reply(());
        }
    }

    impl Handler<GetExits> for TrapActor {
        async fn handle(
            &mut self,
            _msg: GetExits,
            reply: crate::reply::PendingReply<'_, Vec<Exit>>,
            _ctx: &Context<Self>,
        ) {
            reply.reply(self.exits.lock().unwrap().clone());
        }
    }

    fn make_trapper(trap: bool) -> ActorRef<TrapActor> {
        TrapActor {
            exits: Arc::new(Mutex::new(Vec::new())),
            trap,
        }
        .start()
    }

    #[test]
    pub fn link_propagates_panic_to_non_trapping_peer() {
        crate::test_wait::bounded(
            concat!(
                module_path!(),
                "::link_propagates_panic_to_non_trapping_peer"
            ),
            move || {
                let runtime = rt::Runtime::new().unwrap();
                runtime.block_on(async move {
                    // Two non-trapping actors. When A panics, B should be terminated.
                    let a = make_trapper(false);
                    let b = make_trapper(false);
                    a.request(LinkTo(b.child_handle())).await.unwrap();

                    // Make A panic by dropping its only ActorRef while it's mid-handler... easier:
                    // make A panic via a panic message
                    struct Boom;
                    impl Message for Boom {
                        type Result = ();
                    }
                    impl Handler<Boom> for TrapActor {
                        async fn handle(
                            &mut self,
                            _msg: Boom,
                            _reply: crate::reply::PendingReply<'_, ()>,
                            _ctx: &Context<Self>,
                        ) {
                            {
                                panic!("boom");
                            }
                        }
                    }

                    let _ = a.send(Boom);
                    // Both should die
                    let reason_a = a.wait_exit().await;
                    let reason_b = b.wait_exit().await;
                    assert!(matches!(reason_a, ExitReason::Panic(_)));
                    // B should propagate A's reason (transitive propagation)
                    assert!(matches!(reason_b, ExitReason::Panic(_)));
                });
            },
        );
    }

    #[test]
    pub fn link_delivers_exit_to_trapping_peer() {
        crate::test_wait::bounded(
            concat!(module_path!(), "::link_delivers_exit_to_trapping_peer"),
            move || {
                let runtime = rt::Runtime::new().unwrap();
                runtime.block_on(async move {
                    let a = make_trapper(false);
                    let b = make_trapper(true); // B traps
                    b.request(LinkTo(a.child_handle())).await.unwrap();

                    struct Boom2;
                    impl Message for Boom2 {
                        type Result = ();
                    }
                    impl Handler<Boom2> for TrapActor {
                        async fn handle(
                            &mut self,
                            _msg: Boom2,
                            _reply: crate::reply::PendingReply<'_, ()>,
                            _ctx: &Context<Self>,
                        ) {
                            {
                                panic!("boom2");
                            }
                        }
                    }

                    let _ = a.send(Boom2);
                    a.wait_exit().await;
                    let exits = crate::test_wait::wait_for_async(
                        || b.request(GetExits),
                        |exits| exits.as_ref().map_or(true, |e| !e.is_empty()),
                    )
                    .await
                    .unwrap();

                    // B should still be alive and have received an Exit
                    assert!(b.exit_reason().is_none());
                    assert_eq!(exits.len(), 1);
                    assert_eq!(exits[0].from, a.id());
                    assert!(matches!(exits[0].reason, ExitReason::Panic(_)));

                    // Clean up
                    let bh = b.child_handle();
                    bh.stop();
                    bh.wait_exit_async().await;
                });
            },
        );
    }

    #[test]
    pub fn link_normal_exit_not_propagated_to_non_trapping_peer() {
        crate::test_wait::bounded(
            concat!(
                module_path!(),
                "::link_normal_exit_not_propagated_to_non_trapping_peer"
            ),
            move || {
                let runtime = rt::Runtime::new().unwrap();
                runtime.block_on(async move {
                    let a = Counter { count: 0 }.start();
                    let b = make_trapper(false);
                    b.request(LinkTo(a.child_handle())).await.unwrap();

                    // A stops cleanly
                    a.request(StopCounter).await.unwrap();
                    a.join().await;
                    rt::sleep(Duration::from_millis(50)).await;

                    // B should still be alive
                    assert!(b.exit_reason().is_none());

                    let bh = b.child_handle();
                    bh.stop();
                    bh.wait_exit_async().await;
                });
            },
        );
    }

    #[test]
    pub fn link_to_already_dead_actor_delivers_noproc() {
        crate::test_wait::bounded(
            concat!(
                module_path!(),
                "::link_to_already_dead_actor_delivers_noproc"
            ),
            move || {
                let runtime = rt::Runtime::new().unwrap();
                runtime.block_on(async move {
                    // Start and panic a target
                    struct Boom3;
                    impl Message for Boom3 {
                        type Result = ();
                    }
                    impl Handler<Boom3> for TrapActor {
                        async fn handle(
                            &mut self,
                            _msg: Boom3,
                            _reply: crate::reply::PendingReply<'_, ()>,
                            _ctx: &Context<Self>,
                        ) {
                            {
                                panic!("boom3");
                            }
                        }
                    }
                    let target = make_trapper(false);
                    let _ = target.send(Boom3);
                    target.wait_exit().await;

                    // Now link from a trapping observer: the target is past its
                    // death point, so the Exit carries NoProc (E4), not its reason.
                    let observer = make_trapper(true);
                    observer
                        .request(LinkTo(target.child_handle()))
                        .await
                        .unwrap();

                    let exits = crate::test_wait::wait_for_async(
                        || observer.request(GetExits),
                        |exits| exits.as_ref().map_or(true, |e| !e.is_empty()),
                    )
                    .await
                    .unwrap();
                    assert_eq!(exits.len(), 1);
                    assert_eq!(exits[0].reason, ExitReason::NoProc);
                    assert!(matches!(target.exit_reason(), Some(ExitReason::Panic(_))));

                    let oh = observer.child_handle();
                    oh.stop();
                    oh.wait_exit_async().await;
                });
            },
        );
    }

    #[test]
    pub fn unlink_prevents_signal_delivery() {
        crate::test_wait::bounded(
            concat!(module_path!(), "::unlink_prevents_signal_delivery"),
            move || {
                let runtime = rt::Runtime::new().unwrap();
                runtime.block_on(async move {
                    let a = Counter { count: 0 }.start();
                    let b = make_trapper(true);
                    b.request(LinkTo(a.child_handle())).await.unwrap();

                    // Now unlink
                    struct UnlinkFrom(crate::ChildHandle);
                    impl Message for UnlinkFrom {
                        type Result = ();
                    }
                    impl Handler<UnlinkFrom> for TrapActor {
                        async fn handle(
                            &mut self,
                            msg: UnlinkFrom,
                            reply: crate::reply::PendingReply<'_, ()>,
                            ctx: &Context<Self>,
                        ) {
                            ctx.unlink(&msg.0);
                            reply.reply(());
                        }
                    }
                    b.request(UnlinkFrom(a.child_handle())).await.unwrap();

                    // A panics
                    struct Boom4;
                    impl Message for Boom4 {
                        type Result = u64;
                    }
                    impl Handler<Boom4> for Counter {
                        async fn handle(
                            &mut self,
                            _msg: Boom4,
                            _reply: crate::reply::PendingReply<'_, u64>,
                            _ctx: &Context<Self>,
                        ) {
                            panic!("boom4");
                        }
                    }
                    // Counter's Boom4 returns u64; just use send with no expectation of reply
                    let _ = a.send(Boom4);
                    a.wait_exit().await;
                    rt::sleep(Duration::from_millis(50)).await;

                    // B should NOT have received an Exit
                    let exits = b.request(GetExits).await.unwrap();
                    assert!(exits.is_empty());

                    let bh = b.child_handle();
                    bh.stop();
                    bh.wait_exit_async().await;
                });
            },
        );
    }

    #[test]
    pub fn duplicate_link_is_idempotent() {
        crate::test_wait::bounded(
            concat!(module_path!(), "::duplicate_link_is_idempotent"),
            move || {
                let runtime = rt::Runtime::new().unwrap();
                runtime.block_on(async move {
                    let a = Counter { count: 0 }.start();
                    let b = make_trapper(true);
                    b.request(LinkTo(a.child_handle())).await.unwrap();
                    b.request(LinkTo(a.child_handle())).await.unwrap(); // duplicate

                    struct Boom5;
                    impl Message for Boom5 {
                        type Result = u64;
                    }
                    impl Handler<Boom5> for Counter {
                        async fn handle(
                            &mut self,
                            _msg: Boom5,
                            _reply: crate::reply::PendingReply<'_, u64>,
                            _ctx: &Context<Self>,
                        ) {
                            panic!("boom5");
                        }
                    }
                    let _ = a.send(Boom5);
                    a.wait_exit().await;
                    crate::test_wait::wait_for_async(
                        || b.request(GetExits),
                        |exits| exits.as_ref().map_or(true, |e| !e.is_empty()),
                    )
                    .await
                    .unwrap();
                    // Leave room for a duplicate before checking there is none.
                    rt::sleep(Duration::from_millis(50)).await;

                    // Should receive only ONE Exit, not two
                    let exits = b.request(GetExits).await.unwrap();
                    assert_eq!(exits.len(), 1);

                    let bh = b.child_handle();
                    bh.stop();
                    bh.wait_exit_async().await;
                });
            },
        );
    }

    #[test]
    pub fn chain_propagation_through_non_trapping_middle() {
        crate::test_wait::bounded(
            concat!(
                module_path!(),
                "::chain_propagation_through_non_trapping_middle"
            ),
            move || {
                let runtime = rt::Runtime::new().unwrap();
                runtime.block_on(async move {
                    // A linked to B linked to C. A panics → B dies (not trapping) →
                    // B's death propagates to C.
                    let a = make_trapper(false);
                    let b = make_trapper(false);
                    let c = make_trapper(true); // C traps

                    a.request(LinkTo(b.child_handle())).await.unwrap();
                    c.request(LinkTo(b.child_handle())).await.unwrap();

                    struct Boom6;
                    impl Message for Boom6 {
                        type Result = ();
                    }
                    impl Handler<Boom6> for TrapActor {
                        async fn handle(
                            &mut self,
                            _msg: Boom6,
                            _reply: crate::reply::PendingReply<'_, ()>,
                            _ctx: &Context<Self>,
                        ) {
                            {
                                panic!("boom6");
                            }
                        }
                    }
                    let _ = a.send(Boom6);
                    a.wait_exit().await;
                    b.wait_exit().await;
                    let exits = crate::test_wait::wait_for_async(
                        || c.request(GetExits),
                        |exits| exits.as_ref().map_or(true, |e| !e.is_empty()),
                    )
                    .await
                    .unwrap();
                    assert_eq!(exits.len(), 1, "expected C to receive Exit from B's death");

                    let ch = c.child_handle();
                    ch.stop();
                    ch.wait_exit_async().await;
                });
            },
        );
    }

    #[test]
    pub fn link_to_already_dead_delivers_exactly_once_to_trapping_peer() {
        crate::test_wait::bounded(
            concat!(
                module_path!(),
                "::link_to_already_dead_delivers_exactly_once_to_trapping_peer"
            ),
            move || {
                // Regression: previously, `ctx.link()` would deliver a duplicate Exit
                // to trapping actors when the target died concurrently — once from
                // propagate_exit (since register_link inserted self in target's table
                // before target's drain ran) and once from the fallback exit_reason()
                // check.
                struct Boom7;
                impl Message for Boom7 {
                    type Result = ();
                }
                impl Handler<Boom7> for TrapActor {
                    async fn handle(
                        &mut self,
                        _msg: Boom7,
                        _reply: crate::reply::PendingReply<'_, ()>,
                        _ctx: &Context<Self>,
                    ) {
                        {
                            panic!("boom7");
                        }
                    }
                }

                let runtime = rt::Runtime::new().unwrap();
                runtime.block_on(async move {
                    let target = make_trapper(false);
                    // Kick off the panic
                    let _ = target.send(Boom7);
                    // Wait for target to fully die so exit_reason() is Some
                    target.wait_exit().await;

                    // Now link from a trapping observer — should receive Exit EXACTLY ONCE
                    let observer = make_trapper(true);
                    observer
                        .request(LinkTo(target.child_handle()))
                        .await
                        .unwrap();

                    crate::test_wait::wait_for_async(
                        || observer.request(GetExits),
                        |exits| exits.as_ref().map_or(true, |e| !e.is_empty()),
                    )
                    .await
                    .unwrap();
                    // Leave room for a duplicate before checking there is none.
                    rt::sleep(Duration::from_millis(50)).await;
                    let exits = observer.request(GetExits).await.unwrap();
                    assert_eq!(exits.len(), 1, "expected exactly one Exit, got {:?}", exits);

                    let oh = observer.child_handle();
                    oh.stop();
                    oh.wait_exit_async().await;
                });
            },
        );
    }

    #[test]
    pub fn start_linked_links_atomically() {
        crate::test_wait::bounded(
            concat!(module_path!(), "::start_linked_links_atomically"),
            move || {
                // Parent + child via start_linked. When parent stops via panic,
                // child (not trapping) should die too.
                struct Parent;
                struct PanicParent;
                impl Message for PanicParent {
                    type Result = ();
                }
                impl Actor for Parent {}
                impl Handler<PanicParent> for Parent {
                    async fn handle(
                        &mut self,
                        _msg: PanicParent,
                        _reply: crate::reply::PendingReply<'_, ()>,
                        _ctx: &Context<Self>,
                    ) {
                        {
                            panic!("parent boom");
                        }
                    }
                }

                struct Child;
                impl Actor for Child {}

                let runtime = rt::Runtime::new().unwrap();
                runtime.block_on(async move {
                    let parent = Parent.start();
                    let child = Child.start_linked(&parent.context());

                    // Parent panics — child should die too
                    let _ = parent.send(PanicParent);
                    parent.wait_exit().await;
                    // Child should also die (link propagation)
                    let reason = child.wait_exit().await;
                    // Reason should be the parent's panic
                    assert!(matches!(reason, ExitReason::Panic(_)));
                });
            },
        );
    }
}

#[cfg(test)]
mod monitor_cleanup_regressions {
    use super::*;
    struct Idle;
    impl Actor for Idle {}
    impl Handler<Down> for Idle {
        async fn handle(
            &mut self,
            _: Down,
            reply: crate::reply::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            reply.reply(());
        }
    }
    #[test]
    fn closed_monitor_table_rejects_late_registration() {
        crate::test_wait::bounded(
            concat!(
                module_path!(),
                "::closed_monitor_table_rejects_late_registration"
            ),
            move || {
                let rt = rt::Runtime::new().unwrap();
                let _entered = rt.enter();
                let observer = Idle.start();
                let target = Idle.start();
                // Model cleanup during an unexpected unwind before token cancellation.
                drop(MonitorCleanup(observer.shared.monitors.clone()));
                let child = target.child_handle();
                let waiter = child.lifetime_probe();
                observer.context().monitor(&child);
                drop(child);
                assert_eq!(waiter.strong_count(), 0);
                assert!(observer.shared.monitors.lock().unwrap().is_none());
                observer.child_handle().stop();
                target.child_handle().stop();
                rt.block_on(observer.join());
                rt.block_on(target.join());
            },
        );
    }
    fn check_cleanup(stop_owner: bool) {
        let rt = rt::Runtime::new().unwrap();
        let _entered = rt.enter();
        let target = Idle.start();
        let observer = Idle.start();
        let child = target.child_handle();
        let waiter = child.lifetime_probe();
        let monitor = observer.context().monitor(&child);
        drop(child);
        if stop_owner {
            observer.child_handle().stop();
            rt.block_on(observer.join());
            drop(observer);
        } else {
            observer.context().demonitor(monitor);
            // Only the waiter now owns this particular ChildHandle allocation.
            let released = crate::test_wait::wait_for(|| waiter.strong_count() == 0, |r| *r);
            observer.child_handle().stop();
            rt.block_on(observer.join());
            drop(observer);
            target.child_handle().stop();
            rt.block_on(target.join());
            assert!(released, "demonitor retained its waiter until target exit");
            return;
        }
        let released = crate::test_wait::wait_for(|| waiter.strong_count() == 0, |r| *r);
        target.child_handle().stop();
        rt.block_on(target.join());
        assert!(released, "stopped observer retained its monitor waiter");
    }
    #[test]
    fn demonitor_releases_waiter() {
        check_cleanup(false);
    }
    #[test]
    fn stopped_observer_releases_waiter() {
        check_cleanup(true);
    }
}

#[cfg(test)]
mod early_stop_regression {
    use super::*;
    struct EarlyStop;
    impl Actor for EarlyStop {
        async fn started(&mut self, ctx: &Context<Self>) {
            ctx.stop();
        }
        async fn stopped(&mut self, _: &Context<Self>) {
            panic!("cleanup failed");
        }
    }
    #[test]
    fn panic_during_early_stop_is_reported() {
        crate::test_wait::bounded(
            concat!(module_path!(), "::panic_during_early_stop_is_reported"),
            move || {
                let rt = rt::Runtime::new().unwrap();
                let _enter = rt.enter();
                let actor = EarlyStop.start();
                assert!(
                    matches!(rt.block_on(actor.wait_exit()), ExitReason::Panic(message) if message.contains("cleanup failed"))
                );
            },
        );
    }
}

#[cfg(all(test, feature = "backend-experiments"))]
mod turn_regressions {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    #[test]
    fn idle_time_is_excluded_and_an_empty_mailbox_needs_no_yield() {
        crate::test_wait::bounded(
            concat!(
                module_path!(),
                "::idle_time_is_excluded_and_an_empty_mailbox_needs_no_yield"
            ),
            move || {
                let mut turn = MessageTurn {
                    limit: 16,
                    adaptive: false,
                    slice: Some(Duration::from_micros(50)),
                    completed: 0,
                    began: Some(std::time::Instant::now()),
                };
                turn.parked();
                assert!(turn.began.is_none());
                let wake = std::time::Instant::now();
                turn.resumed();
                assert!(turn.began.unwrap() >= wake);
                futures::executor::block_on(turn.after_message(true));
                assert!(turn.began.is_none());
                assert_eq!(turn.completed, 0);
            },
        );
    }
    #[test]
    fn count_only_turn_never_starts_a_clock_and_yields_at_its_count() {
        crate::test_wait::bounded(
            concat!(
                module_path!(),
                "::count_only_turn_never_starts_a_clock_and_yields_at_its_count"
            ),
            move || {
                rt::Runtime::new().unwrap().block_on(async {
                    let mut turn = MessageTurn {
                        limit: 4,
                        adaptive: false,
                        slice: None,
                        completed: 0,
                        began: None,
                    };
                    turn.resumed();
                    for _ in 0..3 {
                        let mut message = Box::pin(turn.after_message(false));
                        assert!(futures::poll!(message.as_mut()).is_ready());
                    }
                    assert!(turn.began.is_none());
                    let mut fourth = Box::pin(turn.after_message(false));
                    assert!(futures::poll!(fourth.as_mut()).is_pending());
                    fourth.await;
                    assert_eq!(turn.completed, 0);
                    assert!(turn.began.is_none());
                    turn.parked();
                    turn.resumed();
                    assert!(turn.began.is_none());
                });
            },
        );
    }
    #[test]
    fn stop_during_batch_yield_does_not_execute_the_next_queued_message() {
        crate::test_wait::bounded(
            concat!(
                module_path!(),
                "::stop_during_batch_yield_does_not_execute_the_next_queued_message"
            ),
            move || {
                struct Count(Arc<AtomicUsize>);
                impl Actor for Count {}
                struct Inc;
                impl Message for Inc {
                    type Result = ();
                }
                impl Handler<Inc> for Count {
                    async fn handle(
                        &mut self,
                        _: Inc,
                        reply: crate::reply::PendingReply<'_, ()>,
                        _: &Context<Self>,
                    ) {
                        self.0.fetch_add(1, Ordering::SeqCst);
                        reply.reply(());
                    }
                }
                rt::Runtime::new().unwrap().block_on(async {
                    let (tx, rx) = mpsc::channel();
                    let token = CancellationToken::new();
                    let (ctx, mut cleanup) = test_context(&tx, &token);
                    let count = Arc::new(AtomicUsize::new(0));
                    for _ in 0..2 {
                        tx.send(Box::new(MessageEnvelope {
                            msg: Some(Inc),
                            slot: SlotHandle::detached(),
                            lent: None,
                            #[cfg(feature = "metrics")]
                            observation: None,
                            #[cfg(feature = "lifecycle-trace")]
                            from: 0,
                        })
                            as Box<dyn Envelope<Count> + Send>)
                            .unwrap();
                    }
                    let turn = MessageTurn {
                        limit: 1,
                        adaptive: false,
                        slice: Some(Duration::from_micros(50)),
                        completed: 0,
                        began: None,
                    };
                    let mut job = Box::pin(run_actor(
                        Count(count.clone()),
                        ctx,
                        rx,
                        token.clone(),
                        &mut cleanup,
                        Some(turn),
                    ));
                    assert!(futures::poll!(job.as_mut()).is_pending());
                    assert_eq!(count.load(Ordering::SeqCst), 1);
                    token.cancel();
                    job.await;
                    assert_eq!(cleanup.die(), ExitReason::Normal);
                    assert_eq!(count.load(Ordering::SeqCst), 1);
                });
            },
        );
    }
}

#[cfg(test)]
mod idle_stop_regression {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Counter(Arc<AtomicUsize>);
    impl Actor for Counter {}
    struct Increment;
    impl Message for Increment {
        type Result = ();
    }
    impl Handler<Increment> for Counter {
        async fn handle(
            &mut self,
            _: Increment,
            reply: crate::reply::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            self.0.fetch_add(1, Ordering::SeqCst);
            reply.reply(());
        }
    }
    #[test]
    fn stop_while_idle_does_not_dispatch_a_queued_request_on_resume() {
        crate::test_wait::bounded(
            concat!(
                module_path!(),
                "::stop_while_idle_does_not_dispatch_a_queued_request_on_resume"
            ),
            move || {
                rt::Runtime::new().unwrap().block_on(async {
                    let (tx, rx) = mpsc::channel();
                    let token = CancellationToken::new();
                    let (ctx, mut cleanup) = test_context(&tx, &token);
                    let count = Arc::new(AtomicUsize::new(0));
                    let mut actor = Box::pin(run_actor(
                        Counter(count.clone()),
                        ctx,
                        rx,
                        token.clone(),
                        &mut cleanup,
                        #[cfg(feature = "backend-experiments")]
                        None,
                    ));
                    // Start and park the actual actor loop, without allowing another
                    // executor task to run it between admission and stop below.
                    assert!(futures::poll!(actor.as_mut()).is_pending());
                    let (slot, response) = reply_slot(crate::reply::Deadline::Never, None, false);
                    tx.send(Box::new(MessageEnvelope {
                        msg: Some(Increment),
                        slot,
                        lent: None,
                        #[cfg(feature = "metrics")]
                        observation: None,
                        #[cfg(feature = "lifecycle-trace")]
                        from: 0,
                    }) as Box<dyn Envelope<Counter> + Send>)
                        .unwrap();
                    token.cancel();
                    actor.await;
                    assert_eq!(cleanup.die(), ExitReason::Normal);
                    assert_eq!(
                        count.load(Ordering::SeqCst),
                        0,
                        "idle actor dispatched queued work after stop"
                    );
                    assert!(response.await.is_err(), "queued request unexpectedly ran");
                });
            },
        );
    }
}

/// Context and request APIs that the behavioural tests above do not reach.
#[cfg(test)]
mod api_coverage {
    use super::*;
    use crate::test_wait::WAIT;

    struct Echo;
    struct Ask(u32);
    impl Message for Ask {
        type Result = u32;
    }
    /// Holds the actor inside a handler until released, then stops it, so a
    /// request queued behind it is admitted but never answered.
    struct StopAfter(rt::oneshot::Receiver<()>);
    impl Message for StopAfter {
        type Result = ();
    }
    struct OwnRecipient;
    impl Message for OwnRecipient {
        type Result = (Recipient<Ask>, ActorId);
    }
    struct Boom;
    impl Message for Boom {
        type Result = ();
    }
    impl Actor for Echo {}
    impl Handler<Ask> for Echo {
        async fn handle(
            &mut self,
            msg: Ask,
            reply: crate::reply::PendingReply<'_, u32>,
            _: &Context<Self>,
        ) {
            reply.reply(msg.0);
        }
    }
    impl Handler<StopAfter> for Echo {
        async fn handle(
            &mut self,
            msg: StopAfter,
            reply: crate::reply::PendingReply<'_, ()>,
            ctx: &Context<Self>,
        ) {
            let _ = msg.0.await;
            ctx.stop();
            reply.reply(());
        }
    }
    impl Handler<OwnRecipient> for Echo {
        async fn handle(
            &mut self,
            _: OwnRecipient,
            reply: crate::reply::PendingReply<'_, (Recipient<Ask>, ActorId)>,
            ctx: &Context<Self>,
        ) {
            reply.reply((ctx.recipient::<Ask>(), ctx.id()));
        }
    }
    impl Handler<Boom> for Echo {
        async fn handle(
            &mut self,
            _: Boom,
            _reply: crate::reply::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            {
                panic!("boom");
            }
        }
    }

    #[test]
    fn context_recipient_reaches_its_actor() {
        crate::test_wait::bounded(
            concat!(module_path!(), "::context_recipient_reaches_its_actor"),
            move || {
                rt::Runtime::new().unwrap().block_on(async {
                    let actor = Echo.start();
                    let (recipient, id) = actor.request(OwnRecipient).await.unwrap();
                    assert_eq!(id, actor.id());
                    assert!(matches!(request(&*recipient, Ask(7), WAIT).await, Ok(7)));
                    recipient.send(Ask(1)).unwrap();
                    actor.child_handle().stop();
                    actor.join().await;
                    // Past the death point: NoProc (v0.8, decision 4 of otp-exit-signals).
                    assert!(matches!(recipient.send(Ask(1)), Err(ActorError::NoProc)));
                });
            },
        );
    }

    #[test]
    fn requests_to_a_stopped_actor_fail_without_waiting() {
        crate::test_wait::bounded(
            concat!(
                module_path!(),
                "::requests_to_a_stopped_actor_fail_without_waiting"
            ),
            move || {
                rt::Runtime::new().unwrap().block_on(async {
                    let actor = Echo.start();
                    let recipient = actor.recipient::<Ask>();
                    actor.child_handle().stop();
                    actor.join().await;
                    // Past the death point: NoProc (v0.8, decision 4 of otp-exit-signals).
                    assert!(matches!(
                        actor.request_with_timeout(Ask(1), WAIT).await,
                        Err(ActorError::NoProc)
                    ));
                    assert!(matches!(
                        request(&*recipient, Ask(1), WAIT).await,
                        Err(ActorError::NoProc)
                    ));
                });
            },
        );
    }

    #[test]
    fn admitted_requests_fail_when_the_actor_stops_before_replying() {
        crate::test_wait::bounded(
            concat!(
                module_path!(),
                "::admitted_requests_fail_when_the_actor_stops_before_replying"
            ),
            move || {
                rt::Runtime::new().unwrap().block_on(async {
                    let actor = Echo.start();
                    let recipient = actor.recipient::<Ask>();
                    let (release, held) = rt::oneshot::channel();
                    actor.send(StopAfter(held)).unwrap();
                    let mut direct = Box::pin(actor.request_with_timeout(Ask(1), WAIT));
                    let mut erased = Box::pin(request(&*recipient, Ask(2), WAIT));
                    assert!(futures::poll!(direct.as_mut()).is_pending());
                    assert!(futures::poll!(erased.as_mut()).is_pending());
                    release.send(()).unwrap();
                    assert!(matches!(
                        direct.await,
                        Err(ActorError::ActorStopped(ExitReason::Normal))
                    ));
                    assert!(matches!(
                        erased.await,
                        Err(ActorError::ActorStopped(ExitReason::Normal))
                    ));
                });
            },
        );
    }

    #[test]
    fn erased_request_times_out() {
        crate::test_wait::bounded(
            concat!(module_path!(), "::erased_request_times_out"),
            move || {
                rt::Runtime::new().unwrap().block_on(async {
                    let actor = Echo.start();
                    let recipient = actor.recipient::<Ask>();
                    let (release, held) = rt::oneshot::channel();
                    actor.send(StopAfter(held)).unwrap();
                    assert!(matches!(
                        request(&*recipient, Ask(1), Duration::from_millis(1)).await,
                        Err(ActorError::RequestTimeout)
                    ));
                    release.send(()).unwrap();
                    actor.join().await;
                });
            },
        );
    }

    #[test]
    fn default_exit_received_keeps_a_trapping_actor_alive() {
        crate::test_wait::bounded(
            concat!(
                module_path!(),
                "::default_exit_received_keeps_a_trapping_actor_alive"
            ),
            move || {
                struct Trapper;
                impl Actor for Trapper {
                    async fn started(&mut self, ctx: &Context<Self>) {
                        ctx.trap_exit(true);
                    }
                }
                impl Handler<Ask> for Trapper {
                    async fn handle(
                        &mut self,
                        msg: Ask,
                        reply: crate::reply::PendingReply<'_, u32>,
                        _: &Context<Self>,
                    ) {
                        reply.reply(msg.0);
                    }
                }
                rt::Runtime::new().unwrap().block_on(async {
                    let trapper = Trapper.start();
                    assert!(matches!(trapper.request(Ask(0)).await, Ok(0)));
                    let peer = Echo.start();
                    trapper.context().link(&peer.child_handle());
                    peer.send(Boom).unwrap();
                    assert!(matches!(peer.wait_exit().await, ExitReason::Panic(_)));
                    // The Exit is handled by the default `exit_received`, which ignores it.
                    assert!(matches!(trapper.request(Ask(3)).await, Ok(3)));
                    assert!(trapper.exit_reason().is_none());
                    trapper
                        .child_handle()
                        .stop_with_reason(crate::ExitReason::Shutdown);
                    assert_eq!(trapper.wait_exit().await, ExitReason::Shutdown);
                });
            },
        );
    }

    #[test]
    fn debug_output_names_the_handle() {
        crate::test_wait::bounded(
            concat!(module_path!(), "::debug_output_names_the_handle"),
            move || {
                rt::Runtime::new().unwrap().block_on(async {
                    let actor = Echo.start();
                    assert!(format!("{actor:?}").starts_with("ActorRef"));
                    assert!(format!("{:?}", actor.context()).starts_with("Context"));
                    actor.child_handle().stop();
                    actor.join().await;
                });
            },
        );
    }
}

#[cfg(test)]
mod shared_state_release {
    //! `ActorRef` and `Context` share one `Arc<ActorShared>`. Once the actor has
    //! stopped and every handle is dropped, that block must be freed: nothing
    //! the runtime keeps (links, monitors, child handles, the run task) may hold it.
    use super::*;
    use std::sync::Weak;

    async fn freed<A: Actor>(weak: &Weak<ActorShared<A>>) -> bool {
        for _ in 0..500 {
            if weak.upgrade().is_none() {
                return true;
            }
            rt::sleep(Duration::from_millis(10)).await;
        }
        false
    }

    struct Plain;
    impl Actor for Plain {}
    impl Handler<Down> for Plain {
        async fn handle(
            &mut self,
            _: Down,
            reply: crate::reply::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            reply.reply(());
        }
    }

    /// Keeps a clone of its own `Context` in its state.
    struct Holder(Option<Context<Holder>>);
    impl Actor for Holder {
        async fn started(&mut self, ctx: &Context<Self>) {
            self.0 = Some(ctx.clone());
        }
    }

    #[test]
    fn freed_after_stop_and_last_handle_dropped() {
        crate::test_wait::bounded(
            concat!(module_path!(), "::freed_after_stop_and_last_handle_dropped"),
            move || {
                rt::Runtime::new().unwrap().block_on(async {
                    let actor = Plain.start();
                    let weak = actor.shared_weak();
                    let extra = (actor.clone(), actor.context(), actor.child_handle());
                    actor.context().stop();
                    actor.join().await;
                    drop(extra);
                    drop(actor);
                    assert!(freed(&weak).await, "ActorShared outlived every handle");
                });
            },
        );
    }

    #[test]
    fn freed_when_context_was_stored_in_actor_state() {
        crate::test_wait::bounded(
            concat!(
                module_path!(),
                "::freed_when_context_was_stored_in_actor_state"
            ),
            move || {
                rt::Runtime::new().unwrap().block_on(async {
                    let actor = Holder(None).start();
                    let weak = actor.shared_weak();
                    actor.context().stop();
                    actor.join().await;
                    drop(actor);
                    assert!(freed(&weak).await, "Context kept in actor state leaked");
                });
            },
        );
    }

    #[test]
    fn freed_with_links_and_monitors() {
        crate::test_wait::bounded(
            concat!(module_path!(), "::freed_with_links_and_monitors"),
            move || {
                rt::Runtime::new().unwrap().block_on(async {
                    let a = Plain.start();
                    let b = Plain.start();
                    let watcher = Plain.start();
                    a.context().trap_exit(true);
                    a.context().link(&b.child_handle());
                    let _monitor = watcher.context().monitor(&a.child_handle());
                    watcher.context().monitor(&b.child_handle());
                    let child = Plain.start_linked(&a.context());
                    let weaks = [
                        a.shared_weak(),
                        b.shared_weak(),
                        watcher.shared_weak(),
                        child.shared_weak(),
                    ];
                    for actor in [&b, &a, &watcher] {
                        actor.context().stop();
                        actor.join().await;
                    }
                    child.context().stop();
                    child.join().await;
                    drop((a, b, watcher, child));
                    for weak in &weaks {
                        assert!(freed(weak).await, "linked or monitored actor leaked");
                    }
                });
            },
        );
    }
}
