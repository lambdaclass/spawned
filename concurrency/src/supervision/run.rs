//! The supervisor coordinator and the per-child runner loops (start, restart with backoff, cohort rounds, shutdown).
use super::*;

pub(super) async fn run_supervisor(core: Arc<Core>, runners: Vec<Box<dyn Runner>>) {
    // All coordinator phases share a boundary. Keep ownership and join progress
    // outside it so a panic during startup, stopping, joining or final publication
    // cannot detach a controller or lose the runtime/subtree completion signal.
    let mut runners = Some(runners);
    let mut tasks = Vec::new();
    let mut joined = 0;
    let mut coordination = None;
    let mut emergency = false;
    loop {
        let result = AssertUnwindSafe(async {
            if let Some(runners) = runners.take() {
                run_supervisor_body(core.clone(), runners, &mut tasks, &mut coordination).await;
            }
            if emergency {
                let children = system::lock(&core.children).clone();
                let mut signalled = true;
                for child in children.iter().rev() {
                    if *child.done().borrow() {
                        continue;
                    }
                    if let Err(payload) = std::panic::catch_unwind(AssertUnwindSafe(|| {
                        child.finish_unstarted(true);
                        child.stop();
                    })) {
                        let message = crate::error::panic_message(&*payload);
                        crate::mailbox::dispose_panic(payload);
                        core.record_cleanup_failure(&format!(
                            "emergency child stop panicked: {message}"
                        ));
                        signalled = false;
                    }
                }
                if !signalled {
                    return false;
                }
            }
            while joined < tasks.len() {
                let result = (&mut tasks[joined]).await;
                joined += 1; // Never poll a consumed JoinHandle after a later panic.
                if let Err(error) = result {
                    let message = format!("child controller failed: {error}");
                    if error.is_panic() {
                        crate::mailbox::dispose_panic(error.into_panic());
                    }
                    core.record_cleanup_failure(&message);
                    core.fail_as_kind(FailureKind::Coordinator, message, || {
                        Some(Event::new("Unmodelled", 0, "ControllerFailed"))
                    });
                }
            }
            if let Some(task) = coordination.as_mut() {
                let result = task.await;
                coordination = None;
                if let Err(error) = result {
                    let message = format!("recovery coordinator failed: {error}");
                    if error.is_panic() {
                        crate::mailbox::dispose_panic(error.into_panic());
                    }
                    core.record_cleanup_failure(&message);
                    core.fail_kind(FailureKind::Coordinator, message);
                }
            }
            let children = system::lock(&core.children).clone();
            for child in &children {
                child.finish_unstarted(core.failed.load(std::sync::atomic::Ordering::Acquire));
            }
            core.log_failure_once();
            trace::atomic(|| {
                trace::emit(core.trace, || Event::sup("SupJoin"));
                core.cohort(|| Event::sup("CDone"));
                trace::tree(core.trace, |id| TreeEvent::new("TDone", id));
                // Copy the tapes before `done` lets a test `take` them.
                #[cfg(feature = "lifecycle-trace")]
                trace::dump_all(core.trace);
                core.done.send_replace(true);
            });
            true
        })
        .catch_unwind()
        .await;
        match result {
            Ok(true) => return,
            Ok(false) => (),
            Err(payload) => {
                let message = crate::error::panic_message(&*payload);
                crate::mailbox::dispose_panic(payload);
                let message = format!("supervisor coordinator panicked: {message}");
                core.record_cleanup_failure(&message);
                core.fail_kind(FailureKind::Coordinator, message);
                emergency = true;
            }
        }
        rt::sleep(Duration::from_millis(10)).await;
    }
}

