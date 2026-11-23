use super::*;
#[test]
fn jitter_draw_covers_the_whole_range() {
    // `nanos + 1` would overflow at the top of the range; the full range
    // takes the stream value as is.
    assert_eq!(jitter_draw(u64::MAX, 12345), 12345);
    assert_eq!(jitter_draw(u64::MAX, u64::MAX), u64::MAX);
    assert_eq!(jitter_draw(0, 12345), 0);
    assert_eq!(jitter_draw(9, 12345), 12345 % 10);
    let mut rng = 1;
    assert!(jitter(Duration::MAX, &mut rng) <= Duration::from_nanos(u64::MAX));
    assert_ne!(rng, 1, "each draw advances the stream");
}
#[test]
fn jitter_seed_separates_supervisors_and_siblings() {
    // Kani proves injectivity in `seed`; this keeps the property under
    // `cargo test` (and cargo-mutants), which do not run Kani.
    assert_ne!(jitter_seed(1, b"a"), jitter_seed(2, b"a"));
    assert_ne!(jitter_seed(7, b"left"), jitter_seed(7, b"right"));
    assert_eq!(jitter_seed(7, b""), 7);
    assert_eq!(jitter_seed(0, b"ab"), 97 * 31 + 98);
    // The seed is mixed with `^`: `|` and `&` survived cargo-mutants
    // (both still separate the pairs above).
    assert_eq!(jitter_seed(1, b"a"), 1 ^ 97);
}
#[tokio::test]
async fn failure_classification_is_typed_and_terminal_failure_is_sticky() {
    crate::test_wait::within(
        concat!(
            module_path!(),
            "::failure_classification_is_typed_and_terminal_failure_is_sticky"
        ),
        async move {
            let system = ActorSystem::new().unwrap();
            let group = system.async_group("terminal-order", 1).unwrap();
            let mut tree = system.supervisor(SupervisorConfig::default()).unwrap();
            let first = tree
                .add(ChildSpec::new("first", &group, |_| async {
                    Ok::<_, String>(CoordinatorWorker)
                }))
                .unwrap();
            let second = tree
                .add(ChildSpec::new("second", &group, |_| async {
                    Ok::<_, String>(CoordinatorWorker)
                }))
                .unwrap();
            first.view.finish(true);
            first.view.finish_unstarted(false); // stale non-failed sweep races last
            second.view.finish_unstarted(false);
            second.view.finish(true); // failed controller races last
            assert_eq!(first.snapshot().state, ChildState::Failed);
            assert_eq!(second.snapshot().state, ChildState::Failed);
            tree.core
                .fail("restart budget exhausted by a user-provided message".into());
            assert_eq!(
                system::lock(&tree.core.failure).as_ref().unwrap().kind,
                FailureKind::Other
            );
            drop(tree);
            assert!(system.shutdown(Duration::from_secs(10)).await.complete);
        },
    )
    .await;
}
#[tokio::test]
async fn initial_placement_failure_has_deterministic_failed_slots() {
    crate::test_wait::within(
        concat!(
            module_path!(),
            "::initial_placement_failure_has_deterministic_failed_slots"
        ),
        async move {
            use crate::tasks::ActorStart;
            for _ in 0..8 {
                let system = ActorSystem::new().unwrap();
                let group = system.thread_group("placement-failure", 1).unwrap();
                let occupied = CoordinatorWorker.with_options(&group).spawn().unwrap();
                rt::timeout(Duration::from_secs(10), occupied.ready())
                    .await
                    .unwrap()
                    .unwrap();
                let mut tree = system.supervisor(SupervisorConfig::default()).unwrap();
                let first = tree
                    .add(ChildSpec::new("first", &group, |_| async {
                        Ok::<_, String>(CoordinatorWorker)
                    }))
                    .unwrap();
                let second = tree
                    .add(ChildSpec::new("second", &group, |_| async {
                        Ok::<_, String>(CoordinatorWorker)
                    }))
                    .unwrap();
                let core = tree.core.clone();
                assert!(tree.start().await.is_err());
                assert!(rt::timeout(
                    Duration::from_secs(10),
                    system::wait_true(core.done.subscribe())
                )
                .await
                .unwrap());
                for child in [first, second] {
                    assert_eq!(child.snapshot().generation, 0);
                    assert_eq!(child.snapshot().state, ChildState::Failed);
                    assert!(matches!(
                        child.current(),
                        Err(ChildUnavailable(ChildState::Failed))
                    ));
                }
                assert!(system.shutdown(Duration::from_secs(10)).await.complete);
            }
        },
    )
    .await;
}
struct PanicRunner;
impl Runner for PanicRunner {
    fn run(self: Box<Self>, _: Arc<Core>) -> BoxFuture<'static, ()> {
        panic!("injected supervisor coordinator fault");
    }
}
struct CoordinatorWorker;
impl Actor for CoordinatorWorker {}
#[tokio::test]
async fn coordinator_fault_joins_launched_controllers_and_helpers() {
    crate::test_wait::within(concat!(module_path!(), "::coordinator_fault_joins_launched_controllers_and_helpers"), async move {
        let system = ActorSystem::new().unwrap();
        let group = system
            .async_group("supervisor-coordinator-fault", 1)
            .unwrap();
        let mut tree = system.supervisor(SupervisorConfig::default()).unwrap();
        let (release, gate) = std::sync::mpsc::channel();
        let gate = Arc::new(Mutex::new(Some(gate)));
        tree.add(
            ChildSpec::new("live", &group, move |ctx| {
                let gate = system::lock(&gate).take().unwrap();
                async move {
                    ctx.spawn_blocking_helper(move || {
                        gate.recv_timeout(Duration::from_secs(10)).unwrap();
                    })
                    .unwrap();
                    Ok::<_, String>(CoordinatorWorker)
                }
            })
            .shutdown_timeout(Duration::from_secs(10)),
        )
        .unwrap();
        tree.add(ChildSpec::new("not-launched", &group, |_| async {
            Ok::<_, String>(CoordinatorWorker)
        }))
        .unwrap();
        tree.runners[1] = Box::new(PanicRunner);
        let core = tree.core.clone();
        // v0.8 (S2): start() returns its error only after the rollback, which
        // waits for the live child's owned helper.
        let start = rt::spawn(tree.start());
        assert!(rt::timeout(
            Duration::from_millis(30),
            system::wait_true(core.done.subscribe())
        )
        .await
        .is_err());
        release.send(()).unwrap();
        assert!(
            matches!(start.await.unwrap(), Err(SupervisorError::Failed(e)) if e.contains("injected supervisor coordinator fault"))
        );
        assert!(rt::timeout(
            Duration::from_secs(10),
            system::wait_true(core.done.subscribe())
        )
        .await
        .unwrap());
        assert!(system::lock(&core.children)
            .iter()
            .all(|child| *child.done().borrow()));
        let report = system.shutdown(Duration::from_secs(10)).await;
        assert!(report.complete);
        assert!(report
            .supervisor_failures
            .iter()
            .any(|e| e.contains("injected supervisor coordinator fault")));
    }).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_wins_over_late_child_failure_without_trace_serialization() {
    crate::test_wait::within(
        concat!(
            module_path!(),
            "::stop_wins_over_late_child_failure_without_trace_serialization"
        ),
        async move {
            let system = ActorSystem::new().unwrap();
            let tree = system.supervisor(SupervisorConfig::default()).unwrap();
            tree.core.close();
            assert!(!tree
                .core
                .fail_if_open_as("late child failure".into(), || None));
            assert!(system::lock(&tree.core.failure).is_none());
            assert!(tree
                .core
                .gate
                .if_open(|| tree.core.ready.send_replace(true))
                .is_none());
            assert!(!*tree.core.ready.borrow());
            // Cleanup failures remain observable after ordinary admission closure.
            tree.core.fail_cleanup("late cleanup failure".into());
            assert_eq!(
                system::lock(&tree.core.failure).as_deref(),
                Some("late cleanup failure")
            );
            drop(tree);
            assert!(system.shutdown(Duration::from_secs(5)).await.complete);
        },
    )
    .await;
}
#[test]
fn delayed_failure_notifications_are_absorbed_by_the_already_assigned_round() {
    crate::test_wait::bounded(
        concat!(
            module_path!(),
            "::delayed_failure_notifications_are_absorbed_by_the_already_assigned_round"
        ),
        move || {
            let mut queue = RecoveryQueue {
                pending: vec![false; 3],
                covered: vec![0; 3],
                next_id: 1,
            };
            assert!(queue.request(1, 0));
            queue.pending.fill(false);
            // The coordinator publishes coverage before per-child assignment.
            queue.covered[1..].fill(1);
            assert!(!queue.request(2, 0));
            assert!(!queue.pending.iter().any(|value| *value));
            // A new failure in a reconstructed generation must still be admitted.
            assert!(queue.request(2, 1));
            assert!(queue.request(0, 0)); // unaffected upstream in RestForOne
        },
    );
}

#[tokio::test]
async fn admission_closure_after_readiness_preserves_fixed_system_order() {
    crate::test_wait::within(
        concat!(
            module_path!(),
            "::admission_closure_after_readiness_preserves_fixed_system_order"
        ),
        async move {
            use crate::tasks::ActorStart;
            struct Ordered {
                id: u32,
                log: Arc<Mutex<Vec<u32>>>,
                entered: Option<rt::oneshot::Sender<()>>,
                release: Option<rt::oneshot::Receiver<()>>,
            }
            impl Actor for Ordered {
                async fn started(&mut self, ctx: &crate::tasks::Context<Self>) {
                    // v0.8: stopped() records the order only for a trapping
                    // child (E3, decision 2).
                    ctx.trap_exit(true);
                }
                async fn stopped(&mut self, _: &crate::tasks::Context<Self>) {
                    system::lock(&self.log).push(self.id);
                    if let Some(entered) = self.entered.take() {
                        let _ = entered.send(());
                    }
                    if let Some(release) = self.release.take() {
                        let _ = release.await;
                    }
                }
            }
            let limit = Duration::from_secs(30);
            let system = ActorSystem::new().unwrap();
            let group = system.thread_group("fixed-order-2-1-2", 5).unwrap();
            let log = Arc::new(Mutex::new(Vec::new()));
            for id in 0..2 {
                Ordered {
                    id,
                    log: log.clone(),
                    entered: None,
                    release: None,
                }
                .with_options(&group)
                .spawn()
                .unwrap()
                .ready()
                .await
                .unwrap();
            }
            let mut builder = system.supervisor(SupervisorConfig::default()).unwrap();
            let child_log = log.clone();
            builder
                .add(ChildSpec::new("middle", &group, move |_| {
                    let log = child_log.clone();
                    async move {
                        Ok::<_, String>(Ordered {
                            id: 100,
                            log,
                            entered: None,
                            release: None,
                        })
                    }
                }))
                .unwrap();
            let resume = CancellationToken::new();
            let (passed, after_check) = watch::channel(false);
            *system::lock(&builder.core.initial_ready_pause) = Some((resume.clone(), passed));
            let tree = builder.start().await.unwrap();
            Ordered {
                id: 200,
                log: log.clone(),
                entered: None,
                release: None,
            }
            .with_options(&group)
            .spawn()
            .unwrap()
            .ready()
            .await
            .unwrap();
            let (entered, arrival) = rt::oneshot::channel();
            let (release, gate) = rt::oneshot::channel();
            Ordered {
                id: 201,
                log: log.clone(),
                entered: Some(entered),
                release: Some(gate),
            }
            .with_options(&group)
            .spawn()
            .unwrap()
            .ready()
            .await
            .unwrap();
            let shutdown = system.shutdown(limit);
            tokio::pin!(shutdown);
            assert!(futures::poll!(&mut shutdown).is_pending());
            rt::timeout(limit, arrival).await.unwrap().unwrap();
            // The later standalone actor owns the current stop turn. Resume the
            // supervisor exactly between readiness and its cancellation wait.
            resume.cancel();
            assert!(rt::timeout(limit, system::wait_true(after_check))
                .await
                .unwrap());
            assert!(
                !tree.core.cancel.is_cancelled(),
                "admission closure stole the supervisor's stop turn"
            );
            release.send(()).unwrap();
            assert!(rt::timeout(limit, shutdown).await.unwrap().complete);
            assert_eq!(*system::lock(&log), vec![201, 200, 100, 1, 0]);
        },
    )
    .await;
}

#[tokio::test]
async fn startup_completion_or_drop_after_readiness_preserves_system_order() {
    crate::test_wait::within(
        concat!(
            module_path!(),
            "::startup_completion_or_drop_after_readiness_preserves_system_order"
        ),
        async move {
            use crate::tasks::ActorStart;
            struct Ordered {
                id: u32,
                log: Arc<Mutex<Vec<u32>>>,
                entered: Option<rt::oneshot::Sender<()>>,
                release: Option<rt::oneshot::Receiver<()>>,
            }
            impl Actor for Ordered {
                async fn started(&mut self, ctx: &crate::tasks::Context<Self>) {
                    // v0.8: stopped() records the order only for a trapping
                    // child (E3, decision 2).
                    ctx.trap_exit(true);
                }
                async fn stopped(&mut self, _: &crate::tasks::Context<Self>) {
                    system::lock(&self.log).push(self.id);
                    if let Some(entered) = self.entered.take() {
                        let _ = entered.send(());
                    }
                    if let Some(release) = self.release.take() {
                        let _ = release.await;
                    }
                }
            }
            for complete_start in [true, false] {
                let limit = Duration::from_secs(30);
                let system = ActorSystem::new().unwrap();
                let group = system.thread_group("fixed-order-2-1-2", 5).unwrap();
                let log = Arc::new(Mutex::new(Vec::new()));
                for id in 0..2 {
                    Ordered {
                        id,
                        log: log.clone(),
                        entered: None,
                        release: None,
                    }
                    .with_options(&group)
                    .spawn()
                    .unwrap()
                    .ready()
                    .await
                    .unwrap();
                }
                let mut builder = system.supervisor(SupervisorConfig::default()).unwrap();
                let child_log = log.clone();
                // Hold construction until start() has been polled once, so the
                // first poll is pending however fast the controller runs.
                let (construct, gate) = rt::oneshot::channel::<()>();
                let gate = Arc::new(Mutex::new(Some(gate)));
                builder
                    .add(ChildSpec::new("middle", &group, move |_| {
                        let log = child_log.clone();
                        let gate = system::lock(&gate).take();
                        async move {
                            if let Some(gate) = gate {
                                let _ = gate.await;
                            }
                            Ok::<_, String>(Ordered {
                                id: 100,
                                log,
                                entered: None,
                                release: None,
                            })
                        }
                    }))
                    .unwrap();
                let core = builder.core.clone();
                let mut startup = Box::pin(builder.start());
                assert!(futures::poll!(startup.as_mut()).is_pending());
                construct.send(()).unwrap();
                assert!(
                    rt::timeout(limit, system::wait_true(core.ready.subscribe()))
                        .await
                        .unwrap()
                );
                Ordered {
                    id: 200,
                    log: log.clone(),
                    entered: None,
                    release: None,
                }
                .with_options(&group)
                .spawn()
                .unwrap()
                .ready()
                .await
                .unwrap();
                let (entered, arrival) = rt::oneshot::channel();
                let (release, gate) = rt::oneshot::channel();
                Ordered {
                    id: 201,
                    log: log.clone(),
                    entered: Some(entered),
                    release: Some(gate),
                }
                .with_options(&group)
                .spawn()
                .unwrap()
                .ready()
                .await
                .unwrap();
                let shutdown = system.shutdown(limit);
                tokio::pin!(shutdown);
                assert!(futures::poll!(&mut shutdown).is_pending());
                rt::timeout(limit, arrival).await.unwrap().unwrap();
                // Readiness committed, but the caller has not observed start's result.
                // Admission now closes while the later actor still owns the stop turn.
                if complete_start {
                    assert!(rt::timeout(limit, startup.as_mut()).await.unwrap().is_ok());
                } else {
                    drop(startup);
                }
                assert!(
                    !core.cancel.is_cancelled(),
                    "completed readiness was rolled back"
                );
                release.send(()).unwrap();
                assert!(rt::timeout(limit, shutdown).await.unwrap().complete);
                assert_eq!(*system::lock(&log), vec![201, 200, 100, 1, 0]);
            }
        },
    )
    .await;
}

#[tokio::test]
async fn startup_success_remains_historical_after_unobserved_ready_then_failure() {
    crate::test_wait::within(
        concat!(
            module_path!(),
            "::startup_success_remains_historical_after_unobserved_ready_then_failure"
        ),
        async move {
            struct Idle;
            impl Actor for Idle {}
            let limit = Duration::from_secs(30);
            let system = ActorSystem::new().unwrap();
            let group = system.async_group("historical-ready-failure", 1).unwrap();
            let mut builder = system
                .supervisor(SupervisorConfig {
                    max_restarts: 0,
                    ..SupervisorConfig::default()
                })
                .unwrap();
            // Hold construction until start() has been polled once, so the
            // first poll is pending however fast the controller runs.
            let (construct, gate) = rt::oneshot::channel::<()>();
            let gate = Arc::new(Mutex::new(Some(gate)));
            let child = builder
                .add(ChildSpec::new("ready-then-failed", &group, move |_| {
                    let gate = system::lock(&gate).take();
                    async move {
                        if let Some(gate) = gate {
                            let _ = gate.await;
                        }
                        Ok::<_, String>(Idle)
                    }
                }))
                .unwrap();
            let core = builder.core.clone();
            let mut startup = Box::pin(builder.start());
            assert!(futures::poll!(startup.as_mut()).is_pending());
            construct.send(()).unwrap();
            assert!(
                rt::timeout(limit, system::wait_true(core.ready.subscribe()))
                    .await
                    .unwrap()
            );
            child
                .current()
                .unwrap()
                .context()
                .stop_with_reason(ExitReason::Kill);
            assert!(rt::timeout(limit, system::wait_true(core.done.subscribe()))
                .await
                .unwrap());
            let tree = rt::timeout(limit, startup).await.unwrap().unwrap();
            assert!(tree.failure().unwrap().contains("budget exhausted"));
            assert!(system.shutdown(limit).await.complete);
        },
    )
    .await;
}

#[tokio::test]
async fn started_failures_allow_n_restarts_and_escalate_the_next_failure() {
    crate::test_wait::within(
        concat!(
            module_path!(),
            "::started_failures_allow_n_restarts_and_escalate_the_next_failure"
        ),
        async move {
            use std::sync::atomic::{AtomicUsize, Ordering};
            // v0.8 (S1): a first start is not retried, so the first generation
            // is healthy and every replacement panics in started().
            struct FailsStarted(Arc<AtomicUsize>, bool);
            impl Actor for FailsStarted {
                async fn started(&mut self, _: &crate::tasks::Context<Self>) {
                    if self.1 {
                        self.0.fetch_add(1, Ordering::SeqCst);
                        panic!("injected started failure");
                    }
                }
            }
            for allowed in [0, 1, 3] {
                let system = ActorSystem::new().unwrap();
                let group = system.async_group("started-budget", 1).unwrap();
                let count = Arc::new(AtomicUsize::new(0));
                let attempts = count.clone();
                let mut builder = system
                    .supervisor(SupervisorConfig {
                        max_restarts: allowed,
                        backoff: Backoff {
                            initial: Duration::ZERO,
                            maximum: Duration::ZERO,
                            jitter: false,
                            ..Backoff::default()
                        },
                        ..SupervisorConfig::default()
                    })
                    .unwrap();
                let child = builder
                    .add(ChildSpec::new("startup-failure", &group, move |ctx| {
                        let attempts = attempts.clone();
                        let fail = ctx.generation() > 1;
                        async move { Ok::<_, String>(FailsStarted(attempts, fail)) }
                    }))
                    .unwrap();
                let core = builder.core.clone();
                rt::timeout(Duration::from_secs(30), builder.start())
                    .await
                    .unwrap()
                    .unwrap();
                child.current().unwrap().child_handle().kill();
                assert!(rt::timeout(
                    Duration::from_secs(30),
                    system::wait_true(core.done.subscribe())
                )
                .await
                .unwrap());
                assert_eq!(count.load(Ordering::SeqCst), allowed);
                assert_eq!(child.snapshot().generation as usize, allowed + 1);
                assert_eq!(
                    system::lock(&core.failure).as_ref().unwrap().kind,
                    FailureKind::RestartBudget
                );
                assert_eq!(child.snapshot().state, ChildState::Failed);
                assert!(system.shutdown(Duration::from_secs(30)).await.complete);
            }
        },
    )
    .await;
}

#[tokio::test]
async fn sliding_window_expires_only_old_grants_before_the_next_retry() {
    crate::test_wait::within(
        concat!(
            module_path!(),
            "::sliding_window_expires_only_old_grants_before_the_next_retry"
        ),
        async move {
            let system = ActorSystem::new().unwrap();
            let group = system.async_group("sliding-budget", 1).unwrap();
            let window = Duration::from_secs(3600);
            let mut builder = system
                .supervisor(SupervisorConfig {
                    max_restarts: 2,
                    restart_window: window,
                    backoff: Backoff {
                        initial: Duration::ZERO,
                        maximum: Duration::ZERO,
                        jitter: false,
                        ..Backoff::default()
                    },
                    ..SupervisorConfig::default()
                })
                .unwrap();
            let child = builder
                .add(ChildSpec::new("worker", &group, |_| async {
                    Ok::<_, String>(CoordinatorWorker)
                }))
                .unwrap();
            let core = builder.core.clone();
            let tree = builder.start().await.unwrap();
            let mut actor = child.current().unwrap();
            for generation in 1..=2 {
                actor
                    .child_handle()
                    .stop_with_reason(ExitReason::Panic("retry".into()));
                actor = rt::timeout(
                    Duration::from_secs(30),
                    child.wait_for_generation(generation),
                )
                .await
                .unwrap()
                .unwrap();
            }
            {
                let mut budget = system::lock(&core.budget);
                assert_eq!(budget.attempts.len(), 2);
                // Control just the oldest timestamp; the real controller still owns
                // admission and retry. The second grant remains inside the window.
                budget.attempts[0] = Instant::now().checked_sub(window).unwrap();
            }
            actor
                .child_handle()
                .stop_with_reason(ExitReason::Panic("expired oldest".into()));
            let fourth = rt::timeout(Duration::from_secs(30), child.wait_for_generation(3))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(system::lock(&core.budget).attempts.len(), 2);
            fourth
                .child_handle()
                .stop_with_reason(ExitReason::Panic("now exhausted".into()));
            rt::timeout(Duration::from_secs(30), tree.join())
                .await
                .unwrap();
            assert_eq!(child.snapshot().generation, 4);
            assert_eq!(
                system::lock(&core.failure).as_ref().unwrap().kind,
                FailureKind::RestartBudget
            );
            assert!(system.shutdown(Duration::from_secs(30)).await.complete);
        },
    )
    .await;
}

/// OTP charges `add_restart` when it decides a restart, before the
/// cohort is rebuilt: a superseded round keeps its charge, and the
/// superseding round is charged once more.
#[tokio::test]
async fn supersession_charges_each_selected_round_once() {
    crate::test_wait::within(
        concat!(
            module_path!(),
            "::supersession_charges_each_selected_round_once"
        ),
        async move {
            let limit = Duration::from_secs(30);
            let system = ActorSystem::new().unwrap();
            let group = system
                .async_group("refund-superseded-admission", 2)
                .unwrap();
            let mut builder = system
                .supervisor(SupervisorConfig {
                    strategy: SupervisorStrategy::RestForOne,
                    max_restarts: 3,
                    backoff: Backoff {
                        initial: Duration::ZERO,
                        maximum: Duration::ZERO,
                        jitter: false,
                        ..Backoff::default()
                    },
                    ..SupervisorConfig::default()
                })
                .unwrap();
            let mut children = Vec::new();
            for name in ["upstream", "middle", "tail"] {
                children.push(
                    builder
                        .add(ChildSpec::new(name, &group, |_| async {
                            Ok::<_, String>(CoordinatorWorker)
                        }))
                        .unwrap(),
                );
            }
            let core = builder.core.clone();
            let tree = builder.start().await.unwrap();
            let resume = CancellationToken::new();
            let (entered, arrival) = watch::channel(false);
            *system::lock(&core.retry_admission_pause) = Some((resume.clone(), entered));
            children[1]
                .current()
                .unwrap()
                .child_handle()
                .stop_with_reason(ExitReason::Panic("middle fails".into()));
            assert!(rt::timeout(limit, system::wait_true(arrival))
                .await
                .unwrap());
            let old_round = system::lock(&children[1].view.data)
                .recovery
                .as_ref()
                .unwrap()
                .meta
                .id;
            {
                let budget = system::lock(&core.budget);
                // The round was charged once, at selection; its rebuilds hold
                // no reservation of their own.
                assert_eq!(budget.attempts.len(), 1);
                assert_eq!(budget.reserved, 0);
            }
            children[0]
                .current()
                .unwrap()
                .child_handle()
                .stop_with_reason(ExitReason::Panic("upstream supersedes".into()));
            let _new_round = rt::timeout(limit, children[1].view.next_recovery(old_round))
                .await
                .unwrap();
            resume.cancel();
            for child in &children {
                rt::timeout(limit, child.wait_for_generation(1))
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(child.snapshot().generation, 2);
            }
            assert!(tree.failure().is_none());
            {
                let budget = system::lock(&core.budget);
                // Two restart actions: the superseded round and its successor,
                // however many children each rebuilt.
                assert_eq!(budget.attempts.len(), 2);
                assert_eq!(budget.reserved, 0);
            }
            assert!(system.shutdown(limit).await.complete);
        },
    )
    .await;
}

#[tokio::test]
async fn cohort_round_takes_one_grant_for_all_rebuilt_children() {
    crate::test_wait::within(
        concat!(
            module_path!(),
            "::cohort_round_takes_one_grant_for_all_rebuilt_children"
        ),
        async move {
            let limit = Duration::from_secs(30);
            let system = ActorSystem::new().unwrap();
            let group = system.async_group("cohort-budget-contract", 2).unwrap();
            let mut builder = system
                .supervisor(SupervisorConfig {
                    strategy: SupervisorStrategy::OneForAll,
                    max_restarts: 1,
                    backoff: Backoff {
                        initial: Duration::ZERO,
                        maximum: Duration::ZERO,
                        jitter: false,
                        ..Backoff::default()
                    },
                    ..SupervisorConfig::default()
                })
                .unwrap();
            let mut children = Vec::new();
            for index in 0..6 {
                children.push(
                    builder
                        .add(ChildSpec::new(
                            format!("worker-{index}"),
                            &group,
                            |_| async { Ok::<_, String>(CoordinatorWorker) },
                        ))
                        .unwrap(),
                );
            }
            let core = builder.core.clone();
            let tree = builder.start().await.unwrap();
            children[5]
                .current()
                .unwrap()
                .child_handle()
                .stop_with_reason(ExitReason::Panic("last child fails".into()));
            // One grant rebuilds all six children (one restart action).
            for child in &children {
                rt::timeout(limit, child.wait_for_generation(1))
                    .await
                    .unwrap()
                    .unwrap();
            }
            assert!(tree.failure().is_none());
            assert_eq!(system::lock(&core.budget).attempts.len(), 1);
            children[5]
                .current()
                .unwrap()
                .child_handle()
                .stop_with_reason(ExitReason::Panic("second failure".into()));
            rt::timeout(limit, tree.join()).await.unwrap();
            assert!(tree
                .failure()
                .unwrap()
                .contains("budget exhausted by worker-5"));
            assert_eq!(
                children
                    .iter()
                    .map(|child| child.snapshot().generation)
                    .collect::<Vec<_>>(),
                vec![2; 6]
            );
            assert!(system.shutdown(limit).await.complete);
        },
    )
    .await;
}

#[tokio::test]
async fn retry_reservations_exclude_siblings_and_refund_interrupted_backoff() {
    crate::test_wait::within(
        concat!(
            module_path!(),
            "::retry_reservations_exclude_siblings_and_refund_interrupted_backoff"
        ),
        async move {
            let system = ActorSystem::new().unwrap();
            let tree = system
                .supervisor(SupervisorConfig {
                    max_restarts: 1,
                    ..SupervisorConfig::default()
                })
                .unwrap();
            let reservation = tree.core.reserve_retry(1).unwrap();
            assert!(tree.core.reserve_retry(2).is_none());
            assert_eq!(system::lock(&tree.core.budget).attempts.len(), 0);
            drop(reservation);
            let mut replacement = tree.core.reserve_retry(2).unwrap();
            tree.core.close();
            assert!(!trace::atomic(|| tree.core.retry(&mut replacement)));
            drop(replacement);
            {
                let budget = system::lock(&tree.core.budget);
                assert!(budget.attempts.is_empty());
                assert_eq!(budget.reserved, 0);
            }
            drop(tree);
            assert!(system.shutdown(Duration::from_secs(10)).await.complete);
        },
    )
    .await;
}

#[test]
fn retry_budget_reservations_count_against_availability() {
    // `available` (the reservation check) must agree with `take` at the
    // limit: `< max` became `<= max` without any test failing.
    let now = Instant::now();
    let mut budget = RetryBudget {
        attempts: VecDeque::from([now]),
        reserved: 0,
        max: 2,
        window: Duration::from_secs(10),
    };
    assert!(budget.available(now));
    budget.reserved = 1;
    assert!(!budget.available(now));
    budget.reserved = 0;
    budget.attempts.push_back(now);
    assert!(!budget.available(now));
    assert!(budget.available(now + Duration::from_secs(10)));
}
#[test]
fn retry_window_releases_capacity_at_boundary_and_does_not_bank_unused_retries() {
    let now = Instant::now();
    let mut budget = RetryBudget {
        attempts: VecDeque::new(),
        reserved: 0,
        max: 2,
        window: Duration::from_secs(10),
    };
    assert!(budget.take(now));
    assert!(budget.take(now + Duration::from_secs(1)));
    assert!(!budget.take(now + Duration::from_secs(9)));
    assert!(budget.take(now + Duration::from_secs(10)));
    assert!(!budget.take(now + Duration::from_secs(10)));
    assert!(budget.take(now + Duration::from_secs(100)));
    assert!(budget.take(now + Duration::from_secs(100)));
    assert!(!budget.take(now + Duration::from_secs(100)));
}
#[test]
fn capped_backoff_and_seeded_jitter_remain_bounded_after_long_failure_streaks() {
    let mut policy = Backoff {
        initial: Duration::from_millis(10),
        maximum: Duration::from_millis(35),
        jitter: false,
        ..Backoff::default()
    };
    let mut seed = 7;
    assert_eq!(
        [0, 1, 2, 32, u32::MAX].map(|n| policy.delay(n, &mut seed)),
        [10, 20, 35, 35, 35].map(Duration::from_millis)
    );
    policy.jitter = true;
    let mut a = 42;
    let mut b = 42;
    let first: Vec<_> = (0..100).map(|n| policy.delay(n, &mut a)).collect();
    let replay: Vec<_> = (0..100).map(|n| policy.delay(n, &mut b)).collect();
    assert_eq!(first, replay);
    assert!(first.iter().all(|d| *d <= policy.maximum));
    assert!(first.windows(2).any(|pair| pair[0] != pair[1]));
}
