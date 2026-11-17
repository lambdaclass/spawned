//! `ActorRef`: the external handle (sends, requests, links, monitors, join and exit observation).
use super::*;

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
        self.send_request_with(msg, deadline.ticks(), true)
    }

    /// `send_request`; `raw` puts a deadline in the owner's heap now (the
    /// receiver may be awaited directly). The `request` paths arm their own
    /// timer and pass `false`.
    pub(crate) fn send_request_with<M>(
        &self,
        msg: M,
        deadline: u64,
        raw: bool,
    ) -> Result<ReplyReceiver<M::Result>, ActorError>
    where
        A: Handler<M>,
        M: Message,
    {
        let (slot, rx) = reply_slot(deadline, Some(&self.replies), raw);
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
        let (slot, rx) = reply_slot(crate::reply::NEVER_TICKS, Some(&self.replies), false);
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
        let result = match self.send_request_with(msg, crate::reply::clock::after(duration), false)
        {
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

    /// This instance's committed startup result (blocks until `started()`
    /// returned; a failure until the instance is published, or its cleanup
    /// deadline): the same answer the waiting starts give.
    pub fn ready(&self) -> Result<crate::StartOutcome<()>, crate::StartError> {
        start::wait(self, false)
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

    fn send_request_timed(
        &self,
        msg: M,
        deadline: Deadline,
    ) -> Result<ReplyReceiver<M::Result>, ActorError> {
        ActorRef::send_request_with(self, msg, deadline.ticks(), false)
    }
}
