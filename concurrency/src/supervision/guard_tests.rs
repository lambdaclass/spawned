//! Deterministic tests for supervisor guards that only matter in narrow race
//! windows (they survived `cargo mutants` against the integration suites).
//! Each test drives the guard directly, or holds the tree in the window with
//! a test hook, so a mutated guard fails on every run.
use super::*;
use std::sync::atomic::AtomicBool;

const LIMIT: Duration = Duration::from_secs(30);

struct Idle;
impl Actor for Idle {}

fn round(id: u64) -> Arc<RecoverySlot> {
    RecoverySlot::new(Arc::new(RecoveryMeta {
        id,
        origin: 0,
        finished: AtomicBool::new(false),
    }))
}

fn idle_spec(name: &str, group: &ExecutionGroup) -> ChildSpec<Idle> {
    ChildSpec::new(name, group, |_| async { Ok::<_, String>(Idle) })
}

/// A controller that decided to stop retires only if no newer recovery
/// round was assigned to it meanwhile; a failed tree retires it anyway.
#[tokio::test]
async fn finish_unless_recovering_yields_only_to_a_newer_round() {
    crate::test_wait::within(
        concat!(
            module_path!(),
            "::finish_unless_recovering_yields_only_to_a_newer_round"
        ),
        async move {
            let system = ActorSystem::new().unwrap();
            let group = system.async_group("finish-guard", 1).unwrap();
            let mut tree = system.supervisor(SupervisorConfig::default()).unwrap();
            let newer = tree.add(idle_spec("newer", &group)).unwrap().view;
            let serviced = tree.add(idle_spec("serviced", &group)).unwrap().view;
            let failed = tree.add(idle_spec("failed", &group)).unwrap().view;
            for view in [&newer, &serviced, &failed] {
                system::lock(&view.data).recovery = Some(round(2));
            }

            // Round 2 is newer than the last serviced round 1: keep the
            // controller alive to service it.
            assert!(!newer.finish_unless_recovering(1, false));
            assert!(!*newer.done.borrow());
            assert_eq!(newer.snapshot().state, ChildState::Waiting);

            // Round 2 already serviced: retire as Stopped.
            assert!(serviced.finish_unless_recovering(2, false));
            assert!(*serviced.done.borrow());
            assert_eq!(serviced.snapshot().state, ChildState::Stopped);

            // A failed tree retires the child even with a newer round.
            assert!(failed.finish_unless_recovering(1, true));
            assert!(*failed.done.borrow());
            assert_eq!(failed.snapshot().state, ChildState::Failed);

            drop(tree);
            assert!(system.shutdown(LIMIT).await.complete);
        },
    )
    .await;
}

/// System shutdown closes every unit's admission first (`ShutdownUnit::
/// close_admission`) and stops units later, in order. Closing admission must
/// close the gate and wake controllers without taking the stop turn.
#[tokio::test]
async fn close_admission_closes_the_gate_without_taking_the_stop_turn() {
    crate::test_wait::within(
        concat!(
            module_path!(),
            "::close_admission_closes_the_gate_without_taking_the_stop_turn"
        ),
        async move {
            let system = ActorSystem::new().unwrap();
            let group = system.async_group("close-admission", 1).unwrap();
            let mut builder = system.supervisor(SupervisorConfig::default()).unwrap();
            let child = builder.add(idle_spec("worker", &group)).unwrap();
            let core = builder.core.clone();
            let tree = builder.start().await.unwrap();
            let actor = child.current().unwrap();
            let scope = system::lock(&child.view.data).scope.clone().unwrap();
            assert!(scope.can_start());

            ShutdownUnit::close_admission(&*core);
            assert!(!core.gate.is_open());
            assert!(core.admission_closed.is_cancelled());
            assert!(!scope.can_start(), "child scope admission open");
            assert!(
                !core.cancel.is_cancelled(),
                "admission closure took the stop turn"
            );
            assert!(!child.view.stop_turn.is_cancelled());

            // A crash now is not restarted: admission is closed.
            actor
                .child_handle()
                .stop_with_reason(ExitReason::Panic("after admission closed".into()));
            assert!(
                rt::timeout(LIMIT, system::wait_true(child.view.done.subscribe()))
                    .await
                    .unwrap()
            );
            assert_eq!(child.snapshot().generation, 1);
            assert!(tree.failure().is_none());
            assert!(!*core.done.borrow(), "the tree waits for its stop turn");
            // `Supervisor::stop` takes the stop turn at once.
            tree.stop();
            assert!(core.cancel.is_cancelled());
            rt::timeout(LIMIT, tree.join()).await.unwrap();
            assert!(system.shutdown(LIMIT).await.complete);
        },
    )
    .await;
}

