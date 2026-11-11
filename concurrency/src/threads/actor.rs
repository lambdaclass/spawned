use crate::mailbox::threads as mpsc;
use spawned_rt::threads::{self as rt, CancellationToken};
#[cfg(test)]
use std::{collections::HashMap, sync::Mutex};
use std::{
    fmt::Debug,
    panic::{catch_unwind, AssertUnwindSafe},
    sync::Arc,
    time::Duration,
};

use crate::child_handle::{ActorId, ChildHandle, Completion};
use crate::error::{panic_message, ActorError, ExitReason, NoProc};
use crate::link::{
    self, new_link_table, new_linked_exit_reason, new_trap_exit_flag, Exit, LinkOutcome, LinkTable,
    LinkedExitReason, SendExitFn, SendHeldFn, SignalTarget, TrapExitFlag,
};
use crate::message::Message;
use crate::monitor::{Down, MonitorCleanup, MonitorRef, MonitorTable};
use crate::reply::{registry::Registry, reply_slot, SlotHandle};
use crate::reply::{Deadline, PendingReply, ReplyReceiver, DEFAULT_MAX_OUTSTANDING};
use std::sync::atomic::Ordering;

pub use crate::response::DEFAULT_REQUEST_TIMEOUT;

/// Cleanup deadline for actors without a supervisor: the bound on joining an
/// exiting actor's cancelled jobs.
pub const DEFAULT_STOP_DEADLINE: Duration = Duration::from_secs(5);

#[path = "jobs.rs"]
mod jobs;
pub use jobs::{BlockingJobPermit, JobCtx};

// ---------------------------------------------------------------------------
// Actor trait
// ---------------------------------------------------------------------------

/// Trait for defining an actor's lifecycle hooks.
///
/// Implement this trait (typically via `#[actor]`) to define `started()` and
/// `stopped()` callbacks. Message handling is defined separately via [`Handler<M>`].
///
/// Actors must be `Send + Sized + 'static` so they can be moved to a spawned thread.
pub trait Actor: Send + Sized + 'static {
    /// Deferred-reply credits per instance (live `ReplyTo` tokens plus running
    /// linked jobs). At the bound `defer()` answers the caller `Overloaded`.
    /// Default 1024; capped at 65535.
    const MAX_OUTSTANDING: usize = DEFAULT_MAX_OUTSTANDING;

    /// Native threads this instance's jobs may run at once. `None`:
    /// `min(4, available_parallelism)`.
    ///
    /// The default is small: an actor that runs many blocking jobs at once
    /// usually needs more, and under load the only symptom is
    /// `PipeError::NativeBudget` refusals (the first one per instance is
    /// logged as a `native_budget_exhausted` warning). Set it with
    /// `#[actor(max_native_jobs = N)]`.
    const MAX_NATIVE_JOBS: Option<usize> = None;

    fn started(&mut self, _ctx: &Context<Self>) {}
    fn stopped(&mut self, _ctx: &Context<Self>) {}

    /// Called when a linked actor stops, if this actor has called
    /// `ctx.trap_exit(true)`. Default impl ignores the signal.
    fn exit_received(&mut self, _exit: Exit, _ctx: &Context<Self>) {}
}

// ---------------------------------------------------------------------------
// Handler trait (per-message, sync version)
// ---------------------------------------------------------------------------

/// Per-message handler trait. Implement once for each message type the actor handles.
///
/// Unlike the `tasks` version, handlers are synchronous — no `async`/`.await`.
///
/// `handle` is the envelope dispatch for both `send` and `request`: answer
/// through `reply` ([`PendingReply::reply`], or [`PendingReply::defer`] to
/// answer later). A request whose `PendingReply` is dropped unanswered gets
/// `ReplyDropped`; a sent message has a detached `PendingReply`.
pub trait Handler<M: Message>: Actor {
    fn handle(&mut self, msg: M, reply: PendingReply<'_, M::Result>, ctx: &Context<Self>);
}

// ---------------------------------------------------------------------------
// Envelope (type-erasure)
// ---------------------------------------------------------------------------

trait Envelope<A: Actor>: Send {
    /// The model's mailbox item kind (`Lifecycle.tla`), for the trace.
    #[cfg(feature = "lifecycle-trace")]
    fn trace_kind(&self) -> &'static str {
        "other"
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
    fn discharge(&mut self) {}

    fn handle(self: Box<Self>, actor: &mut A, ctx: &Context<A>);
}

/// Answer every request queued in a dead owner's mailbox (and `extra`, an
/// envelope received after the stop), destroying nothing: the items come
/// back, to be destroyed once the death point's wake-ups ran. Admission is
/// closed first (waiting for in-flight sends), so nothing is queued after.
fn answer_mailbox<A: Actor>(
    rx: &mut mpsc::Receiver<MailboxItem<A>>,
    extra: Option<Box<dyn Envelope<A> + Send>>,
) -> Vec<MailboxItem<A>> {
    let mut queued = rx.drain();
    queued.extend(extra.map(MailboxItem::Message));
    for item in &mut queued {
        if let MailboxItem::Message(envelope) = item {
            envelope.discharge();
        }
    }
    queued
}

/// A never-polled actor's startup payload (its thread was never started).
type StartupPayload<A> = (
    A,
    Context<A>,
    mpsc::Receiver<MailboxItem<A>>,
    CancellationToken,
);
fn discharge_startup<A: Actor>(payload: &mut StartupPayload<A>) -> Vec<Box<dyn Send>> {
    erase(answer_mailbox(&mut payload.2, None))
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
    msg: M,
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
    fn discharge(&mut self) {
        drop(std::mem::replace(&mut self.slot, SlotHandle::detached()));
    }
    fn handle(self: Box<Self>, actor: &mut A, ctx: &Context<A>) {
        let mut this = *self;
        let reply = PendingReply::new(this.slot.take(), &ctx.replies);
        actor.handle(this.msg, reply, ctx);
    }
}

impl<A: Actor + Handler<Down>> Envelope<A> for DownEnvelope {
    #[cfg(feature = "lifecycle-trace")]
    fn trace_kind(&self) -> &'static str {
        "down"
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
    fn handle(self: Box<Self>, actor: &mut A, ctx: &Context<A>) {
        fail_point!("monitor.down_before_dispatch");
        if crate::monitor::fence::take_queued(&ctx.monitors, &self.down.monitor_ref) {
            let reply = PendingReply::new(None, &ctx.replies);
            actor.handle(self.down, reply, ctx);
        }
    }
}

/// Admit a message. Past the death point (the registry is sealed, or the
/// mailbox is gone) it reports `NoProc`; before it, including while the
/// actor exits, it is queued, and a request is answered at the death point
/// if it is not handled.
fn enqueue<A, M>(
    sender: &mpsc::Sender<MailboxItem<A>>,
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
        msg,
        slot,
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
    down: Down,
    #[cfg(feature = "metrics")]
    observation: Option<crate::instrumentation::MessageObservation>,
}

fn enqueue_down<A: Actor + Handler<Down>>(
    sender: &mpsc::Sender<MailboxItem<A>>,
    _mailbox: &crate::instrumentation::Mailbox,
    replies: &Registry,
    down: Down,
) -> Result<(), ActorError> {
    let envelope: Box<dyn Envelope<A> + Send> = Box::new(DownEnvelope {
        down,
        #[cfg(feature = "metrics")]
        observation: Some(_mailbox.enqueued()),
    });
    admit::<A, Down>(sender, replies, envelope, 0)
}

/// Admission of a built envelope (`M` names its kind for the trace).
fn admit<A: Actor, M: 'static>(
    sender: &mpsc::Sender<MailboxItem<A>>,
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
        match sender.send(MailboxItem::Message(envelope)) {
            Ok(()) => {
                #[cfg(feature = "metrics")]
                crate::instrumentation::accepted();
                crate::lifecycle_trace::life_send::<M>(owner, true, _from);
                Ok(())
            }
            Err(_error) => {
                #[cfg(feature = "metrics")]
                {
                    if let MailboxItem::Message(envelope) = _error.0 {
                        envelope.reject();
                    }
                }
                crate::lifecycle_trace::life_send::<M>(owner, false, _from);
                Err(ActorError::NoProc)
            }
        }
    })
}

/// A closure run in one of the actor's turns (`spawned_test::threads::in_actor`).
type InTurn<A, R> = Box<dyn FnOnce(&mut A, &Context<A>) -> R + Send>;
struct InTurnEnvelope<A: Actor, R: Send + 'static> {
    slot: SlotHandle<R>,
    f: InTurn<A, R>,
}

impl<A: Actor, R: Send + 'static> Envelope<A> for InTurnEnvelope<A, R> {
    fn discharge(&mut self) {
        drop(std::mem::replace(&mut self.slot, SlotHandle::detached()));
    }
    fn handle(self: Box<Self>, actor: &mut A, ctx: &Context<A>) {
        let InTurnEnvelope { f, mut slot } = *self;
        let reply = PendingReply::new(slot.take(), &ctx.replies);
        let value = f(actor, ctx);
        let _ = reply.reply(value);
    }
}

/// System envelope that dispatches an `Exit` notification to the actor's
/// `exit_received` callback. Does not require `Handler<Exit>` on the actor.
struct ExitEnvelope {
    exit: Exit,
}

impl<A: Actor> Envelope<A> for ExitEnvelope {
    /// The model's mailbox item kind (`Lifecycle.tla`), for the trace.
    #[cfg(feature = "lifecycle-trace")]
    fn trace_kind(&self) -> &'static str {
        "exit"
    }
    fn handle(self: Box<Self>, actor: &mut A, ctx: &Context<A>) {
        actor.exit_received(self.exit, ctx);
    }
}

