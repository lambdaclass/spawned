//! Mailbox envelopes: type erasure of messages, requests, `Down`, exit signals and in-turn closures on the actor side.
use super::*;

// ---------------------------------------------------------------------------
// Envelope (type-erasure on the actor side)
// ---------------------------------------------------------------------------

pub(super) trait Envelope<A: Actor>: Send {
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

/// A dispatched turn: None means the guarded dispatch already completed;
/// otherwise its future and caller stay lent by the envelope for the turn.
pub(super) type Dispatch<'a> = (
    Option<Pin<Box<dyn Future<Output = ()> + Send + 'a>>>,
    Option<&'a dyn Caller>,
);

/// Dispatch `envelope`, catching a panic while the handler builds its
/// future (a manual `Handler` may panic before it returns one): a handler
/// panic like any other.
pub(super) fn dispatch<'a, A: Actor>(
    envelope: &'a mut (dyn Envelope<A> + Send),
    actor: &'a mut A,
    ctx: &'a Context<A>,
) -> Result<Dispatch<'a>, Box<dyn std::any::Any + Send>> {
    let mut parts = Some((envelope, actor));
    std::panic::catch_unwind(AssertUnwindSafe(move || {
        let (envelope, actor) = parts.take().expect("dispatched once");
        crate::lifecycle_trace::with_actor(ctx.shared.id.raw(), || envelope.handle(actor, ctx))
    }))
}

/// Take an envelope's payload at its one dispatch.
pub(super) fn dispatched<T>(payload: &mut Option<T>) -> T {
    payload.take().expect("an envelope is dispatched once")
}

/// Answer every request queued in a dead owner's mailbox, destroying
/// nothing: the drained envelopes come back, to be destroyed once the death
/// point's wake-ups ran. Admission is closed first, so nothing is queued
/// after.
pub(super) fn answer_mailbox<A: Actor>(
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
pub(super) type StartupPayload<A> = (
    A,
    Context<A>,
    mpsc::Receiver<Box<dyn Envelope<A> + Send>>,
    StopToken,
);
pub(super) fn discharge_startup<A: Actor>(payload: &mut StartupPayload<A>) -> Vec<Box<dyn Send>> {
    let replies = payload.1.shared.replies.clone();
    erase(answer_mailbox(&mut payload.2, Some(&replies)))
}
pub(super) fn erase<T: Send + 'static>(items: Vec<T>) -> Vec<Box<dyn Send>> {
    items
        .into_iter()
        .map(|item| Box::new(item) as Box<dyn Send>)
        .collect()
}

pub(super) struct MessageEnvelope<M: Message> {
    /// The request's slot: held until dispatch (dropped undispatched, it is
    /// disposed of as a `SlotHandle`), then lent to the turn.
    pub(super) slot: Option<Arc<Slot<M::Result>>>,
    // Some until dispatch; taking the message is also the dispatch marker.
    pub(super) msg: Option<M>,
    #[cfg(feature = "metrics")]
    pub(super) observation: Option<crate::instrumentation::MessageObservation>,
    /// The recorded actor that sent it (0: outside code or a request); its
    /// mailbox item kind is then the model's `msg`.
    #[cfg(feature = "lifecycle-trace")]
    pub(super) from: u64,
}

/// An undispatched request (mailbox disposal without the registry, a
/// refused admission) is answered as its `SlotHandle` would be.
impl<M: Message> Drop for MessageEnvelope<M> {
    #[inline]
    fn drop(&mut self) {
        if self.msg.is_some() && self.slot.is_some() {
            drop(SlotHandle::of(self.slot.take()));
        }
    }
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
        if self.msg.is_some() {
            let mut held = SlotHandle::of(self.slot.take());
            held.dispose(replies);
        }
    }
    fn handle<'a>(&'a mut self, actor: &'a mut A, ctx: &'a Context<A>) -> Dispatch<'a> {
        let Self { slot, msg, .. } = self;
        let msg = dispatched(msg);
        let slot: &'a Option<Arc<Slot<M::Result>>> = slot;
        let reply = PendingReply::lent(slot.as_ref(), &ctx.shared.replies);
        let caller = slot.as_ref().map(|slot| slot as &dyn Caller);
        match <A as Handler<M>>::__spawned_handle_immediate(actor, msg, reply, ctx) {
            None => (None, caller),
            Some((msg, reply)) => (Some(Box::pin(actor.handle(msg, reply, ctx))), caller),
        }
    }
}