/// Dropping `start()` before readiness cancels the tree; after readiness
/// commits, the tree is historical and stays up.
#[tokio::test]
async fn cancel_unready_startup_cancels_only_before_readiness() {
    crate::test_wait::within(
        concat!(
            module_path!(),
            "::cancel_unready_startup_cancels_only_before_readiness"
        ),
        async move {
            let system = ActorSystem::new().unwrap();
            let unready = system.supervisor(SupervisorConfig::default()).unwrap();
            unready.core.cancel_unready_startup();
            assert!(!unready.core.gate.is_open());
            assert!(unready.core.admission_closed.is_cancelled());
            assert!(unready.core.cancel.is_cancelled());

            let ready = system.supervisor(SupervisorConfig::default()).unwrap();
            ready.core.ready.send_replace(true);
            ready.core.cancel_unready_startup();
            assert!(ready.core.gate.is_open());
            assert!(!ready.core.admission_closed.is_cancelled());
            assert!(!ready.core.cancel.is_cancelled());

            drop((unready, ready));
            assert!(system.shutdown(LIMIT).await.complete);
        },
    )
    .await;
}

/// Past the shutdown grace, `stop_remaining` signals every child at once
/// instead of waiting for the ordered stop turns.
#[tokio::test]
async fn stop_remaining_signals_every_child_at_once() {
    crate::test_wait::within(
        concat!(
            module_path!(),
            "::stop_remaining_signals_every_child_at_once"
        ),
        async move {
            let system = ActorSystem::new().unwrap();
            let group = system.async_group("stop-remaining", 2).unwrap();
            let mut builder = system.supervisor(SupervisorConfig::default()).unwrap();
            let first = builder.add(idle_spec("first", &group)).unwrap();
            let second = builder.add(idle_spec("second", &group)).unwrap();
            let core = builder.core.clone();
            let tree = builder.start().await.unwrap();

            assert!(!ShutdownUnit::is_done(&*core));
            ShutdownUnit::stop_remaining(&*core);
            assert!(!core.gate.is_open());
            assert!(core.cancel.is_cancelled());
            assert!(first.view.stop_turn.is_cancelled());
            assert!(second.view.stop_turn.is_cancelled());

            rt::timeout(LIMIT, tree.join()).await.unwrap();
            // A finished tree is pruned from the system's shutdown units.
            assert!(ShutdownUnit::is_done(&*core));
            assert!(system.shutdown(LIMIT).await.complete);
        },
    )
    .await;
}

/// A controller task that dies outside its own panic boundary (its
/// `JoinHandle` fails) fails the tree as a coordinator failure, not only a
/// recorded cleanup failure.
#[tokio::test]
async fn escaped_controller_failure_fails_the_tree() {
    struct EscapingRunner(Arc<View<Idle>>);
    impl Runner for EscapingRunner {
        fn run(self: Box<Self>, _: Arc<Core>) -> BoxFuture<'static, ()> {
            Box::pin(async move {
                // Ready, then terminal, so neither startup nor the stop turn
                // records a failure of its own first.
                self.0.initial.send_replace(true);
                self.0.finish(false);
                panic!("controller escaped its boundary");
            })
        }
    }
    crate::test_wait::within(
        concat!(
            module_path!(),
            "::escaped_controller_failure_fails_the_tree"
        ),
        async move {
            let system = ActorSystem::new().unwrap();
            let group = system.async_group("escaped-controller", 1).unwrap();
            let mut builder = system.supervisor(SupervisorConfig::default()).unwrap();
            let child = builder.add(idle_spec("escaping", &group)).unwrap();
            builder.runners[0] = Box::new(EscapingRunner(child.view.clone()));
            let tree = builder.start().await.unwrap();
            tree.stop();
            rt::timeout(LIMIT, tree.join()).await.unwrap();
            let failure = tree.failure().expect("the tree must fail");
            assert!(
                failure.contains("child controller failed"),
                "unexpected failure: {failure}"
            );
            assert_eq!(
                system::lock(&tree.core.failure).as_ref().unwrap().kind,
                FailureKind::Coordinator
            );
            assert!(system.shutdown(LIMIT).await.complete);
        },
    )
    .await;
}