/// A parent exit (E3): when dispatched, exit begins with the parent's reason
/// and `stopped()` runs. `exit_received` never sees it.
struct ParentExitEnvelope {
    exit: Exit,
}

impl<A: Actor> Envelope<A> for ParentExitEnvelope {
    /// The model's mailbox item kind (`Lifecycle.tla`), for the trace.
    #[cfg(feature = "lifecycle-trace")]
    fn trace_kind(&self) -> &'static str {
        "pexit"
    }
    fn handle(self: Box<Self>, _actor: &mut A, ctx: &Context<A>) {
        ctx.parent_exit(self.exit.reason);
    }
}

/// An exit signal that arrived while `started()` ran, in its mailbox place
/// (phase 0). If the actor trapped exits when `started()` returned it is an
/// `Exit` (or parent exit); otherwise it was resolved then and is inert.
struct HeldEnvelope {
    exit: Exit,
    parent: bool,
}

impl<A: Actor> Envelope<A> for HeldEnvelope {
    /// The model's mailbox item kind (`Lifecycle.tla`), for the trace.
    #[cfg(feature = "lifecycle-trace")]
    fn trace_kind(&self) -> &'static str {
        "held"
    }
    fn handle(self: Box<Self>, actor: &mut A, ctx: &Context<A>) {
        if !link::lock_request(&ctx.linked_reason).held_trapped() {
            return;
        }
        if self.parent {
            ctx.parent_exit(self.exit.reason);
        } else {
            actor.exit_received(self.exit, ctx);
        }
    }
}

