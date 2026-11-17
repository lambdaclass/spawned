//! Actor startup and the main loop on a dedicated OS thread: `started()`, the dispatch loop, `stopped()` and the death point.
use super::*;

// ---------------------------------------------------------------------------
// Actor startup + main loop
// ---------------------------------------------------------------------------

impl<A: Actor> ActorRef<A> {
    /// Stage 1 of every start (design section 3; threads mode): validate,
    /// prepare the instance privately, commit it atomically (the parent's
    /// edge and the name), then start its thread.
    pub(super) fn launch(
        actor: A,
        options: StartOptions<A>,
        verb: Verb,
        arm: Option<ArmMonitor>,
    ) -> Result<(Self, Option<MonitorRef>), crate::start::StartError> {
        use crate::lifecycle_trace as trace;
        use crate::start::{EdgeKind, StartEdge, StartError};
        #[cfg(all(spawned_sim, feature = "async"))]
        assert_native_clock_context();
        let id = ActorId::next();
        let c = trace::start_child(id.raw());
        let parent = match &verb {
            Verb::Link(p) | Verb::Monitor(p) => Some(p),
            Verb::Spawn | Verb::Start => None,
        };
        trace::start_emit(c, || {
            trace::StartEvent::new("SBegin", c)
                .x(verb.trace_name())
                .y("none")
                .f(options.name.is_some())
        });
        if let Err(error) = options.validate() {
            trace::start_emit(c, || trace::StartEvent::new("SPrepareFail", c));
            return Err(error);
        }
        let (tx, rx) = mpsc::channel::<MailboxItem<A>>();
        let cancellation_token = CancellationToken::new();
        register_stop_wakeup(&cancellation_token, tx.clone());
        let (completion_tx, completion) = spawned_rt::sync::watch::channel(None);
        let monitors: MonitorTable = Arc::new(crate::monitor::fence::open());
        let trap_exit = new_trap_exit_flag();
        let links = new_link_table();
        let linked_reason = new_linked_exit_reason();
        {
            // Phase 0: signals that arrive before started() returns are held.
            let mut request = link::lock_request(&linked_reason);
            request.begin_starting();
            #[cfg(feature = "lifecycle-trace")]
            if c != 0 {
                request.trace = trace::StartRole::Child(c);
            }
        }
        let cell = crate::start::StartupCell::new();
        cell.prepare(
            verb.waiting(),
            parent.map(|p| StartEdge {
                kind: if matches!(verb, Verb::Link(_)) {
                    EdgeKind::Link
                } else {
                    EdgeKind::Monitor
                },
                parent: p.signal.clone(),
            }),
            DEFAULT_STOP_DEADLINE,
            c,
        );
        let link_parent = match &verb {
            Verb::Link(p) => Some(p.id),
            _ => None,
        };
        let signal = signal_target::<A>(
            id,
            link_parent,
            &tx,
            trap_exit.clone(),
            linked_reason.clone(),
            cancellation_token.clone(),
            cell.clone(),
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
            replies: Registry::new_native(
                id,
                options.max_outstanding.unwrap_or(A::MAX_OUTSTANDING),
            ),
            jobs: Arc::new(crate::reply::jobs::JobSet::new(
                id,
                options
                    .max_native_jobs
                    .or(A::MAX_NATIVE_JOBS)
                    .unwrap_or_else(crate::reply::jobs::default_max_native_jobs),
            )),
            cleaned: Arc::default(),
        };
        let child_handle = actor_ref.child_handle();

        // Commit: the parent's edge and the name, all or nothing (lock
        // order: the parent's table, its exit request, the registry).
        let register = || -> Result<Option<crate::registry::RegistrationGuard>, StartError> {
            let Some(name) = &options.name else {
                return Ok(None);
            };
            match crate::registry::register_start(
                name.key(),
                id,
                actor_ref.clone(),
                child_handle.clone(),
                c,
                || Ok::<(), StartError>(()),
            )? {
                Ok(guard) => Ok(Some(guard)),
                Err(existing) => Err(StartError::AlreadyStarted {
                    name: name.key().to_owned(),
                    existing,
                }),
            }
        };
        let committed = trace::start_section(|| {
            trace::commit_begin(c, "U");
            let result = (|| -> Result<_, StartError> {
                match &verb {
                    Verb::Link(p) => {
                        let mut inner = Ok(None);
                        let installed =
                            link::install_start_edge(&p.signal, &p.links, &signal, &links, || {
                                let request = link::lock_request(&p.request);
                                if request.reason().is_some() || request.is_dead() {
                                    trace::commit_point(Some("parentexited"));
                                    return false;
                                }
                                inner = register();
                                if inner.is_ok() {
                                    trace::commit_point(None);
                                }
                                inner.is_ok()
                            });
                        match (installed, inner) {
                            (_, Err(error)) => Err(error),
                            (false, Ok(_)) => Err(StartError::ParentExited),
                            (true, Ok(name)) => Ok((name, None)),
                        }
                    }
                    Verb::Monitor(p) => {
                        let monitor_ref = MonitorRef::next();
                        let active = spawned_rt::sync::CancellationToken::new();
                        let mut inner = Ok(None);
                        let registered = crate::monitor::fence::register(
                            &p.monitors,
                            monitor_ref,
                            active.clone(),
                            || {
                                let request = link::lock_request(&p.request);
                                if request.reason().is_some()
                                    || request.is_dead()
                                    || p.token.is_cancelled()
                                {
                                    trace::commit_point(Some("parentexited"));
                                    return true;
                                }
                                inner = register();
                                if inner.is_ok() {
                                    trace::commit_point(None);
                                }
                                inner.is_err()
                            },
                            || false,
                            || {},
                            || {},
                        );
                        match (registered, inner) {
                            (_, Err(error)) => Err(error),
                            (crate::monitor::fence::Registration::Active, Ok(name)) => {
                                Ok((name, Some((monitor_ref, active))))
                            }
                            _ => Err(StartError::ParentExited),
                        }
                    }
                    Verb::Spawn | Verb::Start => Ok((register()?, None)),
                }
            })();
            trace::commit_end(result.as_ref().err().map(StartError::trace_refusal));
            result
        });
        let (name, monitor) = committed?;
        // A waiting start_link/start_monitor: the parent's exit by a signal
        // abandons the wait (OTP: the caller dies at once).
        cell.watch_parent();
        let monitor_ref = monitor.map(|(monitor_ref, active)| {
            if let Some(arm) = arm {
                arm(child_handle.clone(), monitor_ref, active, cell.clone());
            }
            monitor_ref
        });

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

        // Recorded from here: the start_link link is the model's initial one.
        crate::lifecycle_trace::life_spawn(id.raw(), link_parent.map_or(0, |p| p.raw()));
        if let crate::start::Timeout::After(after) = options.init_timeout {
            crate::start::spawn_init_timer(&cell, child_handle.clone(), after);
        }

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
        .with_jobs(actor_ref.jobs.clone())
        .with_startup(cell, link_parent)
        .with_name(name);
        let startup = crate::actor_cleanup::ActorStartup::new(
            (actor, ctx, rx, cancellation_token),
            cleanup,
            discharge_startup::<A>,
        );
        let replies = actor_ref.replies.clone();
        let jobs = actor_ref.jobs.clone();
        let cleaned = crate::reply::jobs::CleanedGuard(actor_ref.cleaned.clone());
        let body = move || {
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
            // The child's owned work is done (`Start.tla` WorkDoneC).
            let c = crate::lifecycle_trace::start_child_of(jobs.owner().raw());
            crate::lifecycle_trace::start_emit(c, || {
                crate::lifecycle_trace::StartEvent::new("SWorkDone", c)
            });
        };
        let mut builder = std::thread::Builder::new();
        if let Some(bytes) = options.stack_size {
            builder = builder.stack_size(bytes);
        }
        if let Some(label) = &options.label {
            builder = builder.name(label.to_string());
        }
        // A spawn failure after the commit kills the child before started()
        // (`InitExited { Killed }`), not a stage-1 refusal.
        crate::start::spawn_thread_or_kill(
            builder,
            &child_handle,
            body,
            |body| body(),
            |body| body(),
        );
        Ok((actor_ref, monitor_ref))
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
    let request = ctx.linked_reason.clone();
    let trace_id = ctx.id.raw();
    let _monitor_cleanup = MonitorCleanup(ctx.monitors.clone());
    let cell = ctx.signal.startup.clone();
    // A kill or the init timer before started() began: it is not run.
    let early = crate::lifecycle_trace::start_section(|| {
        let killed = link::lock_request(&request).killed_early()
            || cell.commit() == crate::start::Commit::TimedOut;
        if !killed {
            let c = cell.trace_id();
            crate::lifecycle_trace::start_emit(c, || {
                crate::lifecycle_trace::StartEvent::new("SStartedBegin", c)
            });
        }
        killed
    });
    let (returned, continuation) = if early {
        (crate::start::Returned::NotBegun, None)
    } else {
        let start_result = catch_unwind(AssertUnwindSafe(|| {
            fail_point!("actor.before_started");
            crate::lifecycle_trace::with_actor(trace_id, || actor.started(&ctx).into())
        }));
        match start_result {
            Err(panic) => {
                let msg = panic_message(&*panic);
                crate::actor_cleanup::log_safely(|| {
                    tracing::error!("Panic in started() callback: {msg}");
                });
                (crate::start::Returned::Panic(msg), None)
            }
            Ok(init) => {
                let init: Init<A> = init;
                init.into_returned()
            }
        }
    };
    // The startup decision (design 2.3) and the readiness commit (5.1),
    // under the exit-request and cell locks. No failed start runs stopped().
    let trapping = ctx.trap_exit.load(Ordering::Acquire);
    let continued = continuation.is_some();
    let row = crate::lifecycle_trace::start_section(|| {
        crate::lifecycle_trace::life_section(trace_id, || {
            let mut req = link::lock_request(&request);
            let row = cell.decide(&returned, continued, trapping, |timed_out| {
                let row = req.resolve_start(trapping, timed_out, &returned, true);
                let cause = match (&row, &returned) {
                    (crate::start::Row::Error, crate::start::Returned::Error(cause)) => {
                        Some(cause.clone())
                    }
                    _ => None,
                };
                (row, req.reason().cloned(), cause)
            });
            drop(req);
            if row == crate::start::Row::Panic {
                crate::lifecycle_trace::life_emit(trace_id, || {
                    crate::lifecycle_trace::LifeEvent::new("Panic", trace_id).x("started")
                });
            }
            row
        })
    });
    if row != crate::start::Row::Ready {
        if let (crate::start::Returned::Error(cause), false) =
            (&returned, row == crate::start::Row::Error)
        {
            let id = ctx.id;
            crate::actor_cleanup::log_safely(|| {
                tracing::debug!(actor = %id, cause, ?row, "init error superseded by a higher-precedence exit");
            });
        }
        cancellation_token.cancel();
        return None;
    }
    // A continuation runs first (L10); a stop observed before it begins
    // discards it, as queued messages.
    let mut panicked = false;
    if let Some(envelope) = continuation {
        if cancellation_token.is_cancelled() {
            discarded = Some(envelope);
        } else {
            let c = cell.trace_id();
            crate::lifecycle_trace::start_emit(c, || {
                crate::lifecycle_trace::StartEvent::new("SContinue", c)
            });
            ctx.replies.begin_turn();
            let result = catch_unwind(AssertUnwindSafe(|| {
                crate::lifecycle_trace::with_actor(trace_id, || envelope.handle(actor, &ctx));
            }));
            ctx.replies.end_turn(result.is_err());
            if let Err(panic) = result {
                let msg = panic_message(&*panic);
                crate::actor_cleanup::log_safely(|| {
                    tracing::error!("Panic in message handler: {msg}");
                });
                crate::lifecycle_trace::life_section(trace_id, || {
                    crate::lifecycle_trace::life_emit(trace_id, || {
                        crate::lifecycle_trace::LifeEvent::new("Panic", trace_id).x("turn")
                    });
                    link::lock_request(&request)
                        .panic(ExitReason::Panic(format!("panic in handler: {msg}")));
                });
                panicked = true;
            }
        }
    }

    if !panicked {
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
                        #[cfg(feature = "lifecycle-trace")]
                        crate::lifecycle_trace::start_dispatch(trace_id, envelope.trace_kind());
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
                        crate::lifecycle_trace::with_actor(trace_id, || {
                            envelope.handle(actor, &ctx)
                        });
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