/// A sent message (no caller): no reply slot to hold, lend or dispose of,
/// so its dispatch builds only a detached `PendingReply`.
pub(super) struct SendEnvelope<M: Message> {
    pub(super) msg: Option<M>,
    #[cfg(feature = "metrics")]
    pub(super) observation: Option<crate::instrumentation::MessageObservation>,
    #[cfg(feature = "lifecycle-trace")]
    pub(super) from: u64,
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
        match <A as Handler<M>>::__spawned_handle_immediate(
            actor,
            msg,
            PendingReply::detached(),
            ctx,
        ) {
            None => (None, None),
            Some((msg, reply)) => (Some(Box::pin(actor.handle(msg, reply, ctx))), None),
        }
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
        (Some(turn), None)
    }
}

/// Admit a message. Past the death point (the registry is sealed, or the
/// mailbox is gone) it reports `NoProc`; before it, including while the
/// actor exits, it is queued, and a request is answered at the death point
/// if it is not handled.
#[inline]
pub(super) fn enqueue<A, M>(
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
    let mut slot = slot;
    let envelope: Box<dyn Envelope<A> + Send> = Box::new(MessageEnvelope {
        msg: Some(msg),
        slot: slot.take(),
        #[cfg(feature = "metrics")]
        observation: Some(_mailbox.enqueued()),
        #[cfg(feature = "lifecycle-trace")]
        from,
    });
    admit::<A, M>(sender, replies, envelope, from)
}

/// Admit a sent message (no caller): no reply slot at all.
pub(super) fn enqueue_send<A, M>(
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
pub(super) struct DownEnvelope {
    pub(super) down: Option<Down>,
    #[cfg(feature = "metrics")]
    pub(super) observation: Option<crate::instrumentation::MessageObservation>,
}

pub(super) fn enqueue_down<A: Actor + Handler<Down>>(
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
pub(super) fn admit<A: Actor, M: 'static>(
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
pub(super) type InTurn<A, R> = Box<dyn FnOnce(&mut A, &Context<A>) -> R + Send>;
pub(super) struct InTurnEnvelope<A: Actor, R: Send + 'static> {
    pub(super) slot: SlotHandle<R>,
    pub(super) lent: Option<Arc<Slot<R>>>,
    pub(super) f: Option<InTurn<A, R>>,
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
        (Some(turn), lent.as_ref().map(|slot| slot as &dyn Caller))
    }
}

// Private lifecycle signal, excluded from application message/handler metrics.
// Nested supervisor adapters have no public mailbox traffic to bury this signal.
pub(super) struct FailureEnvelope(pub(super) Option<String>);
impl<A: Actor> Envelope<A> for FailureEnvelope {
    #[cfg(feature = "diagnostics")]
    fn message_type(&self) -> &'static str {
        "Exit"
    }

    fn handle<'a>(&'a mut self, _: &'a mut A, _: &'a Context<A>) -> Dispatch<'a> {
        let failure = dispatched(&mut self.0);
        (
            Some(Box::pin(async move {
                panic!("nested supervisor failed: {failure}");
            })),
            None,
        )
    }
}

/// System envelope that dispatches an `Exit` notification to the actor's
/// `exit_received` callback. Unlike `MessageEnvelope<M>`, this does not require
/// `Handler<Exit>` on the actor — every actor can receive `Exit` via the
/// trait's default-no-op `exit_received` callback.
pub(super) struct ExitEnvelope {
    pub(super) exit: Option<Exit>,
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
            Some(Box::pin(async move {
                actor.exit_received(exit, ctx).await;
            })),
            None,
        )
    }
}

/// A parent exit (E3): when dispatched, exit begins with the parent's reason
/// and `stopped()` runs. `exit_received` never sees it.
pub(super) struct ParentExitEnvelope {
    pub(super) exit: Option<Exit>,
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
        (
            Some(Box::pin(async move { ctx.parent_exit(exit.reason) })),
            None,
        )
    }
}

/// An exit signal that arrived while `started()` ran, in its mailbox place
/// (phase 0). If the actor trapped exits when `started()` returned it is an
/// `Exit` (or parent exit); otherwise it was resolved then and is inert.
pub(super) struct HeldEnvelope {
    pub(super) exit: Option<Exit>,
    pub(super) parent: bool,
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
        (Some(turn), None)
    }
}

/// Build the signal target of a new actor instance.
#[allow(clippy::too_many_arguments)] // one per field of the signal target
pub(super) fn signal_target<A: Actor>(
    id: ActorId,
    parent: Option<ActorId>,
    sender: &mpsc::Sender<Box<dyn Envelope<A> + Send>>,
    trap: TrapExitFlag,
    request: LinkedExitReason,
    token: StopToken,
    startup: Arc<crate::start::StartupCell>,
    meta: crate::introspection::Meta,
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
        startup,
        meta,
    })
}