/// A stop that closes admission while a recovery round waits for a
/// collateral child's cleanup ends the round; it is not a stuck cleanup.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_during_round_cleanup_is_not_a_cleanup_failure() {
    struct SlowStop {
        entered: Option<rt::oneshot::Sender<()>>,
        release: Option<rt::oneshot::Receiver<()>>,
    }
    impl Actor for SlowStop {
        async fn started(&mut self, ctx: &crate::tasks::Context<Self>) {
            // Trap, so the round's parent exit signal runs stopped().
            ctx.trap_exit(true);
        }
        async fn stopped(&mut self, _: &crate::tasks::Context<Self>) {
            if let Some(entered) = self.entered.take() {
                let _ = entered.send(());
            }
            if let Some(release) = self.release.take() {
                let _ = release.await;
            }
        }
    }
    crate::test_wait::within(
        concat!(
            module_path!(),
            "::stop_during_round_cleanup_is_not_a_cleanup_failure"
        ),
        async move {
            let system = ActorSystem::new().unwrap();
            let group = system.async_group("stop-during-round", 2).unwrap();
            let mut builder = system
                .supervisor(SupervisorConfig::default().strategy(SupervisorStrategy::OneForAll))
                .unwrap();
            let (entered, arrival) = rt::oneshot::channel();
            let (release, gate) = rt::oneshot::channel();
            let hooks = Arc::new(Mutex::new(Some((entered, gate))));
            builder
                .add(
                    ChildSpec::new("slow", &group, move |_| {
                        let hooks = system::lock(&hooks).take();
                        async move {
                            let (entered, release) = hooks.unzip();
                            Ok::<_, String>(SlowStop { entered, release })
                        }
                    })
                    .shutdown_timeout(LIMIT),
                )
                .unwrap();
            let crashing = builder.add(idle_spec("crashing", &group)).unwrap();
            let tree = builder.start().await.unwrap();
            crashing
                .current()
                .unwrap()
                .child_handle()
                .stop_with_reason(ExitReason::Panic("starts a round".into()));
            // The round is now waiting for `slow`'s cleanup.
            rt::timeout(LIMIT, arrival).await.unwrap().unwrap();
            tree.stop();
            release.send(()).unwrap();
            rt::timeout(LIMIT, tree.join()).await.unwrap();
            assert!(tree.failure().is_none(), "{:?}", tree.failure());
            assert!(system.shutdown(LIMIT).await.complete);
        },
    )
    .await;
}

/// S2: a failed start reports a stuck cleanup as `StartupCleanupStuck`,
/// and lists the rollback's other cleanup failures without repeating the
/// cause.
#[tokio::test]
async fn start_error_separates_the_cause_from_rollback_failures() {
    crate::test_wait::within(
        concat!(
            module_path!(),
            "::start_error_separates_the_cause_from_rollback_failures"
        ),
        async move {
            let system = ActorSystem::new().unwrap();
            let origin = |child: &str| {
                Some(FailureOrigin {
                    child: child.into(),
                    generation: 1,
                    reason: None,
                    last_error: None,
                })
            };
            let tree = system.supervisor(SupervisorConfig::default()).unwrap();
            assert!(matches!(tree.core.start_error(), SupervisorError::Stopped));

            let cause = "actor cleanup stuck for slow".to_string();
            tree.core.record_cleanup_failure(&cause);
            tree.core
                .record_cleanup_failure("owned cleanup stuck for other");
            *system::lock(&tree.core.failure) = Some(Failure::new(
                FailureKind::Cleanup,
                cause.clone(),
                origin("slow"),
            ));
            match tree.core.start_error() {
                SupervisorError::StartupCleanupStuck {
                    child,
                    cause: reported,
                    rollback_failures,
                } => {
                    assert_eq!(child, "slow");
                    assert_eq!(reported, cause);
                    assert_eq!(rollback_failures, ["owned cleanup stuck for other"]);
                }
                other => panic!("unexpected {other:?}"),
            }

            let first = system.supervisor(SupervisorConfig::default()).unwrap();
            *system::lock(&first.core.failure) = Some(Failure::new(
                FailureKind::FirstStart,
                "failed to start child boot: factory failed".into(),
                origin("boot"),
            ));
            match first.core.start_error() {
                SupervisorError::FailedToStartChild {
                    child,
                    cause,
                    rollback_failures,
                } => {
                    assert_eq!(child, "boot");
                    assert_eq!(cause, "factory failed");
                    assert!(rollback_failures.is_empty());
                }
                other => panic!("unexpected {other:?}"),
            }
            drop((tree, first));
            assert!(system.shutdown(LIMIT).await.complete);
        },
    )
    .await;
}