pub(super) async fn run_supervisor_body(
    core: Arc<Core>,
    runners: Vec<Box<dyn Runner>>,
    tasks: &mut Vec<rt::JoinHandle<()>>,
    coordination: &mut Option<rt::JoinHandle<()>>,
) {
    let children = system::lock(&core.children).clone();
    *coordination = if core.config.strategy == SupervisorStrategy::OneForOne {
        None
    } else {
        let coordinator = core.clone();
        Some(rt::spawn(async move {
            if let Err(payload) = AssertUnwindSafe(run_recoveries(coordinator.clone()))
                .catch_unwind()
                .await
            {
                let message = format!(
                    "recovery coordinator panicked: {}",
                    crate::error::panic_message(&*payload)
                );
                crate::mailbox::dispose_panic(payload);
                coordinator.record_cleanup_failure(&message);
                coordinator.fail_kind(FailureKind::Coordinator, message);
                coordinator.stop_remaining();
            }
        }))
    };
    for (index, runner) in runners.into_iter().enumerate() {
        let open = trace::atomic(|| {
            let open = core.gate.is_open();
            if open {
                trace::emit(core.trace, || Event::new("SupLaunch", index + 1, ""));
            }
            open
        });
        if !open {
            break;
        }
        tasks.push(rt::spawn(runner.run(core.clone())));
        let event = select(
            Box::pin(core.admission_closed.cancelled()),
            Box::pin(system::wait_true(children[index].initial())),
        )
        .await;
        if !matches!(event, Either::Right((true, _))) {
            break;
        }
    }
    for child in &children {
        child.finish_unstarted(core.failed.load(std::sync::atomic::Ordering::Acquire));
    }
    let initialized = trace::atomic(|| {
        let committed = core.gate.if_open(|| {
            trace::emit(core.trace, || Event::new("SupStartDone", 0, "true"));
            core.cohort(|| Event::sup("CInitialReady"));
            core.ready.send_replace(true);
        });
        if committed.is_none() {
            trace::emit(core.trace, || Event::new("SupStartDone", 0, "false"));
        }
        committed.is_some()
    });

    #[cfg(test)]
    let ready_probe = {
        let pause = system::lock(&core.initial_ready_pause).take();
        if let Some((resume, passed)) = pause {
            resume.cancelled().await;
            Some(passed)
        } else {
            None
        }
    };
    // Once readiness commits, later admission closure is not a stop turn.
    // ActorSystem owns cross-target ordering and cancels this tree in its turn.
    if !initialized {
        core.close();
    }
    #[cfg(test)]
    if let Some(passed) = ready_probe {
        passed.send_replace(true);
    }
    core.cancel.cancelled().await;
    trace::emit(core.trace, || Event::sup("SupCancelSeen"));
    for (index, child) in children.iter().enumerate().rev() {
        let c = index + 1;
        trace::emit(core.trace, || Event::new("SupStopTurn", c, ""));
        fail_point!("shutdown.before_stop_child", async);
        child.stop();
        // V3: the controller kills at shutdown_timeout and is stuck only
        // after a second interval; this wait is a backstop beyond both (the
        // round coordinator's too), so the tree is not failed before the kill.
        let backstop = child.shutdown_timeout().saturating_mul(3);
        let finished = rt::timeout(backstop, system::wait_true(child.done()))
            .await
            .is_ok();
        // The child may finish between the deadline firing and this check;
        // decide on its published completion, atomically with recording.
        trace::atomic(|| {
            if finished || *child.done().borrow() {
                trace::emit(core.trace, || Event::new("SupChildDone", c, ""));
            } else {
                core.fail_cleanup_for(
                    child.snapshot(),
                    format!("child cleanup deadline exceeded: {}", child.snapshot().name),
                    || Some(Event::new("SupChildTimeout", c, "")),
                );
            }
        });
    }
}

