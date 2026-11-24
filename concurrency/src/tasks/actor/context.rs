//! `Context`: the handler-side handle (sends, requests, timers, links, monitors, stop and exit).
use super::*;

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
    pub(super) fn parent_exit(&self, reason: ExitReason) {
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
    /// A caller whose exit already began does nothing and returns `Ok(())`;
    /// an exit racing this call can linearize after the link attempt.
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

    /// Resolves once [`exit_pending`](Self::exit_pending) is `true`: the
    /// awaitable form of it, for a long-running handler, helper or piped job
    /// that should wind down when this actor is asked to stop.
    ///
    /// The same events trigger it: `ctx.stop()`, an administrative stop, a
    /// kill, an untrapped exit signal or link notice, a panic, and a parent
    /// exit (supervisor or system shutdown, a `start_linked` parent's exit)
    /// queued for a trapping actor. A trapped [`Exit`](crate::Exit) does not.
    /// Already pending (including in `stopped()`): it is ready at once. Once
    /// the actor is past its death point it stays ready.
    ///
    /// The future is `'static` and `Send`, so it can be moved into
    /// `tokio::select!`, [`pipe`](Self::pipe) or a helper. Nothing preempts
    /// the handler: it decides what to do when the future resolves, and the
    /// actor exits after the handler returns. Unused, it costs nothing; a
    /// waiting future registers one waiter and adds nothing per message.
    ///
    /// ```ignore
    /// tokio::select! {
    ///     done = self.long_work() => reply.reply(Ok(done)),
    ///     () = ctx.stopping() => reply.reply(Err(Cancelled)),
    /// }
    /// ```
    pub fn stopping(&self) -> impl Future<Output = ()> + Send + 'static {
        let token = self.shared.cancellation_token.clone();
        let request = self.shared.linked_reason.clone();
        async move {
            // Every other pending exit cancels the stop token; the waiter is
            // for a parent exit queued behind this actor's messages.
            let Some(parent_exit) = link::lock_request(&request).stopping_waiter() else {
                return;
            };
            let stop = pin!(token.cancelled());
            let parent_exit = pin!(parent_exit.cancelled());
            future::select(stop, parent_exit).await;
        }
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

    pub(crate) fn cancellation_token(&self) -> StopToken {
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
        Context::send_request_with(self, msg, deadline.ticks(), false)
    }
}