/// Build the signal target of a new actor instance.
fn signal_target<A: Actor>(
    id: ActorId,
    parent: Option<ActorId>,
    sender: &mpsc::Sender<MailboxItem<A>>,
    trap: TrapExitFlag,
    request: LinkedExitReason,
    token: CancellationToken,
) -> Arc<SignalTarget> {
    let exit_tx = sender.clone();
    let parent_tx = sender.clone();
    let held_tx = sender.clone();
    let send_exit: SendExitFn = Arc::new(move |exit: Exit| {
        let envelope: Box<dyn Envelope<A> + Send> = Box::new(ExitEnvelope { exit });
        exit_tx
            .send(MailboxItem::Message(envelope))
            .map_err(|_| ActorError::NoProc)
    });
    let send_parent_exit: SendExitFn = Arc::new(move |exit: Exit| {
        let envelope: Box<dyn Envelope<A> + Send> = Box::new(ParentExitEnvelope { exit });
        parent_tx
            .send(MailboxItem::Message(envelope))
            .map_err(|_| ActorError::NoProc)
    });
    let send_held: SendHeldFn = Arc::new(move |exit: Exit, parent: bool| {
        let envelope: Box<dyn Envelope<A> + Send> = Box::new(HeldEnvelope { exit, parent });
        held_tx
            .send(MailboxItem::Message(envelope))
            .map_err(|_| ActorError::NoProc)
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

// A private wakeup is distinct from user envelopes and never invokes a handler.
enum MailboxItem<A: Actor> {
    Message(Box<dyn Envelope<A> + Send>),
    Shutdown,
}

fn register_stop_wakeup<A: Actor>(token: &CancellationToken, sender: mpsc::Sender<MailboxItem<A>>) {
    // One callback covers Context, ChildHandle, linked exits and framework cleanup.
    // CancellationToken drains this FnOnce callback exactly once, including when
    // multiple callers race to cancel. Repeated stop calls cannot grow the queue.
    token.on_cancel(Box::new(move || {
        let _ = sender.send(MailboxItem::Shutdown);
    }));
}

// ---------------------------------------------------------------------------
// Context
// ---------------------------------------------------------------------------

/// Handle passed to every handler and lifecycle hook, providing access to the
/// actor's mailbox and lifecycle controls.
///
/// Clone is cheap — it clones the inner channel sender and cancellation token.
pub struct Context<A: Actor> {
    id: ActorId,
    sender: mpsc::Sender<MailboxItem<A>>,
    cancellation_token: CancellationToken,
    completion: spawned_rt::sync::watch::Receiver<Option<ExitReason>>,
    monitors: MonitorTable,
    trap_exit: TrapExitFlag,
    links: LinkTable,
    linked_reason: LinkedExitReason,
    signal: Arc<SignalTarget>,
    mailbox: crate::instrumentation::Mailbox,
    replies: Arc<Registry>,
    jobs: Arc<crate::reply::jobs::JobSet>,
    cleaned: Arc<crate::reply::jobs::Cleaned>,
}

impl<A: Actor> Clone for Context<A> {
    fn clone(&self) -> Self {
        Self {
            id: self.id,
            sender: self.sender.clone(),
            cancellation_token: self.cancellation_token.clone(),
            completion: self.completion.clone(),
            monitors: self.monitors.clone(),
            trap_exit: self.trap_exit.clone(),
            links: self.links.clone(),
            linked_reason: self.linked_reason.clone(),
            signal: self.signal.clone(),
            mailbox: self.mailbox.clone(),
            replies: self.replies.clone(),
            jobs: self.jobs.clone(),
            cleaned: self.cleaned.clone(),
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
            id: actor_ref.id,
            sender: actor_ref.sender.clone(),
            cancellation_token: actor_ref.cancellation_token.clone(),
            completion: actor_ref.completion.clone(),
            monitors: actor_ref.monitors.clone(),
            trap_exit: actor_ref.trap_exit.clone(),
            links: actor_ref.links.clone(),
            linked_reason: actor_ref.linked_reason.clone(),
            signal: actor_ref.signal.clone(),
            mailbox: actor_ref.mailbox.clone(),
            replies: actor_ref.replies.clone(),
            jobs: actor_ref.jobs.clone(),
            cleaned: actor_ref.cleaned.clone(),
        }
    }

    /// Waiting messages, including in-flight enqueue reservations, excluding
    /// the active handler and internal exit/shutdown control envelopes.
    /// Concurrent observations are approximate; no registry or actor labels.
    #[cfg(feature = "metrics")]
    pub fn mailbox_depth(&self) -> u64 {
        self.mailbox.depth()
    }

    /// Deferred-reply credits in use by this instance: live deferred
    /// tokens plus running linked jobs (bounded by `Actor::MAX_OUTSTANDING`).
    /// Experimental in 0.8 (for tests and diagnostics; the `metrics`
    /// feature exports the same count): it may change before 1.0.
    pub fn deferred_outstanding(&self) -> usize {
        self.replies.outstanding()
    }

    /// The actor's unique identity.
    pub fn id(&self) -> ActorId {
        self.id
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
        link::stop_with_reason(self.id, &self.linked_reason, reason, || {
            self.cancellation_token.cancel()
        });
    }

    /// A parent exit being dispatched: normal termination with its reason
    /// (part of the model's `Dispatch`, so not recorded as a stop).
    fn parent_exit(&self, reason: ExitReason) {
        link::request_stop(&self.linked_reason, reason, || {
            self.cancellation_token.cancel()
        });
    }

    /// Send an exit signal from this actor to `target` (OTP
    /// `exit_signal(Target, Reason)`); see [`ChildHandle::exit`] for the
    /// delivery rules. From the target's `start_linked` parent the signal is a
    /// parent exit (a trapping target ends normally, through `stopped()`).
    /// Works until this actor's death point, including from `stopped()`;
    /// inert after it.
    pub fn exit(&self, target: &ChildHandle, reason: ExitReason) {
        // Held until delivered: the death point waits for it, so a signal
        // sent before death precedes the death notices (and none is sent
        // after it).
        let Some(_sending) = self.replies.signal_permit() else {
            return;
        };
        if link::lock_request(&self.linked_reason).is_dead() {
            return;
        }
        fail_point!("actor.exit_before_deliver");
        target.exit_from(self.id, reason, false);
    }

    /// Send a fire-and-forget message to this actor.
    pub fn send<M>(&self, msg: M) -> Result<(), ActorError>
    where
        A: Handler<M>,
        M: Message,
    {
        enqueue(
            &self.sender,
            &self.mailbox,
            &self.replies,
            msg,
            SlotHandle::detached(),
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
        let (slot, rx) = reply_slot(deadline, Some(&self.replies));
        enqueue(&self.sender, &self.mailbox, &self.replies, msg, slot)?;
        Ok(rx)
    }

    /// Send a request and block until the reply arrives (default 5s timeout).
    pub fn request<M>(&self, msg: M) -> Result<M::Result, ActorError>
    where
        A: Handler<M>,
        M: Message,
    {
        self.request_with_timeout(msg, DEFAULT_REQUEST_TIMEOUT)
    }

    /// Send a request and block until the reply arrives, with a custom timeout.
    pub fn request_with_timeout<M>(
        &self,
        msg: M,
        duration: Duration,
    ) -> Result<M::Result, ActorError>
    where
        A: Handler<M>,
        M: Message,
    {
        let observation = crate::instrumentation::RequestObservation::new();
        let result = match self.send_request(msg, Deadline::after(duration)) {
            Err(error) => Err(error),
            Ok(rx) => rx.recv(),
        };
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
            id: self.id,
            sender: self.sender.clone(),
            cancellation_token: self.cancellation_token.clone(),
            completion: self.completion.clone(),
            monitors: self.monitors.clone(),
            trap_exit: self.trap_exit.clone(),
            links: self.links.clone(),
            linked_reason: self.linked_reason.clone(),
            signal: self.signal.clone(),
            mailbox: self.mailbox.clone(),
            replies: self.replies.clone(),
            jobs: self.jobs.clone(),
            cleaned: self.cleaned.clone(),
        }
    }

    /// Set up a unidirectional monitor on another actor.
    ///
    /// Returns a [`MonitorRef`] that can be used to cancel the monitor via
    /// [`Context::demonitor`]. When the monitored actor stops, a [`Down`]
    /// message is delivered to this actor's mailbox via `Handler<Down>`.
    ///
    /// If the target is past its death point, exactly one
    /// `Down { reason: NoProc }` is delivered at once (OTP `noproc`).
    ///
    /// Multiple independent monitors are allowed on the same target — each
    /// call returns a distinct `MonitorRef`.
    ///
    /// Monitors are unidirectional: the monitored actor is unaware of the
    /// monitor and unaffected by it.
    /// Registration after owner shutdown starts (including in `stopped()`)
    /// returns an inert reference and creates no waiter or notification.
    ///
    /// **Resource cost (threads mode):** each active monitor occupies one OS
    /// thread until the target exits, the monitor is cancelled, or its owner
    /// stops, waiting on the target's completion signal. For supervisors with many long-lived
    /// children, consider using tasks mode instead.
    pub fn monitor(&self, target: &ChildHandle) -> MonitorRef
    where
        A: Handler<Down>,
    {
        fail_point!("monitor.before_install");
        let monitor_ref = MonitorRef::next();
        let active = spawned_rt::sync::CancellationToken::new();

        let observer = self.id.raw();
        let registered = crate::lifecycle_trace::life_section(observer, || {
            let event = |x| {
                crate::lifecycle_trace::LifeEvent::new("Monitor", observer)
                    .s(target.id().raw())
                    .m(monitor_ref.raw())
                    .x(x)
            };
            crate::monitor::fence::register(
                &self.monitors,
                monitor_ref,
                active.clone(),
                // The token check makes registration inert once shutdown
                // starts. The closed table serializes registration with exit
                // cleanup, even on paths that unwind before cancelling the
                // owner token.
                || self.cancellation_token.is_cancelled(),
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
                        &self.sender,
                        &self.mailbox,
                        &self.replies,
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
        let monitors = self.monitors.clone();
        let registration = crate::monitor::MonitorRegistration(monitors.clone(), monitor_ref);

        rt::spawn(move || {
            let _registration = registration;
            let reason = futures::executor::block_on(async {
                match futures::future::select(
                    Box::pin(active.cancelled()),
                    Box::pin(target.wait_exit_async()),
                )
                .await
                {
                    futures::future::Either::Left(_) => None,
                    futures::future::Either::Right((reason, _)) => Some(reason),
                }
            });
            let Some(reason) = reason else { return };
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
                        &actor_ref.sender,
                        &actor_ref.mailbox,
                        &actor_ref.replies,
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
        let observer = self.id.raw();
        let active = crate::lifecycle_trace::life_section(observer, || {
            crate::monitor::fence::demonitor(&self.monitors, &monitor_ref, || {
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
    /// `NoProc`. Use [`try_link`](Self::try_link) to get `Err(NoProc)`
    /// instead. Inert once this actor's exit began (in `stopped()`, from a
    /// helper after the stop, or past its death point): no record and no
    /// notice, as `monitor`. An existing link, or a pending notice from that peer, makes
    /// this a no-op.
    ///
    /// [`Actor::exit_received`]: Actor::exit_received
    pub fn link(&self, target: &ChildHandle) {
        link::register_link(
            &self.signal,
            &self.links,
            target.signal_target(),
            target.links(),
            false,
        );
    }

    /// Link as [`link`](Self::link), but report a target past its death
    /// point as `Err(NoProc)` and keep no record.
    pub fn try_link(&self, target: &ChildHandle) -> Result<(), NoProc> {
        match link::register_link(
            &self.signal,
            &self.links,
            target.signal_target(),
            target.links(),
            true,
        ) {
            LinkOutcome::NoProc => Err(NoProc),
            LinkOutcome::Linked | LinkOutcome::Unchanged => Ok(()),
        }
    }

    /// Remove a link. A fence (OTP `unlink/1`): once it returns, the link has
    /// no further effect on this actor; an `Exit` already queued stays.
    pub fn unlink(&self, target: &ChildHandle) {
        link::unregister_link(self.id, &self.links, target.id(), target.links());
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
        link::lock_request(&self.linked_reason).exit_pending()
    }

    /// Control how exit signals (link notices and `exit` signals) are
    /// handled; read when each signal arrives (OTP `trap_exit`).
    ///
    /// - `false` (default): `Normal` is ignored; any other reason ends this
    ///   actor after its current handler, without `stopped()`.
    /// - `true`: the signal is queued as an `Exit` for
    ///   [`Actor::exit_received`].
    ///
    /// Only a kill is untrappable. A `start_linked` parent's exit ends a
    /// trapping actor normally, through `stopped()`. Signals that arrive while
    /// `started()` runs are resolved when it returns.
    pub fn trap_exit(&self, enabled: bool) {
        let id = self.id.raw();
        crate::lifecycle_trace::life_section(id, || {
            // Recorded only when it changes (the model's SetTrap).
            if self.trap_exit.swap(enabled, Ordering::AcqRel) != enabled {
                crate::lifecycle_trace::life_emit(id, || {
                    crate::lifecycle_trace::LifeEvent::new("Trap", id)
                        .x(crate::lifecycle_trace::flag(enabled))
                });
            }
        });
    }

    pub(crate) fn cancellation_token(&self) -> CancellationToken {
        self.cancellation_token.clone()
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
}

/// Type-erased reference for sending a single message type.
pub type Recipient<M> = Arc<dyn Receiver<M>>;

/// Send a request through a type-erased `Receiver` with a custom timeout.
pub fn request<M: Message>(
    recipient: &dyn Receiver<M>,
    msg: M,
    timeout: Duration,
) -> Result<M::Result, ActorError> {
    let observation = crate::instrumentation::RequestObservation::new();
    let result = match recipient.send_request(msg, Deadline::after(timeout)) {
        Err(error) => Err(error),
        Ok(rx) => rx.recv(),
    };
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
    id: ActorId,
    sender: mpsc::Sender<MailboxItem<A>>,
    cancellation_token: CancellationToken,
    completion: spawned_rt::sync::watch::Receiver<Option<ExitReason>>,
    monitors: MonitorTable,
    trap_exit: TrapExitFlag,
    links: LinkTable,
    linked_reason: LinkedExitReason,
    signal: Arc<SignalTarget>,
    mailbox: crate::instrumentation::Mailbox,
    replies: Arc<Registry>,
    jobs: Arc<crate::reply::jobs::JobSet>,
    cleaned: Arc<crate::reply::jobs::Cleaned>,
}

impl<A: Actor> Debug for ActorRef<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActorRef").finish_non_exhaustive()
    }
}

impl<A: Actor> Clone for ActorRef<A> {
    fn clone(&self) -> Self {
        Self {
            id: self.id,
            sender: self.sender.clone(),
            cancellation_token: self.cancellation_token.clone(),
            completion: self.completion.clone(),
            monitors: self.monitors.clone(),
            trap_exit: self.trap_exit.clone(),
            links: self.links.clone(),
            linked_reason: self.linked_reason.clone(),
            signal: self.signal.clone(),
            mailbox: self.mailbox.clone(),
            replies: self.replies.clone(),
            jobs: self.jobs.clone(),
            cleaned: self.cleaned.clone(),
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

    /// Send a fire-and-forget message to the actor.
    pub fn send<M>(&self, msg: M) -> Result<(), ActorError>
    where
        A: Handler<M>,
        M: Message,
    {
        enqueue(
            &self.sender,
            &self.mailbox,
            &self.replies,
            msg,
            SlotHandle::detached(),
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
        let (slot, rx) = reply_slot(deadline, Some(&self.replies));
        enqueue(&self.sender, &self.mailbox, &self.replies, msg, slot)?;
        Ok(rx)
    }

    /// Send a request and block until the reply arrives (default 5s timeout).
    pub fn request<M>(&self, msg: M) -> Result<M::Result, ActorError>
    where
        A: Handler<M>,
        M: Message,
    {
        self.request_with_timeout(msg, DEFAULT_REQUEST_TIMEOUT)
    }

    /// Run `f` in one of the actor's turns, with its state and `Context`,
    /// and receive its result. For tests: `spawned_test::threads::in_actor` (through
    /// `__private`).
    pub(crate) fn in_turn<R, F>(&self, f: F) -> Result<ReplyReceiver<R>, ActorError>
    where
        R: Send + 'static,
        F: FnOnce(&mut A, &Context<A>) -> R + Send + 'static,
    {
        let (slot, rx) = reply_slot(Deadline::Never, Some(&self.replies));
        let envelope: Box<dyn Envelope<A> + Send> = Box::new(InTurnEnvelope {
            f: Box::new(f),
            slot,
        });
        // Past the death point the actor is gone (E4), as for `enqueue`.
        if self.replies.refuses_admission() {
            return Err(ActorError::NoProc);
        }
        self.sender
            .send(MailboxItem::Message(envelope))
            .map_err(|_| ActorError::NoProc)?;
        Ok(rx)
    }

    /// Send a request and block until the reply arrives, with a custom timeout.
    pub fn request_with_timeout<M>(
        &self,
        msg: M,
        duration: Duration,
    ) -> Result<M::Result, ActorError>
    where
        A: Handler<M>,
        M: Message,
    {
        let observation = crate::instrumentation::RequestObservation::new();
        let result = match self.send_request(msg, Deadline::after(duration)) {
            Err(error) => Err(error),
            Ok(rx) => rx.recv(),
        };
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

    /// Block until the actor has fully stopped (including `stopped()` callback).
    ///
    /// Returns when cleanup completes: the actor died and its cancelled jobs
    /// were joined (bounded by the cleanup deadline). Use
    /// [`wait_exit`](Self::wait_exit) to observe death alone.
    pub fn join(&self) {
        let _ = self.wait_exit();
        self.cleaned.wait();
    }

    /// Poll the exit reason. Returns `None` until the exit is published.
    /// A completion channel closed without a reason reports
    /// [`ExitReason::Killed`], matching the wait methods.
    pub fn exit_reason(&self) -> Option<ExitReason> {
        crate::child_handle::poll_exit_reason(&self.completion)
    }

    /// Block until the actor stops and return the exit reason.
    pub fn wait_exit(&self) -> ExitReason {
        crate::child_handle::wait_for_thread_exit_blocking(self.completion.clone())
    }

    /// Waiting messages, including in-flight enqueue reservations, excluding
    /// the active handler and internal exit/shutdown control envelopes.
    /// Concurrent observations are approximate; no registry or actor labels.
    #[cfg(feature = "metrics")]
    pub fn mailbox_depth(&self) -> u64 {
        self.mailbox.depth()
    }

    /// The actor's unique identity.
    pub fn id(&self) -> ActorId {
        self.id
    }

    /// Get a type-erased `ChildHandle` for this actor.
    pub fn child_handle(&self) -> ChildHandle {
        ChildHandle::from(self.clone())
    }
}

impl<A: Actor> From<ActorRef<A>> for ChildHandle {
    fn from(actor_ref: ActorRef<A>) -> Self {
        let failures = actor_ref.sender.failures();
        ChildHandle::new(
            actor_ref.signal,
            Completion::Threads(actor_ref.completion),
            actor_ref.links,
        )
        .with_mailbox_failures(failures)
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
}

// ---------------------------------------------------------------------------
// Actor startup + main loop
// ---------------------------------------------------------------------------

impl<A: Actor> ActorRef<A> {
    fn spawn(actor: A, parent: Option<ChildHandle>) -> Self {
        let (tx, rx) = mpsc::channel::<MailboxItem<A>>();
        let cancellation_token = CancellationToken::new();
        register_stop_wakeup(&cancellation_token, tx.clone());
        let (completion_tx, completion) = spawned_rt::sync::watch::channel(None);
        let id = ActorId::next();
        let monitors: MonitorTable = Arc::new(crate::monitor::fence::open());
        let trap_exit = new_trap_exit_flag();
        let links = new_link_table();
        let linked_reason = new_linked_exit_reason();
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

        let actor_ref = ActorRef {
            id,
            sender: tx.clone(),
            cancellation_token: cancellation_token.clone(),
            completion: completion.clone(),
            monitors: monitors.clone(),
            trap_exit: trap_exit.clone(),
            links: links.clone(),
            linked_reason: linked_reason.clone(),
            signal: signal.clone(),
            mailbox: crate::instrumentation::Mailbox::default(),
            replies: Registry::new(id, A::MAX_OUTSTANDING),
            jobs: Arc::new(crate::reply::jobs::JobSet::new(
                id,
                A::MAX_NATIVE_JOBS.unwrap_or_else(crate::reply::jobs::default_max_native_jobs),
            )),
            cleaned: Arc::default(),
        };

        let ctx = Context {
            id,
            sender: tx,
            cancellation_token: cancellation_token.clone(),
            completion: actor_ref.completion.clone(),
            monitors,
            trap_exit,
            links: links.clone(),
            linked_reason: linked_reason.clone(),
            signal,
            mailbox: actor_ref.mailbox.clone(),
            replies: actor_ref.replies.clone(),
            jobs: actor_ref.jobs.clone(),
            cleaned: actor_ref.cleaned.clone(),
        };

        let parent_id = parent.as_ref().map_or(0, |p| p.id().raw());
        if let Some(parent) = parent {
            actor_ref.context().link(&parent);
        }
        // Recorded from here: the start_linked link is the model's initial one.
        crate::lifecycle_trace::life_spawn(id.raw(), parent_id);

        let cleanup_token = cancellation_token.clone();
        let cleanup = crate::actor_cleanup::ActorCleanup::new(
            id,
            links,
            linked_reason,
            actor_ref.monitors.clone(),
            completion_tx,
            move || cleanup_token.cancel(),
        )
        .with_mailbox_failures(actor_ref.sender.failures())
        .with_replies(actor_ref.replies.clone())
        .with_jobs(actor_ref.jobs.clone());
        let startup = crate::actor_cleanup::ActorStartup::new(
            (actor, ctx, rx, cancellation_token),
            cleanup,
            discharge_startup::<A>,
        );
        let replies = actor_ref.replies.clone();
        let jobs = actor_ref.jobs.clone();
        let cleaned = crate::reply::jobs::CleanedGuard(actor_ref.cleaned.clone());
        let _thread_handle = rt::spawn(move || {
            let _cleaned = cleaned;
            let ((mut actor, ctx, mut rx, cancellation_token), mut cleanup) = startup.into_parts();
            let request = ctx.linked_reason.clone();
            let discarded = match catch_unwind(AssertUnwindSafe(|| {
                run_actor(&mut actor, ctx, &mut rx, cancellation_token)
            })) {
                Ok(discarded) => discarded,
                Err(payload) => {
                    let message = panic_message(&*payload);
                    crate::mailbox::dispose_panic(payload);
                    link::lock_request(&request).panic(ExitReason::Panic(format!(
                        "actor lifecycle unwound: {message}"
                    )));
                    None
                }
            };
            // The death point D, only now that `stopped()` returned (or was
            // skipped): the reason becomes final, the link table closes, job
            // admission and the registry close (every open token is
            // answered), then the mailbox refuses admission and its queued
            // requests (and one received after the stop) are answered, all
            // before any destructor.
            cleanup.die();
            let queued = answer_mailbox(&mut rx, discarded);
            // Every caller is answered: wake them, then destroy (queued
            // payloads, then state).
            cleanup.discharged();
            rx.discard_all(queued);
            drop(rx);
            // Destruction: a destructor panic is a cleanup failure only.
            if let Some(failure) = crate::actor_cleanup::finish_actor_state(actor) {
                cleanup.cleanup_failed(failure);
            }
            // Publication (wait_exit, monitors, link notices) before the job join.
            drop(cleanup);
            // Job cleanup (§5 step 4), after death: join the cancelled jobs
            // that signal completion, bounded by the cleanup deadline.
            if jobs.running() > 0 {
                let remaining = replies.cleanup_remaining(DEFAULT_STOP_DEADLINE);
                let unfinished = jobs.join_blocking(std::time::Instant::now() + remaining);
                crate::reply::jobs::report_unfinished(jobs.owner(), unfinished);
            }
        });

        actor_ref
    }
}

/// Returns an envelope received after the stop (undispatched; answered at
/// the death point).
fn run_actor<A: Actor>(
    actor: &mut A,
    ctx: Context<A>,
    rx: &mut mpsc::Receiver<MailboxItem<A>>,
    cancellation_token: CancellationToken,
) -> Option<Box<dyn Envelope<A> + Send>> {
    run_actor_receiving(
        actor,
        ctx,
        move || {
            rx.recv()
                .map_err(|_| std::sync::mpsc::RecvTimeoutError::Disconnected)
        },
        cancellation_token,
    )
}

fn run_actor_receiving<A: Actor>(
    actor: &mut A,
    ctx: Context<A>,
    mut receive: impl FnMut() -> Result<MailboxItem<A>, std::sync::mpsc::RecvTimeoutError>,
    cancellation_token: CancellationToken,
) -> Option<Box<dyn Envelope<A> + Send>> {
    let mut discarded = None;
    let request = ctx.linked_reason.clone();
    let trace_id = ctx.id.raw();
    let _monitor_cleanup = MonitorCleanup(ctx.monitors.clone());
    let start_result = catch_unwind(AssertUnwindSafe(|| {
        fail_point!("actor.before_started");
        crate::lifecycle_trace::with_actor(trace_id, || actor.started(&ctx));
    }));
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
        return None;
    }
    // Phase 0 ends: resolve signals held during started() with the trap
    // flag read now.
    if link::lock_request(&request).end_starting(ctx.trap_exit.load(Ordering::Acquire)) {
        cancellation_token.cancel();
    }

    while !cancellation_token.is_cancelled() {
        let msg = match receive() {
            Ok(MailboxItem::Message(msg)) => Some(msg),
            Ok(MailboxItem::Shutdown) => None,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => None,
        };
        match msg {
            Some(envelope) => {
                #[cfg(feature = "metrics")]
                let mut envelope = envelope;

                // A stop observed after receive wins over dispatch, whether
                // the channel waited or was buffered. Defer the message drop.
                // The check and the trace event are one lifecycle-trace step.
                let stopped = crate::lifecycle_trace::life_section(trace_id, || {
                    if cancellation_token.is_cancelled() {
                        return true;
                    }
                    #[cfg(feature = "lifecycle-trace")]
                    crate::lifecycle_trace::life_emit(trace_id, || {
                        crate::lifecycle_trace::LifeEvent::new("Dispatch", trace_id)
                            .k(envelope.trace_kind())
                    });
                    false
                });
                if stopped {
                    discarded = Some(envelope);
                    break;
                }
                #[cfg(feature = "metrics")]
                let observation = envelope
                    .observation()
                    .map(|observation| observation.begin());
                ctx.replies.begin_turn();
                let result = catch_unwind(AssertUnwindSafe(|| {
                    crate::lifecycle_trace::with_actor(trace_id, || envelope.handle(actor, &ctx));
                }));
                ctx.replies.end_turn(result.is_err());
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
            }
            None => break,
        }
    }

    cancellation_token.cancel();
    ctx.replies.exiting();
    // Payload drops are held until every caller is answered (DropFailures).
    ctx.sender.failures().hold();
    ctx.jobs.cancel_all();
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
            ctx.id,
            std::any::type_name::<A>(),
            cause,
            reason.as_ref(),
        );
    }
    if run_hook {
        let stop_result = catch_unwind(AssertUnwindSafe(|| {
            fail_point!("actor.before_stopped");
            crate::lifecycle_trace::with_actor(trace_id, || actor.stopped(&ctx));
        }));
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

    // Message destructors and reply cancellation may re-enter monitor state.
    // Preserve stopped() -> monitor cleanup -> undelivered-message drop (the
    // envelope received after the stop is answered at the death point, then
    // dropped with the mailbox).
    drop(_monitor_cleanup);
    discarded
}

// ---------------------------------------------------------------------------
// Actor::start
// ---------------------------------------------------------------------------

/// Extension trait for starting an actor. Automatically implemented for all [`Actor`] types.
pub trait ActorStart: Actor {
    /// Start the actor on a dedicated OS thread.
    fn start(self) -> ActorRef<Self> {
        ActorRef::spawn(self, None)
    }

    /// Start linked to `parent_ctx`'s actor, which becomes this instance's
    /// parent (E3). The link is registered before `started()`. The parent's
    /// exit ends a trapping child normally (its `stopped()` runs; it never
    /// reaches `exit_received`), a non-trapping child as any link notice.
    fn start_linked<P: Actor>(self, parent_ctx: &Context<P>) -> ActorRef<Self> {
        ActorRef::spawn(self, Some(parent_ctx.actor_ref().child_handle()))
    }
}

impl<A: Actor> ActorStart for A {}

// ---------------------------------------------------------------------------
// send_message_on (utility)
// ---------------------------------------------------------------------------

/// Send a message to an actor when a blocking closure completes.
///
/// Spawns a thread that runs `f()`, then sends `msg` to the actor.
/// If the actor stops before `f()` returns, the message is not sent.
pub fn send_message_on<A, M, F>(ctx: Context<A>, f: F, msg: M) -> rt::JoinHandle<()>
where
    A: Actor + Handler<M>,
    M: Message,
    F: FnOnce() + Send + 'static,
{
    let cancellation_token = ctx.cancellation_token();
    rt::spawn(move || {
        f();
        if !cancellation_token.is_cancelled() {
            if let Err(e) = ctx.send(msg) {
                tracing::error!("Failed to send message: {e:?}")
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::Message;
    use std::thread;

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
        fn handle(
            &mut self,
            _msg: GetCount,
            reply: crate::reply::PendingReply<'_, u64>,
            _ctx: &Context<Self>,
        ) {
            reply.reply(self.count);
        }
    }

    impl Handler<Increment> for Counter {
        fn handle(
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
        fn handle(
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
    fn basic_send_and_request() {
        let actor = Counter { count: 0 }.start();
        assert_eq!(actor.request(GetCount).unwrap(), 0);
        assert_eq!(actor.request(Increment).unwrap(), 1);
        // Same mailbox: the request is handled after the send.
        actor.send(Increment).unwrap();
        assert_eq!(actor.request(GetCount).unwrap(), 2);
        actor.request(StopCounter).unwrap();
    }

    #[test]
    fn join_waits_for_completion() {
        struct SlowStop;
        struct StopSlow;
        impl Message for StopSlow {
            type Result = ();
        }
        impl Actor for SlowStop {
            fn stopped(&mut self, _ctx: &Context<Self>) {
                rt::sleep(Duration::from_millis(300));
            }
        }
        impl Handler<StopSlow> for SlowStop {
            fn handle(
                &mut self,
                _msg: StopSlow,
                reply: crate::reply::PendingReply<'_, ()>,
                ctx: &Context<Self>,
            ) {
                ctx.stop();
                reply.reply(());
            }
        }

        let actor = SlowStop.start();
        actor.send(StopSlow).unwrap();
        actor.join();
        // If join() returned, stopped() has completed
    }

    #[test]
    fn join_multiple_callers() {
        struct SlowStop2;
        struct StopSlow2;
        impl Message for StopSlow2 {
            type Result = ();
        }
        impl Actor for SlowStop2 {
            fn stopped(&mut self, _ctx: &Context<Self>) {
                rt::sleep(Duration::from_millis(200));
            }
        }
        impl Handler<StopSlow2> for SlowStop2 {
            fn handle(
                &mut self,
                _msg: StopSlow2,
                reply: crate::reply::PendingReply<'_, ()>,
                ctx: &Context<Self>,
            ) {
                ctx.stop();
                reply.reply(());
            }
        }

        let actor = SlowStop2.start();
        let a1 = actor.clone();
        let a2 = actor.clone();
        let t1 = thread::spawn(move || {
            a1.join();
            1u32
        });
        let t2 = thread::spawn(move || {
            a2.join();
            2u32
        });
        actor.send(StopSlow2).unwrap();
        assert_eq!(t1.join().unwrap(), 1);
        assert_eq!(t2.join().unwrap(), 2);
    }

    #[test]
    fn panic_in_started_stops_actor() {
        struct PanicOnStart;
        struct PingThread;
        impl Message for PingThread {
            type Result = ();
        }
        impl Actor for PanicOnStart {
            fn started(&mut self, _ctx: &Context<Self>) {
                panic!("boom in started");
            }
        }
        impl Handler<PingThread> for PanicOnStart {
            fn handle(
                &mut self,
                _msg: PingThread,
                reply: crate::reply::PendingReply<'_, ()>,
                _ctx: &Context<Self>,
            ) {
                reply.reply(());
            }
        }

        let actor = PanicOnStart.start();
        actor.join();
        let result = actor.send(PingThread);
        assert!(result.is_err());
    }

    #[test]
    fn panic_in_handler_stops_actor() {
        struct PanicOnMsg;
        struct ExplodeThread;
        impl Message for ExplodeThread {
            type Result = ();
        }
        struct CheckThread;
        impl Message for CheckThread {
            type Result = u32;
        }
        impl Actor for PanicOnMsg {}
        impl Handler<ExplodeThread> for PanicOnMsg {
            fn handle(
                &mut self,
                _msg: ExplodeThread,
                _reply: crate::reply::PendingReply<'_, ()>,
                _ctx: &Context<Self>,
            ) {
                {
                    panic!("boom in handler");
                }
            }
        }
        impl Handler<CheckThread> for PanicOnMsg {
            fn handle(
                &mut self,
                _msg: CheckThread,
                reply: crate::reply::PendingReply<'_, u32>,
                _ctx: &Context<Self>,
            ) {
                reply.reply(42);
            }
        }

        let actor = PanicOnMsg.start();
        actor.send(ExplodeThread).unwrap();
        actor.join();
        let result = actor.request(CheckThread);
        assert!(result.is_err());
    }

    #[test]
    fn panic_in_stopped_still_completes() {
        struct PanicOnStop;
        struct StopMeThread;
        impl Message for StopMeThread {
            type Result = ();
        }
        impl Actor for PanicOnStop {
            fn stopped(&mut self, _ctx: &Context<Self>) {
                panic!("boom in stopped");
            }
        }
        impl Handler<StopMeThread> for PanicOnStop {
            fn handle(
                &mut self,
                _msg: StopMeThread,
                reply: crate::reply::PendingReply<'_, ()>,
                ctx: &Context<Self>,
            ) {
                ctx.stop();
                reply.reply(());
            }
        }

        let actor = PanicOnStop.start();
        actor.send(StopMeThread).unwrap();
        actor.join();
    }

    #[test]
    fn recipient_type_erasure() {
        let actor = Counter { count: 42 }.start();
        let recipient: Recipient<GetCount> = actor.recipient();
        let result = request(&*recipient, GetCount, Duration::from_secs(5)).unwrap();
        assert_eq!(result, 42);
    }

    #[test]
    fn send_message_on_delivers() {
        let actor = Counter { count: 0 }.start();
        let ctx = actor.context();
        send_message_on(ctx, || rt::sleep(Duration::from_millis(10)), Increment);
        let count =
            crate::test_wait::wait_for(|| actor.request(GetCount), |c| matches!(c, Ok(1))).unwrap();
        assert_eq!(count, 1);
    }

    // --- ExitReason tests ---

    #[test]
    fn exit_reason_normal_on_clean_stop() {
        let actor = Counter { count: 0 }.start();
        actor.request(StopCounter).unwrap();
        let reason = actor.wait_exit();
        assert!(matches!(reason, ExitReason::Normal));
    }

    #[test]
    fn exit_reason_panic_in_started() {
        struct PanicStartThread;
        struct PingThread2;
        impl Message for PingThread2 {
            type Result = ();
        }
        impl Actor for PanicStartThread {
            fn started(&mut self, _ctx: &Context<Self>) {
                panic!("boom in started");
            }
        }
        impl Handler<PingThread2> for PanicStartThread {
            fn handle(
                &mut self,
                _msg: PingThread2,
                reply: crate::reply::PendingReply<'_, ()>,
                _ctx: &Context<Self>,
            ) {
                reply.reply(());
            }
        }

        let actor = PanicStartThread.start();
        let reason = actor.wait_exit();
        assert!(matches!(reason, ExitReason::Panic(ref msg) if msg.contains("boom in started")));
    }

    #[test]
    fn exit_reason_panic_in_handler() {
        struct PanicHandlerThread;
        struct ExplodeThread2;
        impl Message for ExplodeThread2 {
            type Result = ();
        }
        impl Actor for PanicHandlerThread {}
        impl Handler<ExplodeThread2> for PanicHandlerThread {
            fn handle(
                &mut self,
                _msg: ExplodeThread2,
                _reply: crate::reply::PendingReply<'_, ()>,
                _ctx: &Context<Self>,
            ) {
                {
                    panic!("boom in handler");
                }
            }
        }

        let actor = PanicHandlerThread.start();
        let _ = actor.send(ExplodeThread2);
        let reason = actor.wait_exit();
        assert!(matches!(reason, ExitReason::Panic(ref msg) if msg.contains("boom in handler")));
    }

    #[test]
    fn exit_reason_poll_returns_none_while_running() {
        let actor = Counter { count: 0 }.start();
        assert!(actor.exit_reason().is_none());
        actor.request(StopCounter).unwrap();
        actor.join();
        assert!(actor.exit_reason().is_some());
    }

    // --- Monitor tests ---

    struct GetDowns;
    impl Message for GetDowns {
        type Result = Vec<crate::monitor::Down>;
    }

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
        fn handle(
            &mut self,
            msg: StartMonitor,
            reply: crate::reply::PendingReply<'_, crate::monitor::MonitorRef>,
            ctx: &Context<Self>,
        ) {
            reply.reply(ctx.monitor(&msg.0));
        }
    }

    impl Handler<MonitorThenDemonitor> for Watcher {
        fn handle(
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
        fn handle(
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
        fn handle(
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
        fn handle(
            &mut self,
            _msg: GetDowns,
            reply: crate::reply::PendingReply<'_, Vec<crate::monitor::Down>>,
            _ctx: &Context<Self>,
        ) {
            reply.reply(self.downs.lock().unwrap().clone());
        }
    }

    fn make_watcher() -> ActorRef<Watcher> {
        Watcher {
            downs: Arc::new(Mutex::new(Vec::new())),
        }
        .start()
    }

    #[test]
    fn monitor_running_actor_delivers_down_on_exit() {
        let target = Counter { count: 0 }.start();
        let target_handle = target.child_handle();
        let watcher = make_watcher();

        let monitor_ref = watcher.request(StartMonitor(target_handle)).unwrap();

        target.request(StopCounter).unwrap();
        target.join();
        let downs = crate::test_wait::wait_for(
            || watcher.request(GetDowns),
            |downs| downs.as_ref().map_or(true, |d| !d.is_empty()),
        )
        .unwrap();
        assert_eq!(downs.len(), 1);
        assert_eq!(downs[0].monitor_ref, monitor_ref);
        assert!(matches!(downs[0].reason, ExitReason::Normal));
    }

    /// OTP `demonitor(Ref, [flush])`: a `Down` already in the mailbox
    /// (here the `NoProc` a monitor of a dead target queues at once) is
    /// never handled once `demonitor` returned.
    #[test]
    fn demonitor_flushes_a_queued_down() {
        let target = Counter { count: 0 }.start();
        target.request(StopCounter).unwrap();
        target.join();
        let watcher = make_watcher();
        watcher
            .request(MonitorThenDemonitor(target.child_handle()))
            .unwrap();
        // FIFO: the queued Down is dispatched before this request.
        let downs = watcher.request(GetDowns).unwrap();
        assert!(downs.is_empty(), "a flushed Down was handled: {downs:?}");
    }

    #[test]
    fn monitor_already_dead_actor_delivers_down_immediately() {
        let target = Counter { count: 0 }.start();
        target.request(StopCounter).unwrap();
        target.join();
        let target_handle = target.child_handle();

        let watcher = make_watcher();
        let _ = watcher.request(StartMonitor(target_handle)).unwrap();
        let downs = crate::test_wait::wait_for(
            || watcher.request(GetDowns),
            |downs| downs.as_ref().map_or(true, |d| !d.is_empty()),
        )
        .unwrap();
        assert_eq!(downs.len(), 1);
    }

    #[test]
    fn demonitor_before_target_dies_suppresses_down() {
        let target = Counter { count: 0 }.start();
        let target_handle = target.child_handle();
        let watcher = make_watcher();

        let monitor_ref = watcher.request(StartMonitor(target_handle)).unwrap();
        watcher.request(CallDemonitor(monitor_ref)).unwrap();

        target.request(StopCounter).unwrap();
        target.join();
        rt::sleep(Duration::from_millis(150));

        let downs = watcher.request(GetDowns).unwrap();
        assert!(downs.is_empty());
    }

    #[test]
    fn multiple_monitors_each_get_own_ref_and_down() {
        let target = Counter { count: 0 }.start();
        let target_handle = target.child_handle();
        let watcher = make_watcher();

        let r1 = watcher
            .request(StartMonitor(target_handle.clone()))
            .unwrap();
        let r2 = watcher.request(StartMonitor(target_handle)).unwrap();
        assert_ne!(r1, r2);

        target.request(StopCounter).unwrap();
        target.join();
        let downs = crate::test_wait::wait_for(
            || watcher.request(GetDowns),
            |downs| downs.as_ref().map_or(true, |d| d.len() >= 2),
        )
        .unwrap();
        assert_eq!(downs.len(), 2);
        let refs: Vec<_> = downs.iter().map(|d| d.monitor_ref).collect();
        assert!(refs.contains(&r1));
        assert!(refs.contains(&r2));
    }

    #[test]
    fn monitor_observes_panic_reason() {
        struct PanicMsg;
        impl Message for PanicMsg {
            type Result = ();
        }
        struct PanicMe;
        impl Actor for PanicMe {}
        impl Handler<PanicMsg> for PanicMe {
            fn handle(
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

        let target = PanicMe.start();
        let target_handle = target.child_handle();
        let watcher = make_watcher();

        let _ = watcher.request(StartMonitor(target_handle)).unwrap();
        let _ = target.send(PanicMsg);

        let downs = crate::test_wait::wait_for(
            || watcher.request(GetDowns),
            |downs| downs.as_ref().map_or(true, |d| !d.is_empty()),
        )
        .unwrap();
        assert_eq!(downs.len(), 1);
        assert!(matches!(downs[0].reason, ExitReason::Panic(_)));
    }

    // --- Link tests ---

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
        fn started(&mut self, ctx: &Context<Self>) {
            ctx.trap_exit(self.trap);
        }
        fn exit_received(&mut self, exit: Exit, _ctx: &Context<Self>) {
            self.exits.lock().unwrap().push(exit);
        }
    }

    impl Handler<LinkTo> for TrapActor {
        fn handle(
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
        fn handle(
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
    fn link_propagates_panic_to_non_trapping_peer_threads() {
        struct Boom;
        impl Message for Boom {
            type Result = ();
        }
        impl Handler<Boom> for TrapActor {
            fn handle(
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

        let a = make_trapper(false);
        let b = make_trapper(false);
        a.request(LinkTo(b.child_handle())).unwrap();

        let _ = a.send(Boom);
        let reason_a = a.wait_exit();
        let reason_b = b.wait_exit();
        assert!(matches!(reason_a, ExitReason::Panic(_)));
        assert!(matches!(reason_b, ExitReason::Panic(_)));
    }

    #[test]
    fn link_delivers_exit_to_trapping_peer_threads() {
        struct Boom;
        impl Message for Boom {
            type Result = ();
        }
        impl Handler<Boom> for TrapActor {
            fn handle(
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

        let a = make_trapper(false);
        let b = make_trapper(true);
        b.request(LinkTo(a.child_handle())).unwrap();

        let _ = a.send(Boom);
        a.wait_exit();
        let exits = crate::test_wait::wait_for(
            || b.request(GetExits),
            |exits| exits.as_ref().map_or(true, |e| !e.is_empty()),
        )
        .unwrap();
        assert!(b.exit_reason().is_none());
        assert_eq!(exits.len(), 1);
        assert_eq!(exits[0].from, a.id());
        assert!(matches!(exits[0].reason, ExitReason::Panic(_)));

        let bh = b.child_handle();
        bh.stop();
        bh.wait_exit_blocking();
    }

    #[test]
    fn start_linked_threads() {
        struct Parent;
        struct PanicParent;
        impl Message for PanicParent {
            type Result = ();
        }
        impl Actor for Parent {}
        impl Handler<PanicParent> for Parent {
            fn handle(
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

        let parent = Parent.start();
        let child = Child.start_linked(&parent.context());

        let _ = parent.send(PanicParent);
        parent.wait_exit();
        let reason = child.wait_exit();
        assert!(matches!(reason, ExitReason::Panic(_)));
    }
}

#[cfg(test)]
mod monitor_cleanup_regressions {
    use super::*;
    struct Idle;
    impl Actor for Idle {}
    impl Handler<Down> for Idle {
        fn handle(
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
        let observer = Idle.start();
        let target = Idle.start();
        // Model cleanup during an unexpected unwind before token cancellation.
        drop(MonitorCleanup(observer.monitors.clone()));
        let child = target.child_handle();
        let waiter = child.lifetime_probe();
        observer.context().monitor(&child);
        drop(child);
        assert_eq!(waiter.strong_count(), 0);
        assert!(observer.monitors.lock().unwrap().is_none());
        observer.child_handle().stop();
        target.child_handle().stop();
        observer.join();
        target.join();
    }
    fn check_cleanup(stop_owner: bool) {
        let target = Idle.start();
        let observer = Idle.start();
        let child = target.child_handle();
        let waiter = child.lifetime_probe();
        let monitor = observer.context().monitor(&child);
        drop(child);
        if stop_owner {
            observer.child_handle().stop();
            observer.join();
            drop(observer);
        } else {
            observer.context().demonitor(monitor);
            // Only the waiter now owns this particular ChildHandle allocation.
            let released = crate::test_wait::wait_for(|| waiter.strong_count() == 0, |r| *r);
            observer.child_handle().stop();
            observer.join();
            drop(observer);
            target.child_handle().stop();
            target.join();
            assert!(released, "demonitor retained its waiter until target exit");
            return;
        }
        let released = crate::test_wait::wait_for(|| waiter.strong_count() == 0, |r| *r);
        target.child_handle().stop();
        target.join();
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
        fn started(&mut self, ctx: &Context<Self>) {
            ctx.stop();
        }
        fn stopped(&mut self, _: &Context<Self>) {
            panic!("cleanup failed");
        }
    }
    #[test]
    fn panic_during_early_stop_is_reported() {
        let actor = EarlyStop.start();
        assert!(
            matches!(actor.wait_exit(), ExitReason::Panic(message) if message.contains("cleanup failed"))
        );
    }
}

#[cfg(test)]
mod idle_stop_regression {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Barrier,
    };
    struct Counter(Arc<AtomicUsize>, Arc<Mutex<Vec<&'static str>>>);
    impl Actor for Counter {
        fn stopped(&mut self, _: &Context<Self>) {
            self.1.lock().unwrap().push("stopped");
        }
    }
    impl Drop for Counter {
        fn drop(&mut self) {
            self.1.lock().unwrap().push("actor-dropped");
        }
    }
    struct Increment(Arc<Mutex<Vec<&'static str>>>, MonitorTable);
    impl Drop for Increment {
        fn drop(&mut self) {
            assert!(
                self.1.lock().unwrap().is_none(),
                "message dropped before monitor cleanup"
            );
            self.0.lock().unwrap().push("message-dropped");
        }
    }
    impl Message for Increment {
        type Result = ();
    }
    impl Handler<Increment> for Counter {
        fn handle(
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
    fn stop_while_receiving_does_not_dispatch_a_later_request() {
        let (tx, rx) = mpsc::channel();
        let token = CancellationToken::new();
        let (_, completion_rx) = spawned_rt::sync::watch::channel(None);
        let monitors = Arc::new(Mutex::new(Some(HashMap::new())));
        let events = Arc::new(Mutex::new(Vec::new()));
        let (id, trap, request) = (
            ActorId::next(),
            new_trap_exit_flag(),
            new_linked_exit_reason(),
        );
        let ctx = Context {
            id,
            sender: tx.clone(),
            cancellation_token: token.clone(),
            completion: completion_rx,
            monitors: monitors.clone(),
            trap_exit: trap.clone(),
            links: new_link_table(),
            linked_reason: request.clone(),
            signal: signal_target::<_>(id, None, &tx, trap.clone(), request.clone(), token.clone()),
            mailbox: crate::instrumentation::Mailbox::default(),
            replies: Registry::new(ActorId::next(), DEFAULT_MAX_OUTSTANDING),
            jobs: Arc::new(crate::reply::jobs::JobSet::new(ActorId::next(), 1)),
            cleaned: Arc::default(),
        };
        let count = Arc::new(AtomicUsize::new(0));
        let seen = count.clone();
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let (e, r) = (entered.clone(), release.clone());
        let cancel = token.clone();
        let trace = events.clone();
        let actor = std::thread::spawn(move || {
            let mut first = true;
            let mut counter = Counter(seen, trace);
            // The envelope received after the stop comes back undispatched;
            // the death point answers it and destroys it before the state.
            let discarded = run_actor_receiving(
                &mut counter,
                ctx,
                move || {
                    // Pause the real receive boundary after the started/cancel
                    // checks; the standard channel still carries the envelope.
                    if first {
                        first = false;
                        e.wait();
                        r.wait();
                    }
                    rx.recv_timeout(Duration::from_secs(5))
                },
                cancel,
            );
            drop(discarded);
            drop(counter);
        });
        entered.wait();
        let (slot, response) = reply_slot(crate::reply::Deadline::Never, None);
        tx.send(MailboxItem::Message(Box::new(MessageEnvelope {
            msg: Increment(events.clone(), monitors),
            slot,
            #[cfg(feature = "metrics")]
            observation: None,
            #[cfg(feature = "lifecycle-trace")]
            from: 0,
        })))
        .unwrap();
        token.cancel();
        release.wait();
        actor.join().unwrap();
        assert_eq!(
            link::lock_request(&request).reason(),
            None,
            "a plain stop request"
        );
        assert_eq!(
            count.load(Ordering::SeqCst),
            0,
            "thread actor dispatched work after idle stop"
        );
        assert!(response.recv().is_err());
        assert_eq!(
            *events.lock().unwrap(),
            ["stopped", "message-dropped", "actor-dropped"]
        );
    }
}

#[cfg(test)]
mod stop_wakeup_regression {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Idle(mpsc::Sender<()>);
    impl Actor for Idle {
        fn stopped(&mut self, _: &Context<Self>) {
            let _ = self.0.send(());
        }
    }
    struct Noop;
    impl Message for Noop {
        type Result = ();
    }
    impl Handler<Noop> for Idle {
        fn handle(
            &mut self,
            _: Noop,
            reply: crate::reply::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            reply.reply(());
        }
    }

    #[test]
    fn concurrent_cancellation_enqueues_exactly_one_shutdown() {
        for already_cancelled in [false, true] {
            let (tx, rx) = mpsc::channel::<MailboxItem<Idle>>();
            let token = CancellationToken::new();
            if already_cancelled {
                token.cancel();
            }
            register_stop_wakeup(&token, tx.clone());
            std::thread::scope(|scope| {
                for _ in 0..16 {
                    let token = &token;
                    scope.spawn(move || {
                        for _ in 0..256 {
                            token.cancel();
                        }
                    });
                }
            });
            assert!(matches!(rx.try_recv(), Ok(MailboxItem::Shutdown)));
            // Keep tx alive: Empty proves no duplicate, rather than disconnection.
            assert!(matches!(
                rx.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Empty)
            ));
        }
    }

    #[test]
    fn cancellation_wakes_the_real_blocking_receive_boundary() {
        let (tx, rx) = mpsc::channel();
        let token = CancellationToken::new();
        register_stop_wakeup(&token, tx.clone());
        let (_, completion) = spawned_rt::sync::watch::channel(None);
        let (id, trap, request) = (
            ActorId::next(),
            new_trap_exit_flag(),
            new_linked_exit_reason(),
        );
        let ctx = Context {
            id,
            sender: tx.clone(),
            cancellation_token: token.clone(),
            completion,
            monitors: Arc::new(Mutex::new(Some(HashMap::new()))),
            trap_exit: trap.clone(),
            links: new_link_table(),
            linked_reason: request.clone(),
            signal: signal_target::<_>(id, None, &tx, trap.clone(), request.clone(), token.clone()),
            mailbox: crate::instrumentation::Mailbox::default(),
            replies: Registry::new(ActorId::next(), DEFAULT_MAX_OUTSTANDING),
            jobs: Arc::new(crate::reply::jobs::JobSet::new(ActorId::next(), 1)),
            cleaned: Arc::default(),
        };
        let (entered_tx, entered) = mpsc::channel();
        let (stopped_tx, stopped) = mpsc::channel();
        let cancel = token.clone();
        let actor = std::thread::spawn(move || {
            run_actor_receiving(
                &mut Idle(stopped_tx),
                ctx,
                move || {
                    let _ = entered_tx.send(());
                    rx.recv()
                        .map_err(|_| std::sync::mpsc::RecvTimeoutError::Disconnected)
                },
                cancel,
            )
        });
        entered.recv_timeout(Duration::from_secs(2)).unwrap();
        token.cancel();
        stopped
            .recv_timeout(Duration::from_secs(2))
            .expect("cancel did not wake recv");
        actor.join().unwrap();
        assert_eq!(
            link::lock_request(&request).reason(),
            None,
            "a plain stop request"
        );
    }

    struct Crash;
    impl Message for Crash {
        type Result = ();
    }
    struct Failing;
    impl Actor for Failing {}
    impl Handler<Crash> for Failing {
        fn handle(
            &mut self,
            _: Crash,
            _reply: crate::reply::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            {
                panic!("linked wakeup regression");
            }
        }
    }

    #[test]
    fn every_stop_route_wakes_an_idle_thread() {
        for route in 0..4 {
            let (stopped_tx, stopped) = mpsc::channel();
            let actor = Idle(stopped_tx).start();
            // A reply ensures startup is complete before the cancellation race.
            actor.request(Noop).unwrap();
            let failing = if route == 3 {
                let peer = Failing.start();
                actor.context().link(&peer.child_handle());
                Some(peer)
            } else {
                None
            };
            match route {
                0 => actor.context().stop(),
                1 => actor.child_handle().stop(),
                2 => actor.cancellation_token.cancel(),
                _ => failing.as_ref().unwrap().send(Crash).unwrap(),
            }
            if route == 3 {
                // An untrapped link notice ends the idle actor without
                // stopped() (decision 2): the wakeup is observed as its exit.
                let reason = actor.child_handle().wait_exit_blocking();
                assert!(matches!(reason, ExitReason::Panic(_)), "got {reason:?}");
                assert!(stopped.try_recv().is_err(), "stopped() ran");
                failing.unwrap().join();
                continue;
            }
            let result = stopped.recv_timeout(Duration::from_secs(2));
            // A failed test must not hang cleanup on a broken wakeup callback.
            if result.is_err() {
                actor.child_handle().stop();
                let _ = actor.send(Noop);
            }
            assert!(
                result.is_ok(),
                "stop route {route} failed to wake the mailbox"
            );
            let reason = actor.wait_exit();
            if route == 3 {
                assert!(matches!(reason, ExitReason::Panic(_)));
                failing.unwrap().join();
            } else {
                assert_eq!(reason, ExitReason::Normal);
            }
        }
    }

    struct Busy {
        entered: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
        handled: Arc<AtomicUsize>,
    }
    impl Actor for Busy {}
    struct Work;
    impl Message for Work {
        type Result = ();
    }
    impl Handler<Work> for Busy {
        fn handle(
            &mut self,
            _: Work,
            reply: crate::reply::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            self.handled.fetch_add(1, Ordering::SeqCst);
            let _ = self.entered.send(());
            self.release.recv_timeout(Duration::from_secs(5)).unwrap();
            reply.reply(());
        }
    }

    #[test]
    fn repeated_stop_during_a_handler_discards_queued_requests() {
        let (entered_tx, entered) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        let handled = Arc::new(AtomicUsize::new(0));
        let actor = Busy {
            entered: entered_tx,
            release: release_rx,
            handled: handled.clone(),
        }
        .start();
        let first = actor.send_request(Work, DEFAULT_REQUEST_TIMEOUT).unwrap();
        entered.recv_timeout(Duration::from_secs(2)).unwrap();
        let replies: Vec<_> = (0..256)
            .map(|_| actor.send_request(Work, DEFAULT_REQUEST_TIMEOUT).unwrap())
            .collect();
        std::thread::scope(|scope| {
            for _ in 0..16 {
                let child = actor.child_handle();
                scope.spawn(move || {
                    for _ in 0..256 {
                        child.stop();
                    }
                });
            }
        });
        release.send(()).unwrap();
        first.recv().unwrap();
        for reply in replies {
            assert!(matches!(
                reply.recv(),
                Err(ActorError::ActorStopped(ExitReason::Normal))
            ));
        }
        assert_eq!(actor.wait_exit(), ExitReason::Normal);
        assert_eq!(handled.load(Ordering::SeqCst), 1);
    }
}

/// Context and request APIs that the behavioural tests above do not reach.
#[cfg(test)]
mod api_coverage {
    use super::*;
    use crate::test_wait::WAIT;
    use std::sync::mpsc as std_mpsc;

    struct Echo;
    struct Ask(u32);
    impl Message for Ask {
        type Result = u32;
    }
    /// Holds the actor inside a handler until released, then stops it, so a
    /// request queued behind it is admitted but never answered.
    struct StopAfter(std_mpsc::Receiver<()>);
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
    struct Unlink(ChildHandle);
    impl Message for Unlink {
        type Result = ();
    }
    impl Actor for Echo {}
    impl Handler<Ask> for Echo {
        fn handle(
            &mut self,
            msg: Ask,
            reply: crate::reply::PendingReply<'_, u32>,
            _: &Context<Self>,
        ) {
            reply.reply(msg.0);
        }
    }
    impl Handler<StopAfter> for Echo {
        fn handle(
            &mut self,
            msg: StopAfter,
            reply: crate::reply::PendingReply<'_, ()>,
            ctx: &Context<Self>,
        ) {
            let _ = msg.0.recv_timeout(WAIT);
            ctx.stop();
            reply.reply(());
        }
    }
    impl Handler<OwnRecipient> for Echo {
        fn handle(
            &mut self,
            _: OwnRecipient,
            reply: crate::reply::PendingReply<'_, (Recipient<Ask>, ActorId)>,
            ctx: &Context<Self>,
        ) {
            reply.reply((ctx.recipient::<Ask>(), ctx.id()));
        }
    }
    impl Handler<Boom> for Echo {
        fn handle(
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
    fn context_recipient_and_identity_reach_their_actor() {
        let actor = Echo.start();
        let (recipient, id) = actor.request(OwnRecipient).unwrap();
        assert_eq!(id, actor.id());
        assert!(matches!(request(&*recipient, Ask(7), WAIT), Ok(7)));
        recipient.send(Ask(1)).unwrap();
        actor.child_handle().stop();
        actor.join();
        // Past the death point: NoProc (v0.8, decision 4 of otp-exit-signals).
        assert!(matches!(recipient.send(Ask(1)), Err(ActorError::NoProc)));
    }

    #[test]
    fn requests_to_a_stopped_actor_fail_without_waiting() {
        let actor = Echo.start();
        let recipient = actor.recipient::<Ask>();
        actor.child_handle().stop();
        actor.join();
        // Past the death point: NoProc (v0.8, decision 4 of otp-exit-signals).
        assert!(matches!(
            actor.request_with_timeout(Ask(1), WAIT),
            Err(ActorError::NoProc)
        ));
        assert!(matches!(
            request(&*recipient, Ask(1), WAIT),
            Err(ActorError::NoProc)
        ));
    }

    #[test]
    fn admitted_requests_fail_when_the_handler_unwinds_before_replying() {
        let actor = Echo.start();
        assert!(matches!(
            actor.request_with_timeout(Boom, WAIT),
            Err(ActorError::ActorStopped(ExitReason::Panic(_)))
        ));
        let other = Echo.start();
        let recipient = other.recipient::<Boom>();
        assert!(matches!(
            request(&*recipient, Boom, WAIT),
            Err(ActorError::ActorStopped(ExitReason::Panic(_)))
        ));
    }

    #[test]
    fn requests_time_out_while_the_actor_is_busy() {
        let actor = Echo.start();
        let recipient = actor.recipient::<Ask>();
        let (release, held) = std_mpsc::channel();
        actor.send(StopAfter(held)).unwrap();
        let short = Duration::from_millis(1);
        assert!(matches!(
            actor.request_with_timeout(Ask(1), short),
            Err(ActorError::RequestTimeout)
        ));
        assert!(matches!(
            request(&*recipient, Ask(1), short),
            Err(ActorError::RequestTimeout)
        ));
        release.send(()).unwrap();
        actor.join();
    }

    struct Trapper {
        trap: bool,
    }
    impl Actor for Trapper {
        fn started(&mut self, ctx: &Context<Self>) {
            ctx.trap_exit(self.trap);
        }
    }
    impl Handler<Ask> for Trapper {
        fn handle(
            &mut self,
            msg: Ask,
            reply: crate::reply::PendingReply<'_, u32>,
            _: &Context<Self>,
        ) {
            reply.reply(msg.0);
        }
    }
    impl Handler<Unlink> for Trapper {
        fn handle(
            &mut self,
            msg: Unlink,
            reply: crate::reply::PendingReply<'_, ()>,
            ctx: &Context<Self>,
        ) {
            ctx.unlink(&msg.0);
            reply.reply(());
        }
    }

    #[test]
    fn default_exit_received_keeps_a_trapping_actor_alive() {
        let trapper = Trapper { trap: true }.start();
        assert!(matches!(trapper.request(Ask(0)), Ok(0)));
        let peer = Echo.start();
        trapper.context().link(&peer.child_handle());
        peer.send(Boom).unwrap();
        assert!(matches!(peer.wait_exit(), ExitReason::Panic(_)));
        // The Exit is handled by the default `exit_received`, which ignores it.
        assert!(matches!(trapper.request(Ask(3)), Ok(3)));
        assert!(trapper.exit_reason().is_none());
        trapper
            .child_handle()
            .stop_with_reason(crate::ExitReason::Shutdown);
        assert_eq!(trapper.wait_exit(), ExitReason::Shutdown);
    }

    #[test]
    fn unlink_prevents_signal_delivery() {
        // Not trapping: a linked peer's panic would take this actor down.
        let trapper = Trapper { trap: false }.start();
        let peer = Echo.start();
        let peer_handle = peer.child_handle();
        trapper.context().link(&peer_handle);
        trapper.request(Unlink(peer_handle.clone())).unwrap();
        // Unlinking yourself or an already-unlinked peer is a no-op.
        trapper.request(Unlink(trapper.child_handle())).unwrap();
        trapper.request(Unlink(peer_handle)).unwrap();
        peer.send(Boom).unwrap();
        assert!(matches!(peer.wait_exit(), ExitReason::Panic(_)));
        assert!(matches!(trapper.request(Ask(1)), Ok(1)));
        assert!(trapper.exit_reason().is_none());
        trapper
            .child_handle()
            .stop_with_reason(crate::ExitReason::Shutdown);
        trapper.join();
    }

    #[test]
    fn opposite_links_between_the_same_pair_do_not_deadlock() {
        // Each link/unlink locks both link tables. Both directions must take
        // the locks in the same global order, or these two loops deadlock.
        let a = Trapper { trap: true }.start();
        let b = Trapper { trap: true }.start();
        let (done, finished) = std_mpsc::channel();
        let workers: Vec<_> = [
            (a.context(), b.child_handle()),
            (b.context(), a.child_handle()),
        ]
        .into_iter()
        .map(|(own, peer)| {
            let done = done.clone();
            std::thread::spawn(move || {
                for _ in 0..2_000 {
                    own.link(&peer);
                    own.unlink(&peer);
                }
                done.send(()).unwrap();
            })
        })
        .collect();
        for _ in 0..2 {
            finished
                .recv_timeout(WAIT)
                .expect("opposite link directions deadlocked");
        }
        for worker in workers {
            worker.join().unwrap();
        }
        for actor in [a, b] {
            actor
                .child_handle()
                .stop_with_reason(crate::ExitReason::Shutdown);
            actor.join();
        }
    }

    #[test]
    fn debug_output_names_the_handle() {
        let actor = Echo.start();
        assert!(format!("{actor:?}").starts_with("ActorRef"));
        assert!(format!("{:?}", actor.context()).starts_with("Context"));
        actor.child_handle().stop();
        actor.join();
    }
}