pub(super) async fn run_recoveries(core: Arc<Core>) {
    let children = system::lock(&core.children).clone();
    let mut changes = core.recovery_changed.subscribe();
    'rounds: loop {
        changes.borrow_and_update();
        if !core.gate.is_open() {
            return;
        }
        let round = trace::atomic(|| {
            let mut queue = system::lock(&core.recovery);
            // The round is the restart action: charge it once, before any
            // child of the cohort is stopped or rebuilt.
            let origin = queue.pending.iter().position(|pending| *pending)?;
            if !core.charge_round() {
                return Some(Err(origin));
            }
            queue
                .select(core.config.strategy == SupervisorStrategy::OneForAll)
                .map(|(origin, start, id)| {
                    let meta = Arc::new(RecoveryMeta {
                        id,
                        origin,
                        finished: std::sync::atomic::AtomicBool::new(false),
                    });
                    core.cohort(|| {
                        Event::new("CSelect", origin + 1, "")
                            .round(id)
                            .count(start + 1)
                    });
                    Ok((start, meta))
                })
        });
        if let Some(Err(origin)) = round {
            // Exhausted: a concurrent stop wins; only an open supervisor fails.
            let snapshot = children[origin].snapshot();
            let message = format!("restart budget exhausted by {}", snapshot.name);
            core.fail_record_if_open_as(
                snapshot_failure(snapshot, FailureKind::RestartBudget, message),
                || None,
            );
            core.log_failure_once();
            return;
        }
        let Some(Ok((start, meta))) = round else {
            if matches!(
                select(
                    Box::pin(core.admission_closed.cancelled()),
                    Box::pin(changes.changed())
                )
                .await,
                Either::Left(_)
            ) {
                return;
            }
            continue;
        };
        let mut affected = Vec::new();
        for (affected_index, child) in children.iter().enumerate().skip(start) {
            let slot = RecoverySlot::new(meta.clone());
            let accepted = trace::atomic(|| {
                let accepted = child.assign_recovery(slot.clone());
                core.cohort(|| {
                    Event::new("CAssign", affected_index + 1, trace::flag(accepted)).round(meta.id)
                });
                accepted
            });
            if accepted {
                affected.push((affected_index + 1, child.clone(), slot));
            }
        }
        for (index, child, slot) in affected.iter().rev() {
            if !core.gate.is_open() {
                return;
            }
            trace::atomic(|| {
                child.stop_generation(slot);
                core.cohort(|| Event::new("CStop", *index, "").round(meta.id));
            });
            // The child's controller kills it at shutdown_timeout and
            // declares its slot stuck (failing the tree, which closes
            // admission) a second interval later (V3); this wait outlasts
            // both, as a backstop. Both start at the stop above (the
            // controller observes the slot's stop token, not the actor), so
            // it loses only to a controller starved for a whole interval. It
            // stays bounded because a child still constructing when its
            // round stops it is bounded by startup_timeout, not this.
            match select(
                Box::pin(core.admission_closed.cancelled()),
                Box::pin(rt::timeout(
                    child.shutdown_timeout().saturating_mul(3),
                    system::wait_true(slot.cleaned.subscribe()),
                )),
            )
            .await
            {
                Either::Left(_) => return,
                Either::Right((Ok(true), _)) => (),
                _ => {
                    core.fail_cleanup_for(
                        child.snapshot(),
                        format!("coordinated cleanup stuck for {}", child.snapshot().name),
                        || None,
                    );
                    core.stop_remaining();
                    return;
                }
            }
        }
        for (index, _, slot) in &affected {
            if !core.gate.is_open() {
                return;
            }
            if system::lock(&core.recovery)
                .pending
                .iter()
                .any(|pending| *pending)
            {
                trace::atomic(|| {
                    core.cohort(|| Event::new("CEnd", 0, "superseded").round(meta.id));
                    meta.finished
                        .store(true, std::sync::atomic::Ordering::Release);
                });
                continue 'rounds;
            }
            trace::atomic(|| {
                core.cohort(|| Event::new("CResume", *index, "").round(meta.id));
                slot.resume.cancel();
            });
            loop {
                changes.borrow_and_update();
                // A ready upstream or previously unaffected dependency can fail
                // while this child initializes. Supersede the round rather than
                // await a dependent that cannot start without that upstream.
                if system::lock(&core.recovery)
                    .pending
                    .iter()
                    .any(|pending| *pending)
                {
                    trace::atomic(|| {
                        core.cohort(|| Event::new("CEnd", 0, "superseded").round(meta.id));
                        meta.finished
                            .store(true, std::sync::atomic::Ordering::Release);
                    });
                    continue 'rounds;
                }
                match select(
                    Box::pin(core.admission_closed.cancelled()),
                    Box::pin(select(
                        Box::pin(system::wait_true(slot.ready.subscribe())),
                        Box::pin(changes.changed()),
                    )),
                )
                .await
                {
                    Either::Left(_) => return,
                    Either::Right((Either::Left((true, _)), _)) => break,
                    Either::Right((Either::Left((false, _)), _)) => return,
                    Either::Right((Either::Right(_), _)) => (),
                }
            }
        }
        trace::atomic(|| {
            core.cohort(|| Event::new("CEnd", 0, "complete").round(meta.id));
            meta.finished
                .store(true, std::sync::atomic::Ordering::Release);
        });
    }
}

