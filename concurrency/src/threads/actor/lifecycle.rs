//! Actor startup and the main loop on a dedicated OS thread: `started()`, the dispatch loop, `stopped()` and the death point.
use super::*;

// ---------------------------------------------------------------------------
// Actor startup + main loop
// ---------------------------------------------------------------------------

impl<A: Actor> ActorRef<A> {
    pub(super) fn spawn(actor: A, parent: Option<ChildHandle>) -> Self {
        #[cfg(all(spawned_sim, feature = "async"))]
        assert_native_clock_context();
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

        let shared = Arc::new(ActorShared {
            id,
            sender: tx,
            cancellation_token: cancellation_token.clone(),
            completion,
            monitors,
            trap_exit,
            links: links.clone(),
            linked_reason: linked_reason.clone(),
            signal,
            mailbox: crate::instrumentation::Mailbox::default(),
            replies: Registry::new_native(id, A::MAX_OUTSTANDING),
            jobs: Arc::new(crate::reply::jobs::JobSet::new(
                id,
                A::MAX_NATIVE_JOBS.unwrap_or_else(crate::reply::jobs::default_max_native_jobs),
            )),
            cleaned: Arc::default(),
        });
        let actor_ref = ActorRef {
            shared: shared.clone(),
        };
        let ctx = Context { shared };

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
        let replies = actor_ref.shared.replies.clone();
        let jobs = actor_ref.shared.jobs.clone();
        let cleaned = crate::reply::jobs::CleanedGuard(actor_ref.shared.cleaned.clone());
        let _thread_handle = rt::spawn(move || {
            let _cleaned = cleaned;
            let ((mut actor, ctx, mut rx, cancellation_token), mut cleanup) = startup.into_parts();
            let request = ctx.shared.linked_reason.clone();
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
            let queued = answer_mailbox(&mut rx, discarded, Some(&replies));
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
pub(super) fn run_actor<A: Actor>(
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

pub(super) fn run_actor_receiving<A: Actor>(
    actor: &mut A,
    ctx: Context<A>,
    mut receive: impl FnMut() -> Result<MailboxItem<A>, std::sync::mpsc::RecvTimeoutError>,
    cancellation_token: CancellationToken,
) -> Option<Box<dyn Envelope<A> + Send>> {
    let mut discarded = None;
    let request = ctx.shared.linked_reason.clone();
    let trace_id = ctx.shared.id.raw();
    let _monitor_cleanup = MonitorCleanup(ctx.shared.monitors.clone());
    let start_result = catch_unwind(AssertUnwindSafe(|| {
        fail_point!("actor.before_started");
        crate::lifecycle_trace::with_actor(trace_id, || actor.started(&ctx));
    }));
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
        return None;
    }
    // Phase 0 ends: resolve signals held during started() with the trap
    // flag read now.
    if link::lock_request(&request).end_starting(ctx.shared.trap_exit.load(Ordering::Acquire)) {
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
                ctx.shared.replies.begin_turn();
                let result = catch_unwind(AssertUnwindSafe(|| {
                    crate::lifecycle_trace::with_actor(trace_id, || envelope.handle(actor, &ctx));
                }));
                ctx.shared.replies.end_turn(result.is_err());
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
    if run_hook {
        let stop_result = catch_unwind(AssertUnwindSafe(|| {
            fail_point!("actor.before_stopped");
            crate::lifecycle_trace::with_actor(trace_id, || actor.stopped(&ctx));
        }));
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

    // Message destructors and reply cancellation may re-enter monitor state.
    // Preserve stopped() -> monitor cleanup -> undelivered-message drop (the
    // envelope received after the stop is answered at the death point, then
    // dropped with the mailbox).
    drop(_monitor_cleanup);
    discarded
}
