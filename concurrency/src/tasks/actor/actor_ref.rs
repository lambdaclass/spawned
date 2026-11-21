//! `ActorRef`: the external handle (sends, requests, links, monitors, join and exit observation).
use super::*;

impl<A: Actor> ActorRef<A> {
    /// Clone this handle with a timeout override for generated protocol calls.
    /// Per-call override wins over method and protocol defaults. No message is
    /// sent until a protocol method is called; timeout does not cancel its work.
    pub fn with_request_timeout(&self, timeout: Duration) -> crate::response::RequestOptions<Self> {
        crate::response::RequestOptions::new(self.clone(), timeout)
    }

    /// This instance's committed startup result, for every placement and
    /// verb (a `spawn`ed actor included): `Started` once `started()`
    /// returned ready, `Ignored`, or the start's error. A failure is
    /// answered after the instance's publication (or with `pending` after
    /// its cleanup deadline), as the waiting starts answer. Success does not
    /// guarantee the actor remains alive afterward.
    pub async fn ready(&self) -> Result<crate::start::StartOutcome<()>, crate::start::StartError> {
        start::observe(self, false).await
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
        self.send_request_with(msg, deadline.ticks(), true)
    }

    /// `send_request`; `raw` puts a deadline in the owner's heap now (the
    /// receiver may be awaited directly). The `request` paths arm their own
    /// timer and pass `false`.
    #[inline]
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
    ///
    /// Uses the same request state machine as `request_with_timeout`, with
    /// a zero-sized fixed budget instead of capturing a Duration.
    pub fn request<M>(
        &self,
        msg: M,
    ) -> impl std::future::Future<Output = Result<M::Result, ActorError>> + Send + '_
    where
        A: Handler<M>,
        M: Message,
    {
        self.request_with_budget(msg, crate::reply::DefaultRequestBudget)
    }

    /// Run `f` in one of the actor's turns, with its state and `Context`,
    /// and receive its result. For tests: `spawned_test::tasks::in_actor` (through
    /// `__private`).
    pub(crate) fn in_turn<R, F>(&self, f: F) -> Result<ReplyReceiver<R>, ActorError>
    where
        R: Send + 'static,
        F: FnOnce(&mut A, &Context<A>) -> R + Send + 'static,
    {
        let (slot, rx) = reply_slot(crate::reply::NEVER_TICKS, Some(&self.shared.replies), false);
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
    pub fn request_with_timeout<M>(
        &self,
        msg: M,
        duration: Duration,
    ) -> impl Future<Output = Result<M::Result, ActorError>> + Send + '_
    where
        A: Handler<M>,
        M: Message,
    {
        self.request_with_budget(msg, duration)
    }

    pub(super) async fn request_with_budget<M, B>(
        &self,
        msg: M,
        budget: B,
    ) -> Result<M::Result, ActorError>
    where
        A: Handler<M>,
        M: Message,
        B: crate::reply::RequestBudget,
    {
        let duration = budget.into_duration();
        let observation = crate::instrumentation::RequestObservation::new();
        // Bind the receiver before awaiting it: awaiting inside a `match` on
        // the `Result` keeps the scrutinee's storage alive beside the awaited
        // receiver and doubles this future's size.
        let (deadline, timer) = crate::reply::request_timer(duration);
        let rx = match self.send_request_with(msg, deadline, false) {
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
    pub(super) fn shared_weak(&self) -> std::sync::Weak<ActorShared<A>> {
        Arc::downgrade(&self.shared)
    }
}

impl<A: Actor> ActorShared<A> {
    pub(super) fn child_handle(&self) -> ChildHandle {
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
        ActorRef::send_request_with(self, msg, deadline.ticks(), false)
    }
}