/// E3: on the supervisor's own stop, a trapping child whose `stopped()`
/// outlives `shutdown_timeout` is killed (OTP's `brutal_kill` after the
/// shutdown timeout) before a second interval marks it stuck. The kill
/// cannot cut a running `stopped()` short, but it decides the reason.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_turn_kills_a_child_whose_cleanup_outlives_shutdown_timeout() {
    struct Hung(Option<rt::oneshot::Receiver<()>>);
    impl Actor for Hung {
        async fn started(&mut self, ctx: &crate::tasks::Context<Self>) {
            ctx.trap_exit(true);
        }
        async fn stopped(&mut self, _: &crate::tasks::Context<Self>) {
            if let Some(never) = self.0.take() {
                let _ = never.await;
            }
        }
    }
    crate::test_wait::within(
        concat!(
            module_path!(),
            "::stop_turn_kills_a_child_whose_cleanup_outlives_shutdown_timeout"
        ),
        async move {
            let system = ActorSystem::new().unwrap();
            let group = system.async_group("stop-turn-kill", 1).unwrap();
            let mut builder = system.supervisor(SupervisorConfig::default()).unwrap();
            // Held by the test until the cleanup is marked stuck.
            let (release, never) = rt::oneshot::channel::<()>();
            let never = Arc::new(Mutex::new(Some(never)));
            let child = builder
                .add(
                    ChildSpec::new("hung", &group, move |_| {
                        let never = system::lock(&never).take();
                        async move { Ok::<_, String>(Hung(never)) }
                    })
                    .shutdown_timeout(Duration::from_millis(20)),
                )
                .unwrap();
            let tree = builder.start().await.unwrap();
            let actor = child.current().unwrap().child_handle();
            tree.stop();
            // Stuck is marked only after the kill's second interval.
            let state = crate::test_wait::wait_for_async(
                || async { child.snapshot().state },
                |state| *state == ChildState::Failed,
            )
            .await;
            assert_eq!(state, ChildState::Failed, "cleanup never marked stuck");
            release.send(()).unwrap();
            let reason = rt::timeout(LIMIT, actor.wait_exit_async()).await.unwrap();
            assert_eq!(reason, ExitReason::Killed, "the stop turn did not kill");
            rt::timeout(LIMIT, tree.join()).await.unwrap();
            let _ = system.shutdown(LIMIT).await;
        },
    )
    .await;
}

/// E3 escalates to a kill only for the supervisor's own stop. A child that
/// exits by itself and whose cleanup outlives `shutdown_timeout` is stuck at
/// once: the supervisor does not kill it, so its exit reason is its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn self_exit_with_stuck_cleanup_is_not_killed() {
    struct SlowStopped(Option<rt::oneshot::Receiver<()>>);
    impl Actor for SlowStopped {
        async fn stopped(&mut self, _: &crate::tasks::Context<Self>) {
            if let Some(release) = self.0.take() {
                let _ = release.await;
            }
        }
    }
    crate::test_wait::within(
        concat!(
            module_path!(),
            "::self_exit_with_stuck_cleanup_is_not_killed"
        ),
        async move {
            let system = ActorSystem::new().unwrap();
            let group = system.async_group("self-exit-stuck", 1).unwrap();
            let mut builder = system.supervisor(SupervisorConfig::default()).unwrap();
            let (release, held) = rt::oneshot::channel();
            let held = Arc::new(Mutex::new(Some(held)));
            let child = builder
                .add(
                    ChildSpec::new("slow", &group, move |_| {
                        let held = system::lock(&held).take();
                        async move { Ok::<_, String>(SlowStopped(held)) }
                    })
                    .shutdown_timeout(Duration::from_millis(20)),
                )
                .unwrap();
            let tree = builder.start().await.unwrap();
            let actor = child.current().unwrap().child_handle();
            // Administrative stop: exits Normal through the gated stopped().
            actor.stop();
            let state = crate::test_wait::wait_for_async(
                || async { child.snapshot().state },
                |state| *state == ChildState::Failed,
            )
            .await;
            assert_eq!(state, ChildState::Failed, "cleanup never marked stuck");
            assert!(tree
                .failure()
                .is_some_and(|failure| failure.contains("actor cleanup stuck")));
            release.send(()).unwrap();
            // Normal (its own stop), or Shutdown if the failed tree's stop
            // turn signalled it first; never the escalation's Killed.
            let reason = rt::timeout(LIMIT, actor.wait_exit_async()).await.unwrap();
            assert!(
                matches!(reason, ExitReason::Normal | ExitReason::Shutdown),
                "supervisor killed a self-exiting child: {reason:?}"
            );
            rt::timeout(LIMIT, tree.join()).await.unwrap();
            assert_eq!(child.snapshot().last_exit, Some(reason));
            let _ = system.shutdown(LIMIT).await;
        },
    )
    .await;
}