pub(super) async fn run_child<A: Actor>(spec: ChildSpec<A>, view: Arc<View<A>>, core: Arc<Core>) {
    let mut first = true;
    let mut last_recovery = 0;
    let mut collateral = false;
    // This iteration rebuilds for a recovery round (already charged).
    let mut in_round = false;
    let mut failures = 0u32;
    // Wall-clock entropy; the simulation seed's entropy stream under spawned_sim.
    let seed = core
        .config
        .backoff
        .seed
        .unwrap_or_else(spawned_rt::time::entropy);
    let mut rng = jitter_seed(seed, system::lock(&view.data).snapshot.name.as_bytes());
    let c = view.trace_index;
    loop {
        let open = trace::atomic(|| {
            let open = core.gate.is_open();
            let branch = if !open {
                "finish"
            } else if first {
                "attempt"
            } else {
                "backoff"
            };
            trace::emit(core.trace, || Event::new("LoopTop", c, branch));
            open
        });
        if !open {
            break;
        }
        if let Some(round) = view.recovery(last_recovery) {
            if matches!(
                select(
                    Box::pin(core.admission_closed.cancelled()),
                    Box::pin(round.stop.cancelled())
                )
                .await,
                Either::Left(_)
            ) {
                break;
            }
            trace::atomic(|| {
                core.cohort(|| Event::new("CClean", c, "").round(round.meta.id));
                round.cleaned.send_replace(true);
            });
            match select(
                Box::pin(core.admission_closed.cancelled()),
                Box::pin(select(
                    Box::pin(view.next_recovery(round.meta.id)),
                    Box::pin(round.resume.cancelled()),
                )),
            )
            .await
            {
                Either::Left(_) => break,
                Either::Right((Either::Left(_), _)) => continue,
                Either::Right((Either::Right(_), _)) => (),
            }
            last_recovery = round.meta.id;
            if spec.restart == RestartMode::Temporary {
                let failed = system::lock(&core.failure).is_some();
                // Publish terminal state under the same lock as assignment before
                // acknowledging the round. A superseding coordinator must either
                // skip this finished slot or be serviced by this controller.
                if trace::atomic(|| {
                    let finished = view.finish_unless_recovering(last_recovery, failed);
                    if finished {
                        trace::emit(core.trace, || Event::new("Finish", c, ""));
                        core.cohort(|| Event::new("CRetire", c, "").round(last_recovery));
                    }
                    finished
                }) {
                    trace::atomic(|| {
                        core.cohort(|| Event::new("CAck", c, "").round(round.meta.id));
                        round.ready.send_replace(true);
                    });
                    core.log_failure_once();
                    return;
                }
                continue;
            }
            collateral = round.meta.origin != view.index;
            in_round = true;
        }
        let mut backoff = Duration::ZERO;
        let mut retry_reservation = None;
        // A recovery round was charged once when the coordinator selected
        // it: its rebuilds (origin and collateral) take no grant of their own.
        let round_rebuild = std::mem::take(&mut in_round);
        if !first {
            fail_point!("supervisor.before_restart", async);
            let reservation = if round_rebuild {
                Some(None)
            } else {
                core.reserve_retry(c).map(Some)
            };
            let Some(reservation) = reservation else {
                // A concurrent stop wins: only fail a supervisor that is still open.
                trace::atomic(|| {
                    if core.gate.is_open() {
                        core.fail_record_if_open_as(
                            child_failure(
                                &spec,
                                &view,
                                FailureKind::RestartBudget,
                                format!("restart budget exhausted by {}", spec.name),
                            ),
                            || {
                                let budget = system::lock(&core.budget);
                                let held = budget.attempts.len() + budget.reserved;
                                Some(Event::new("BackoffDone", c, "exhausted").count(held))
                            },
                        );
                    } else {
                        trace::emit(core.trace, || Event::new("BackoffDone", c, "closed"));
                    }
                });
                core.log_failure_once();
                break;
            };
            let delay = if collateral {
                Duration::ZERO
            } else {
                core.config.backoff.delay(failures, &mut rng)
            };
            backoff = delay;
            let count_attempt = !collateral;
            collateral = false;
            view.change(|v| v.snapshot.state = ChildState::Backoff);
            if matches!(
                select(
                    Box::pin(core.admission_closed.cancelled()),
                    Box::pin(select(
                        Box::pin(async {
                            if core.config.strategy == SupervisorStrategy::OneForOne {
                                futures::future::pending::<()>().await;
                            } else {
                                view.recovery_stopping(last_recovery).await;
                            }
                        }),
                        Box::pin(rt::sleep(delay))
                    ))
                )
                .await,
                Either::Left(_)
            ) {
                drop(reservation);
                trace::emit(core.trace, || Event::new("BackoffDone", c, "closed"));
                break;
            }
            fail_point!("supervisor.during_backoff", async);
            if view.recovery(last_recovery).is_some() {
                continue;
            }
            if count_attempt {
                failures = failures.saturating_add(1);
            }
            retry_reservation = reservation;
        }
        first = false;
        let (scope, lease) = match spec.group.reserve(Some(core.gate.clone())) {
            Ok(attempt) => attempt,
            Err(e) => {
                trace::atomic(|| {
                    let was_retry = retry_reservation.is_some();
                    drop(retry_reservation.take());
                    if core.gate.is_open() {
                        core.fail_record_if_open_as(
                            child_failure(
                                &spec,
                                &view,
                                if *view.initial.borrow() {
                                    FailureKind::Other
                                } else {
                                    FailureKind::FirstStart
                                },
                                format!("placement failed for {}: {e}", spec.name),
                            ),
                            || Some(Event::new("Unmodelled", c, "PlacementFailed")),
                        );
                    } else {
                        trace::emit(core.trace, || {
                            Event::new(
                                if was_retry { "BackoffDone" } else { "Attempt" },
                                c,
                                if was_retry { "closed" } else { "false" },
                            )
                        });
                    }
                });
                break;
            }
        };
        #[cfg(test)]
        if retry_reservation.is_some() || round_rebuild {
            let pause = system::lock(&core.retry_admission_pause).take();
            if let Some((resume, entered)) = pause {
                entered.send_replace(true);
                resume.cancelled().await;
            }
        }
        let generation = trace::atomic(|| {
            let was_retry = retry_reservation.is_some();
            let generation = core
                .gate
                .if_open(|| {
                    let mut data = system::lock(&view.data);
                    if data
                        .recovery
                        .as_ref()
                        .is_some_and(|r| r.meta.id > last_recovery)
                    {
                        return None;
                    }
                    if let Some(reservation) = retry_reservation.as_mut() {
                        if !core.retry_under_gate(reservation) {
                            return None;
                        }
                    }
                    data.snapshot.generation += 1;
                    data.snapshot.state = ChildState::Starting;
                    data.snapshot.actor_id = None;
                    data.current = None;
                    data.scope = Some(scope.clone());
                    Some(data.snapshot.generation)
                })
                .flatten();
            if generation.is_none() && was_retry {
                drop(retry_reservation.take());
                if !core.gate.is_open() {
                    trace::emit(core.trace, || Event::new("BackoffDone", c, "closed"));
                }
            }
            if generation.is_some() || !was_retry {
                trace::emit(core.trace, || {
                    Event::new("Attempt", c, trace::flag(generation.is_some()))
                });
            }
            if let Some(generation) = generation {
                trace::tree(core.trace, |id| {
                    TreeEvent::new("TBegin", id).child(c, generation)
                });
                core.cohort(|| {
                    Event::new("CBegin", c, "")
                        .round(last_recovery)
                        .count(generation as usize)
                });
            }
            generation
        });
        let Some(generation) = generation else {
            scope.stop(ExitReason::Shutdown(None));
            drop(lease);
            if core.gate.is_open() {
                continue;
            } else {
                break;
            }
        };
        scope.set_name(&spec.name);
        if generation > 1 {
            let previous = view.snapshot();
            crate::actor_cleanup::log_safely(|| {
                tracing::info!(event = "restart_scheduled", child = %spec.name,
                generation = generation - 1, reason = ?previous.last_exit,
                error = ?previous.last_error, ?backoff, "child retry granted after backoff");
            });
            crate::actor_cleanup::log_safely(|| {
                tracing::info!(
                    event = "restart_started",
                    child = %spec.name,
                    generation,
                    reason = ?previous.last_exit,
                    error = ?previous.last_error,
                    "starting replacement child"
                );
            });
        }
        view.changed.send_modify(|n| *n = n.wrapping_add(1));
        let ctx = FactoryContext {
            scope: scope.clone(),
            generation,
        };
        let factory = spec.factory.clone();
        scope.set_bounds(spec.bounds);
        let attempt_scope = scope.clone();
        let construction = async move {
            if !attempt_scope.can_start() {
                return Err("scope closed before construction".into());
            }
            let factory_work = AssertUnwindSafe(async move {
                fail_point!("child.factory", async => return Err("failpoint child.factory".into()));
                factory(ctx).await
            })
            .catch_unwind();
            let result = match select(
                Box::pin(attempt_scope.token.cancelled()),
                Box::pin(factory_work),
            )
            .await
            {
                Either::Left(_) => return Err("construction cancelled".into()),
                Either::Right((Ok(result), _)) => result,
                Either::Right((Err(payload), _)) => {
                    let message = crate::error::panic_message(&*payload);
                    // The payload's destructor is user code (L6).
                    crate::mailbox::dispose_panic(payload);
                    return Err(format!("factory panicked: {message}"));
                }
            };
            let actor = result?;
            let actor_lease = attempt_scope.actor_lease().map_err(|e| e.to_string())?;
            ActorRef::spawn_managed(actor, attempt_scope.clone(), actor_lease)
                .map_err(|e| e.to_string())
        };
        let task = scope.handle.spawn(Tracked {
            future: construction,
            lease,
        });
        let startup = async {
            let actor = task
                .await
                .map_err(|e| format!("construction task failed: {e}"))??;
            actor.ready().await.map_err(|e| e.to_string())?;
            Ok::<_, String>(actor)
        };
        let mut startup_timed_out = false;
        let began = match select(
            Box::pin(core.admission_closed.cancelled()),
            Box::pin(rt::timeout(spec.startup_timeout, startup)),
        )
        .await
        {
            Either::Left(_) => Err("supervisor stopping".to_string()),
            Either::Right((Err(_), _)) => {
                startup_timed_out = true;
                Err("startup deadline exceeded".to_string())
            }
            Either::Right((Ok(result), _)) => result,
        };
        #[cfg(feature = "failpoints")]
        let began = match began {
            Ok(actor) => {
                let mut began = Ok(actor);
                fail_point!("child.after_ready_before_publish", async => {
                    began = Err("failpoint child.after_ready_before_publish".to_string())
                });
                began
            }
            failed => failed,
        };
        let mut ready_at = None;
        let mut exit = None;
        let mut cleanup_until = None;
        // Trace bookkeeping: the model's StartupFailed (whose reason is known
        // only after cleanup) and CleanupStuck (which skips CleanupDone and
        // Decide).
        let startup_failed = began.is_err();
        let mut stuck = false;
        match began {
            Ok(actor) => {
                let published = trace::atomic(|| {
                    let published = core
                        .gate
                        .if_open(|| {
                            let mut data = system::lock(&view.data);
                            if data
                                .recovery
                                .as_ref()
                                .is_some_and(|r| r.meta.id > last_recovery)
                            {
                                return false;
                            }
                            data.snapshot.state = ChildState::Running;
                            data.snapshot.actor_id = Some(actor.id());
                            data.current = Some(actor.clone());
                            true
                        })
                        .unwrap_or(false);
                    trace::emit(core.trace, || {
                        Event::new("BecomeReady", c, trace::flag(published))
                    });
                    if published {
                        core.cohort(|| {
                            Event::new("CReady", c, "")
                                .round(last_recovery)
                                .count(generation as usize)
                        });
                    }
                    published
                });
                view.changed.send_modify(|n| *n = n.wrapping_add(1));
                if published {
                    ready_at = Some(Instant::now());
                    view.initial.send_replace(true);
                    let round = system::lock(&view.data).recovery.clone();
                    if let Some(round) = round.filter(|r| r.meta.id == last_recovery) {
                        trace::atomic(|| {
                            core.cohort(|| Event::new("CAck", c, "").round(round.meta.id));
                            round.ready.send_replace(true);
                        });
                    }
                    let child = actor.child_handle();
                    // Cancellation precedes terminal publication when stopped()
                    // hangs. Start its cleanup deadline at stop observation,
                    // rather than waiting forever for the completion channel.
                    //
                    // V3: a OneForAll/RestForOne round's stop is the
                    // supervisor's too: the same kill at shutdown_timeout,
                    // and stuck only after a second interval. It is observed
                    // on the round's own token, which `stop_generation`
                    // cancels before its parent exit signal: a trapping
                    // child queues that signal behind its current turn, so
                    // its scope token ends only at its death point and the
                    // deadline must not wait for it. A child that exited on
                    // its own is not killed (that would relabel its reason).
                    let round_stopping = async {
                        if core.config.strategy == SupervisorStrategy::OneForOne {
                            futures::future::pending().await
                        } else {
                            view.recovery_stopping(last_recovery).await
                        }
                    };
                    let ended = select(
                        Box::pin(core.cancel.cancelled()),
                        Box::pin(select(
                            Box::pin(round_stopping),
                            Box::pin(scope.token.cancelled()),
                        )),
                    )
                    .await;
                    let (stop_turn, round_stop) = match ended {
                        Either::Left(_) => (true, None),
                        Either::Right((Either::Left((round, _)), _)) => {
                            (false, Some(round.meta.id))
                        }
                        // The round arm is polled first, so this is a self
                        // exit unless the round's signal ended a
                        // non-trapping child between the two polls: its stop
                        // was cancelled before that signal, so it shows.
                        Either::Right((Either::Right(_), _)) => (
                            false,
                            view.recovery(last_recovery)
                                .filter(|round| round.stop.is_cancelled())
                                .map(|round| round.meta.id),
                        ),
                    };
                    let supervisor_stop = stop_turn || round_stop.is_some();
                    if stop_turn {
                        view.stop_turn.cancelled().await;
                        trace::emit(core.trace, || Event::new("StopObserved", c, ""));
                        // ChildView::stop already sent the parent's Shutdown.
                    }
                    view.change(|v| {
                        v.current = None;
                        v.snapshot.state = ChildState::Stopping;
                    });
                    let cleanup_started = Instant::now();
                    cleanup_until = Some(cleanup_started);
                    // The child's jobs are joined within the same deadline.
                    scope.set_cleanup_timeout(spec.shutdown_timeout);
                    exit = Some(
                        match rt::timeout(spec.shutdown_timeout, child.wait_exit_async()).await {
                            Ok(reason) => {
                                if stop_turn {
                                    trace::emit(core.trace, || Event::new("StopExited", c, ""));
                                } else {
                                    let failed = child.cleanup_failure().is_some();
                                    trace::emit(core.trace, || {
                                        Event::new("SelfExit", c, exit_class(&reason, failed))
                                    });
                                }
                                reason
                            }
                            // E3: the supervisor stopped this child and it
                            // did not reach its death point within
                            // shutdown_timeout: the parent's kill, then a
                            // second interval before cleanup is stuck.
                            Err(_) if supervisor_stop => {
                                fail_point!("supervisor.before_stop_kill", async);
                                trace::atomic(|| {
                                    scope.kill();
                                    if stop_turn {
                                        trace::emit(core.trace, || {
                                            Event::new("CleanupKill", c, "")
                                        });
                                    }
                                    if let Some(round) = round_stop {
                                        core.cohort(|| Event::new("CKill", c, "").round(round));
                                    }
                                });
                                // The second interval runs from the kill.
                                cleanup_until = Some(Instant::now());
                                match rt::timeout(spec.shutdown_timeout, child.wait_exit_async())
                                    .await
                                {
                                    Ok(reason) => {
                                        if stop_turn {
                                            trace::emit(core.trace, || {
                                                Event::new("StopExited", c, "")
                                            });
                                        }
                                        reason
                                    }
                                    Err(_) => {
                                        stuck = true;
                                        // Reply traces: the cleanup deadline
                                        // passed before the child's death
                                        // point (DeferredReplies `CleanupDeadline`).
                                        crate::lifecycle_trace::reply(
                                            child.id().raw(),
                                            "Stuck",
                                            0,
                                            "",
                                        );
                                        if let Some(round) = round_stop {
                                            trace::atomic(|| {
                                                core.cohort(|| {
                                                    Event::new("CStuck", c, "").round(round)
                                                })
                                            });
                                        }
                                        core.fail_cleanup_for(
                                            view.snapshot(),
                                            format!("actor cleanup stuck for {}", spec.name),
                                            || Some(Event::new("CleanupStuck", c, "")),
                                        );
                                        view.mark_stuck();
                                        child.wait_exit_async().await
                                    }
                                }
                            }
                            Err(_) => {
                                if !stop_turn {
                                    trace::emit(core.trace, || {
                                        Event::new("SelfExit", c, "unknown")
                                    });
                                }
                                stuck = true;
                                crate::lifecycle_trace::reply(child.id().raw(), "Stuck", 0, "");
                                // Unreachable for a round's stop (it takes
                                // the kill above); recorded so a regression
                                // that skips the kill is a CStuck without a
                                // CKill, which Cohorts.tla rejects.
                                if let Some(round) = round_stop {
                                    trace::atomic(|| {
                                        core.cohort(|| Event::new("CStuck", c, "").round(round))
                                    });
                                }
                                core.fail_cleanup_for(
                                    view.snapshot(),
                                    format!("actor cleanup stuck for {}", spec.name),
                                    || Some(Event::new("CleanupStuck", c, "")),
                                );
                                view.mark_stuck();
                                child.wait_exit_async().await
                            }
                        },
                    );
                }
            }
            Err(error) => view.change(|v| v.snapshot.last_error = Some(error)),
        }
        let startup_reason = |exit: Option<&ExitReason>| {
            if startup_timed_out {
                "timeout"
            } else {
                trace::reason(exit)
            }
        };
        scope.stop(exit.clone().unwrap_or(ExitReason::Shutdown(None)));
        view.change(|v| {
            v.current = None;
            v.snapshot.last_exit = exit.clone();
            // A stuck cleanup already settled the snapshot as Failed (S2).
            if v.snapshot.state != ChildState::Failed {
                v.snapshot.state = ChildState::Stopping;
            }
        });
        fail_point!("child.before_cleanup_join", async);
        let remaining = cleanup_until
            .map(|started| spec.shutdown_timeout.saturating_sub(started.elapsed()))
            .unwrap_or(spec.shutdown_timeout);
        if rt::timeout(remaining, scope.joined()).await.is_err() {
            core.fail_cleanup_for(
                view.snapshot(),
                format!("owned cleanup stuck for {}", spec.name),
                || {
                    if stuck {
                        return None;
                    }
                    if startup_failed {
                        let reason = match scope.child().map(|child| child.exit_reason()) {
                            Some(None) if !startup_timed_out => "unknown",
                            Some(reason) => startup_reason(reason.as_ref()),
                            None => startup_reason(None),
                        };
                        trace::emit(core.trace, || Event::new("StartupFailed", c, reason));
                    }
                    Some(Event::new("CleanupStuck", c, ""))
                },
            );
            view.mark_stuck();
            scope.joined().await;
            trace::atomic(|| {
                trace::tree(core.trace, |id| {
                    TreeEvent::new("TJoined", id).child(c, generation)
                });
                core.cohort(|| Event::new("CJoined", c, "").count(generation as usize));
            });
            break;
        }
        trace::atomic(|| {
            trace::tree(core.trace, |id| {
                TreeEvent::new("TJoined", id).child(c, generation)
            });
            core.cohort(|| Event::new("CJoined", c, "").count(generation as usize));
        });
        if exit.is_none() {
            exit = scope.child().and_then(|child| child.exit_reason());
        }
        if !stuck {
            trace::atomic(|| {
                if startup_failed {
                    trace::emit(core.trace, || {
                        Event::new("StartupFailed", c, startup_reason(exit.as_ref()))
                    });
                }
                trace::emit(core.trace, || Event::new("CleanupDone", c, ""));
            });
        }
        let cleanup_failure = scope.child().and_then(|child| child.cleanup_failure());
        view.change(|v| {
            v.snapshot.last_exit = exit.clone();
            if cleanup_failure.is_some() {
                v.snapshot.last_error = cleanup_failure.clone();
            }
            if v.snapshot.actor_id.is_none() {
                v.snapshot.actor_id = scope.child().map(|child| child.id());
            }
            v.scope = None;
        });
        // A stable generation resets its failure streak even when another
        // child's recovery supersedes this controller during cleanup.
        if scope.was_stable(core.config.backoff.stable_after) {
            failures = 0;
        }
        if core.gate.is_open() && view.recovery(last_recovery).is_some() {
            continue;
        }
        // S1: a child's first start that fails (any cause) aborts the
        // supervisor's start whatever the restart type, as OTP's
        // `start_children` returns `failed_to_start_child`.
        let first_start = ready_at.is_none() && !*view.initial.borrow();
        // A generation that never became ready after the child's initial
        // readiness is a failed restart attempt: retried within the budget
        // whatever its exit reason, as OTP retries a failed restart
        // (`try_again_restart`), unless Temporary.
        let failed_restart = ready_at.is_none() && !first_start;
        let restart = if first_start {
            false
        } else if startup_timed_out || failed_restart {
            spec.restart != RestartMode::Temporary
        } else {
            spec.restart
                .restarts(exit.as_ref(), cleanup_failure.is_some())
        };
        // S1: a ready child's exit during the initial start is recorded and
        // processed once the tree commits ready, as OTP handles the queued
        // 'EXIT' after `init` returns. If the start aborts instead, the
        // exit is part of the rollback. Only the decision waits: cleanup
        // already ran within its deadline.
        let deferred = ready_at.is_some()
            && trace::atomic(|| {
                let defer = core.gate.is_open() && !*core.ready.borrow();
                if defer {
                    trace::emit(core.trace, || Event::new("DeferExit", c, ""));
                }
                defer
            });
        if deferred {
            let _ = select(
                Box::pin(core.admission_closed.cancelled()),
                Box::pin(system::wait_true(core.ready.subscribe())),
            )
            .await;
            trace::emit(core.trace, || Event::new("Undefer", c, ""));
        }
        // Gate check, decision and any resulting failure happen in one step,
        // so a concurrent stop is never followed by a failure of this child.
        let again = trace::atomic(|| {
            if !core.gate.is_open() {
                if !stuck {
                    trace::emit(core.trace, || Event::new("Decide", c, "closed"));
                }
                return false;
            }
            if spec.group.is_closed() {
                core.fail_record_if_open_as(
                    child_failure(
                        &spec,
                        &view,
                        if first_start {
                            FailureKind::FirstStart
                        } else {
                            FailureKind::Other
                        },
                        format!("execution group closed for {}", spec.name),
                    ),
                    || Some(Event::new("Unmodelled", c, "GroupClosed")),
                );
                return false;
            }
            if restart {
                trace::emit(core.trace, || Event::new("Decide", c, "restart"));
                return true;
            }
            if ready_at.is_some() {
                trace::emit(core.trace, || Event::new("Decide", c, "stop"));
            } else {
                let (kind, message) = if first_start {
                    let snapshot = view.snapshot();
                    let cause = snapshot
                        .last_error
                        .clone()
                        .or_else(|| exit.as_ref().map(|reason| format!("exited {reason}")))
                        .unwrap_or_else(|| "unknown".into());
                    (
                        FailureKind::FirstStart,
                        format!("failed to start child {}: {cause}", spec.name),
                    )
                } else {
                    (
                        FailureKind::Other,
                        format!("non-restartable child failed startup: {}", spec.name),
                    )
                };
                core.fail_record_if_open_as(child_failure(&spec, &view, kind, message), || {
                    Some(Event::new("Decide", c, "fail"))
                });
            }
            false
        });
        if !again {
            if core.config.strategy == SupervisorStrategy::OneForOne || !core.gate.is_open() {
                break;
            }
            let failed = system::lock(&core.failure).is_some();
            if trace::atomic(|| {
                let finished = view.finish_unless_recovering(last_recovery, failed);
                if finished {
                    trace::emit(core.trace, || Event::new("Finish", c, ""));
                    core.cohort(|| Event::new("CRetire", c, "").round(last_recovery));
                }
                finished
            }) {
                core.log_failure_once();
                return;
            }
            continue;
        }
        // A failed replacement inside a recovery round requests a new round
        // like any other failure, so OneForAll stops and rebuilds the whole
        // cohort again (and RestForOne the failed child and its later
        // siblings), as OTP's `try_again_restart` re-runs the strategy.
        if core.config.strategy != SupervisorStrategy::OneForOne && *core.ready.borrow() {
            core.request_recovery(view.index, last_recovery);
            if matches!(
                select(
                    Box::pin(core.admission_closed.cancelled()),
                    Box::pin(view.next_recovery(last_recovery))
                )
                .await,
                Either::Left(_)
            ) {
                break;
            }
        }
    }
    // Record Finish and publish completion together, so the supervisor's
    // deadline check sees both or neither.
    trace::atomic(|| {
        trace::emit(core.trace, || Event::new("Finish", c, ""));
        view.finish(system::lock(&core.failure).is_some());
        core.cohort(|| Event::new("CRetire", c, "").round(last_recovery));
    });
    core.log_failure_once();
}

pub(super) fn child_failure<A: Actor>(
    spec: &ChildSpec<A>,
    view: &View<A>,
    kind: FailureKind,
    message: String,
) -> Failure {
    let mut snapshot = view.snapshot();
    snapshot.name = spec.name.clone();
    snapshot_failure(snapshot, kind, message)
}

pub(super) fn snapshot_failure(
    snapshot: ChildSnapshot,
    kind: FailureKind,
    message: String,
) -> Failure {
    Failure::new(
        kind,
        message,
        Some(FailureOrigin {
            child: snapshot.name,
            generation: snapshot.generation,
            reason: snapshot.last_exit,
            last_error: snapshot.last_error,
        }),
    )
}
