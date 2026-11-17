//! Mailbox envelopes: type erasure of messages, requests, `Down`, exit signals, in-turn closures and the stop wakeup on the actor side.
use super::*;

// ---------------------------------------------------------------------------
// Envelope (type-erasure)
// ---------------------------------------------------------------------------

pub(super) trait Envelope<A: Actor>: Send {
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
    fn discharge(&mut self, _replies: Option<&Registry>) {}

    fn handle(self: Box<Self>, actor: &mut A, ctx: &Context<A>);
}

/// Answer every request queued in a dead owner's mailbox (and `extra`, an
/// envelope received after the stop), destroying nothing: the items come
/// back, to be destroyed once the death point's wake-ups ran. Admission is
/// closed first (waiting for in-flight sends), so nothing is queued after.
pub(super) fn answer_mailbox<A: Actor>(
    rx: &mut mpsc::Receiver<MailboxItem<A>>,
    extra: Option<Box<dyn Envelope<A> + Send>>,
    replies: Option<&Registry>,
) -> Vec<MailboxItem<A>> {
    let mut queued = rx.drain();
    queued.extend(extra.map(MailboxItem::Message));
    for item in &mut queued {
        if let MailboxItem::Message(envelope) = item {
            envelope.discharge(replies);
        }
    }
    queued
}

/// A never-polled actor's startup payload (its thread was never started).
pub(super) type StartupPayload<A> = (
    A,
    Context<A>,
    mpsc::Receiver<MailboxItem<A>>,
    CancellationToken,
);
pub(super) fn discharge_startup<A: Actor>(payload: &mut StartupPayload<A>) -> Vec<Box<dyn Send>> {
    let replies = payload.1.replies.clone();
    erase(answer_mailbox(&mut payload.2, None, Some(&replies)))
}
pub(super) fn erase<T: Send + 'static>(items: Vec<T>) -> Vec<Box<dyn Send>> {
    items
        .into_iter()
        .map(|item| Box::new(item) as Box<dyn Send>)
        .collect()
}

pub(super) struct MessageEnvelope<M: Message> {
    // Before the message: a plain drop also answers the request first.
    pub(super) slot: SlotHandle<M::Result>,
    pub(super) msg: M,
    #[cfg(feature = "metrics")]
    pub(super) observation: Option<crate::instrumentation::MessageObservation>,
    /// The recorded actor that sent it (0: outside code or a request); its
    /// mailbox item kind is then the model's `msg`.
    #[cfg(feature = "lifecycle-trace")]
    pub(super) from: u64,
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
    fn discharge(&mut self, replies: Option<&Registry>) {
        self.slot.dispose(replies);
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

#[cfg(all(spawned_sim, feature = "async"))]
pub(super) fn assert_native_clock_context() {
    assert!(
        spawned_rt::tasks::Handle::try_current_or_ambient().is_err(),
        "spawned_sim: native actors cannot run in or receive messages from a simulated world; use tasks actors for simulation, or run native actors outside the simulation"
    );
}

/// Admit a message. Past the death point (the registry is sealed, or the
/// mailbox is gone) it reports `NoProc`; before it, including while the
/// actor exits, it is queued, and a request is answered at the death point
/// if it is not handled.
pub(super) fn enqueue<A, M>(
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
    #[cfg(all(spawned_sim, feature = "async"))]
    assert_native_clock_context();
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
pub(super) struct DownEnvelope {
    pub(super) down: Down,
    #[cfg(feature = "metrics")]
    pub(super) observation: Option<crate::instrumentation::MessageObservation>,
}

pub(super) fn enqueue_down<A: Actor + Handler<Down>>(
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
pub(super) fn admit<A: Actor, M: 'static>(
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
pub(super) type InTurn<A, R> = Box<dyn FnOnce(&mut A, &Context<A>) -> R + Send>;
pub(super) struct InTurnEnvelope<A: Actor, R: Send + 'static> {
    pub(super) slot: SlotHandle<R>,
    pub(super) f: InTurn<A, R>,
}

impl<A: Actor, R: Send + 'static> Envelope<A> for InTurnEnvelope<A, R> {
    fn discharge(&mut self, replies: Option<&Registry>) {
        self.slot.dispose(replies);
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
pub(super) struct ExitEnvelope {
    pub(super) exit: Exit,
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
pub(super) struct ParentExitEnvelope {
    pub(super) exit: Exit,
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
pub(super) struct HeldEnvelope {
    pub(super) exit: Exit,
    pub(super) parent: bool,
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
pub(super) fn signal_target<A: Actor>(
    id: ActorId,
    parent: Option<ActorId>,
    sender: &mpsc::Sender<MailboxItem<A>>,
    trap: TrapExitFlag,
    request: LinkedExitReason,
    token: CancellationToken,
    startup: Arc<crate::start::StartupCell>,
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
        startup,
    })
}

// A private wakeup is distinct from user envelopes and never invokes a handler.
pub(super) enum MailboxItem<A: Actor> {
    Message(Box<dyn Envelope<A> + Send>),
    Shutdown,
}

pub(super) fn register_stop_wakeup<A: Actor>(
    token: &CancellationToken,
    sender: mpsc::Sender<MailboxItem<A>>,
) {
    // One callback covers Context, ChildHandle, linked exits and framework cleanup.
    // CancellationToken drains this FnOnce callback exactly once, including when
    // multiple callers race to cancel. Repeated stop calls cannot grow the queue.
    token.on_cancel(Box::new(move || {
        let _ = sender.send(MailboxItem::Shutdown);
    }));
}
