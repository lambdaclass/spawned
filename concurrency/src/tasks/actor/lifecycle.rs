//! Actor startup and the main loop: spawn on a backend, `started()`, the dispatch loop, `stopped()` and the death point.
use super::*;

// ---------------------------------------------------------------------------
// Actor startup + main loop
// ---------------------------------------------------------------------------

impl<A: Actor> ActorRef<A> {
    pub(super) fn spawn(actor: A, backend: Backend, parent: Option<ChildHandle>) -> Self {
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

    pub(super) fn spawn_inner(
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
            run_actor(actor, ctx, rx, cancellation_token, &mut cleanup).await;
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
                let _handle = rt::spawn(inner_future);
            }
            Backend::Blocking => {
                let _handle = rt::spawn_blocking(move || rt::block_on(inner_future));
            }
            Backend::Thread => {
                let _handle = threads::spawn(move || threads::block_on(inner_future));
            }
        }

        Ok(actor_ref)
    }
}

/// A pending user future of the lifecycle: `started()`, a turn (handler,
/// `exit_received`, a job's `done`) or `stopped()`. If the runtime drops the
/// lifecycle while it is pending (G1), its `Drop` reaches the death point
/// first: the turn's own caller, every deferred token and every queued
/// request are answered and woken before this future and its captures (the
/// message, the handler's locals) are destroyed. A destructor panic of the
/// future is a cleanup failure.
pub(super) struct DeathFirst<'a, A: Actor, C: Fn()> {
    pub(super) inner: Option<Pin<Box<dyn Future<Output = ()> + Send + 'a>>>,
    pub(super) rx: &'a mut mpsc::Receiver<Box<dyn Envelope<A> + Send>>,
    pub(super) cleanup: &'a mut crate::actor_cleanup::ActorCleanup<C>,
    pub(super) caller: Option<&'a dyn Caller>,
    /// The run loop already polled `inner` once in this task poll (it was
    /// pending): the next poll returns `Pending` without polling it again.
    pub(super) polled: bool,
}
impl<'a, A: Actor, C: Fn()> DeathFirst<'a, A, C> {
    pub(super) fn new(
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
    pub(super) fn pending(
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
pub(super) type TurnFuture<'a> = Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

/// Poll `turn` once, catching its panic.
#[inline]
pub(super) fn poll_caught(
    turn: &mut TurnFuture<'_>,
    cx: &mut std::task::Context<'_>,
) -> Result<std::task::Poll<()>, Panic> {
    std::panic::catch_unwind(AssertUnwindSafe(|| turn.as_mut().poll(cx)))
}

/// Drop a turn that panicked; a panic of its destructor is disposed of.
pub(super) fn drop_panicked(turn: Option<TurnFuture<'_>>) {
    if let Err(secondary) = std::panic::catch_unwind(AssertUnwindSafe(|| drop(turn))) {
        crate::mailbox::dispose_panic(secondary);
    }
}
/// A caught panic payload.
pub(super) type Panic = Box<dyn std::any::Any + Send>;

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
pub(super) struct ExitOnDrop<'c, A: Actor, C: Fn()> {
    pub(super) actor: Option<A>,
    pub(super) rx: mpsc::Receiver<Box<dyn Envelope<A> + Send>>,
    pub(super) cleanup: &'c mut crate::actor_cleanup::ActorCleanup<C>,
}
impl<A: Actor, C: Fn()> ExitOnDrop<'_, A, C> {
    /// D, then the queued requests, then destruction (mailbox, then state).
    pub(super) fn finish(&mut self, dropped: bool) {
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

pub(super) async fn run_actor<A: Actor, C: Fn()>(
    actor: A,
    ctx: Context<A>,
    rx: mpsc::Receiver<Box<dyn Envelope<A> + Send>>,
    cancellation_token: StopToken,
    cleanup: &mut crate::actor_cleanup::ActorCleanup<C>,
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

pub(super) async fn run_actor_lifecycle<A: Actor, C: Fn()>(
    actor: &mut A,
    ctx: Context<A>,
    rx: &mut mpsc::Receiver<Box<dyn Envelope<A> + Send>>,
    cleanup: &mut crate::actor_cleanup::ActorCleanup<C>,
    cancellation_token: StopToken,
) {
    let _monitor_cleanup = MonitorCleanup(ctx.shared.monitors.clone());
    let request = ctx.shared.linked_reason.clone();
    let trace_id = ctx.shared.id.raw();
    if let Some(scope) = &ctx.shared.managed {
        if !scope.can_start() {
            scope.stop(ExitReason::Shutdown(None));
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
        // The payload is user code too: its destructor may panic (L6).
        crate::mailbox::dispose_panic(panic);
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
    let (stop, stopping) = {
        let mut request = link::lock_request(&request);
        let stop = request.end_starting(ctx.shared.trap_exit.load(Ordering::Acquire));
        (stop, request.take_stopping_wake())
    };
    if stop {
        cancellation_token.cancel();
    }
    if let Some(waiter) = stopping {
        // A held parent exit is now queued: wake `ctx.stopping()`.
        waiter.cancel();
    }

    fail_point!("actor.after_started_before_ready", async);
    if let Some(scope) = &ctx.shared.managed {
        if !scope.mark_ready() {
            scope.stop(ExitReason::Shutdown(None));
        }
    }

    // One cancellation future for the whole loop: polled only when the
    // mailbox has nothing, so a busy actor registers no waiter per message.
    let mut cancelled = pin!(cancellation_token.cancelled());
    // Check stop after receiving and before dispatch: a message published
    // after stop must not slip through an earlier snapshot. A rejected receive
    // stays in the mailbox wrapper until the death point answers its caller.
    // The fast check is one atomic load; only parked loops register a waiter.
    loop {
        let msg = {
            let recv = pin!(future::poll_fn(|cx| {
                // Stop and closed-mailbox both take the normal lifecycle exit,
                // just like the cancellation arm below. Check after the
                // mailbox acquire and retain a rejected item for shutdown.
                // The receive, check and trace event are one step
                // of the lifecycle trace: a recorded stop is seen here.
                crate::lifecycle_trace::life_section(trace_id, || {
                    #[cfg(all(test, not(feature = "lifecycle-trace"), not(spawned_sim)))]
                    stop_receive_probe::before_receive();
                    let received = rx.poll_recv(cx);
                    let result = cancellation_token.after_receive(received, |item| {
                        rx.hold_for_shutdown(item);
                    });
                    #[cfg(feature = "lifecycle-trace")]
                    if let std::task::Poll::Ready(Some(envelope)) = &result {
                        crate::lifecycle_trace::life_emit(trace_id, || {
                            crate::lifecycle_trace::LifeEvent::new("Dispatch", trace_id)
                                .k(envelope.trace_kind())
                        });
                    }
                    result
                })
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
                    // Dispatch completed the turn synchronously. The caller
                    // hold is only needed if an unfinished future is dropped;
                    // turn accounting and observation bracket either branch.
                    Ok((None, _)) => Ok(()),
                    Ok((Some(turn), caller)) => {
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
                    // The payload is user code too: its destructor may panic (L6).
                    crate::mailbox::dispose_panic(panic);
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
        // The payload is user code too: its destructor may panic (L6).
        crate::mailbox::dispose_panic(panic);
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
