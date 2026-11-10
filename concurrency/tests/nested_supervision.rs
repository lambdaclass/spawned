#![cfg(feature = "async")]
use spawned_concurrency::{
    message::Message,
    supervision::{
        Backoff, ChildSpec, RestartMode, SupervisedChild, SupervisorConfig, SupervisorError,
        SupervisorSpec,
    },
    system::ActorSystem,
    tasks::{Actor, Context, Handler},
};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
#[cfg(feature = "lifecycle-trace")]
#[path = "support/tree_record.rs"]
mod tree_record;
const LIMIT: Duration = Duration::from_secs(5);
fn policy(retries: usize) -> SupervisorConfig {
    SupervisorConfig {
        max_restarts: retries,
        backoff: Backoff {
            initial: Duration::from_millis(1),
            maximum: Duration::from_millis(4),
            jitter: false,
            ..Backoff::default()
        },
        ..SupervisorConfig::default()
    }
}
struct Next;
impl Message for Next {
    type Result = usize;
}
struct Crash;
impl Message for Crash {
    type Result = ();
}
struct Counter(usize);
impl Actor for Counter {}
impl Handler<Next> for Counter {
    async fn handle(&mut self, _: Next, _: &Context<Self>) -> usize {
        self.0 += 1;
        self.0
    }
}
impl Handler<Crash> for Counter {
    async fn handle(&mut self, _: Crash, _: &Context<Self>) {
        panic!("nested leaf failed");
    }
}
type Published = Arc<Mutex<Option<SupervisedChild<Counter>>>>;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nested_budget_failure_reconstructs_fresh_subtree_and_preserves_parent_sibling() {
    let system = ActorSystem::new().unwrap();
    let controls = system.async_group("tree-controls", 1).unwrap();
    let workers = system.async_group("tree-workers", 1).unwrap();
    let published: Published = Arc::new(Mutex::new(None));
    let expose = published.clone();
    let placement = workers.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let factories = calls.clone();
    let mut root = system.supervisor(policy(1)).unwrap();
    let subtree = root
        .add_supervisor(
            SupervisorSpec::new("subtree", &controls, policy(0), move |tree| {
                factories.fetch_add(1, Ordering::SeqCst);
                let leaf = tree.add(ChildSpec::new("leaf", &placement, |_| async {
                    Ok::<_, String>(Counter(0))
                }))?;
                *expose.lock().unwrap() = Some(leaf);
                Ok::<_, SupervisorError>(())
            })
            .restart(RestartMode::Transient),
        )
        .unwrap();
    let sibling = root
        .add(ChildSpec::new("healthy", &workers, |_| async {
            Ok::<_, String>(Counter(0))
        }))
        .unwrap();
    let supervisor = root.start().await.unwrap();
    let old_tree = subtree.current().unwrap();
    let old_slot = subtree.snapshot();
    let old_leaf = published
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .current()
        .unwrap();
    let sibling_id = sibling.current().unwrap().id();
    assert_eq!(old_leaf.request(Next).await.unwrap(), 1);
    assert!(old_leaf.request(Crash).await.is_err());
    let replacement = tokio::time::timeout(LIMIT, subtree.wait_for_generation(old_slot.generation))
        .await
        .unwrap()
        .unwrap();
    assert_ne!(subtree.snapshot().actor_id, old_slot.actor_id);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(old_tree.failure().unwrap().contains("budget exhausted"));
    old_tree.join().await;
    let new_leaf = published
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .current()
        .unwrap();
    assert_ne!(old_leaf.id(), new_leaf.id());
    assert!(old_leaf.request(Next).await.is_err());
    assert_eq!(new_leaf.request(Next).await.unwrap(), 1);
    assert_eq!(sibling.current().unwrap().id(), sibling_id);
    assert_eq!(replacement.snapshots().len(), 1);
    // The parent allowed exactly one subtree replacement. A second exhausted
    // leaf escalates through that parent budget, rather than constructing again.
    assert!(new_leaf.request(Crash).await.is_err());
    tokio::time::timeout(LIMIT, supervisor.join())
        .await
        .unwrap();
    assert!(supervisor.failure().unwrap().contains("budget exhausted"));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(subtree.snapshot().generation, 2);
    assert!(supervisor.shutdown(LIMIT).await.complete);
    assert!(system.shutdown(LIMIT).await.complete);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_nested_startup_is_cleaned_before_parent_retries() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("nested-startup", 1).unwrap();
    let placement = group.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let attempts = calls.clone();
    let mut root = system.supervisor(policy(5)).unwrap();
    let nested = root
        .add_supervisor(SupervisorSpec::new(
            "nested",
            &group,
            policy(0),
            move |tree| {
                let attempt = attempts.fetch_add(1, Ordering::SeqCst);
                tree.add(ChildSpec::new(
                    "fallible",
                    &placement,
                    move |_| async move {
                        if attempt == 0 {
                            Err("nested initial construction failed".to_string())
                        } else {
                            Ok(Counter(0))
                        }
                    },
                ))?;
                Ok::<_, SupervisorError>(())
            },
        ))
        .unwrap();
    let supervisor = tokio::time::timeout(LIMIT, root.start())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(nested.snapshot().generation, 2);
    assert!(nested.current().is_ok());
    assert!(supervisor.shutdown(LIMIT).await.complete);
    assert!(system.shutdown(LIMIT).await.complete);
}

struct HoldStart {
    entered: Option<tokio::sync::oneshot::Sender<()>>,
    release: Option<tokio::sync::oneshot::Receiver<()>>,
}
impl Actor for HoldStart {
    async fn started(&mut self, _: &Context<Self>) {
        let _ = self.entered.take().unwrap().send(());
        let _ = self.release.take().unwrap().await;
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_during_nested_startup_retains_parent_scope_until_descendants_finish() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("nested-cancel", 1).unwrap();
    let placement = group.clone();
    let (entered_tx, entered) = tokio::sync::oneshot::channel();
    let (release, release_rx) = tokio::sync::oneshot::channel();
    let args = Arc::new(Mutex::new(Some((entered_tx, release_rx))));
    let mut root = system.supervisor(policy(5)).unwrap();
    root.add_supervisor(SupervisorSpec::new(
        "nested",
        &group,
        policy(0),
        move |tree| {
            let args = args.clone();
            tree.add(ChildSpec::new("held", &placement, move |_| {
                let (entered, release) = args.lock().unwrap().take().unwrap();
                async move {
                    Ok::<_, String>(HoldStart {
                        entered: Some(entered),
                        release: Some(release),
                    })
                }
            }))?;
            Ok::<_, SupervisorError>(())
        },
    ))
    .unwrap();
    let startup = tokio::spawn(root.start());
    tokio::time::timeout(LIMIT, entered).await.unwrap().unwrap();
    startup.abort();
    assert!(startup.await.is_err());
    let mut report = system.shutdown(Duration::from_millis(20)).await;
    assert!(!report.complete);
    for _ in 0..20 {
        if !report.exits.is_empty() {
            break;
        }
        report = report.wait(Duration::from_millis(5)).await;
    }
    assert!(
        !report.exits.is_empty(),
        "nested adapter should exit while retaining its cleanup lease"
    );
    assert!(
        report
            .exits
            .iter()
            .any(|exit| report.unfinished_actors.contains(&exit.id)),
        "parent cleanup ownership was dropped before nested startup finished"
    );
    release.send(()).unwrap();
    assert!(report.wait(LIMIT).await.complete);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nested_supervisors_can_form_multiple_levels_and_stop_from_the_root() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("three-level-tree", 1).unwrap();
    let middle_group = group.clone();
    let leaf_group = group.clone();
    let mut root = system.supervisor(policy(5)).unwrap();
    let middle = root
        .add_supervisor(SupervisorSpec::new(
            "middle",
            &group,
            policy(5),
            move |tree| {
                let leaf_group = leaf_group.clone();
                tree.add_supervisor(SupervisorSpec::new(
                    "inner",
                    &middle_group,
                    policy(5),
                    move |inner| {
                        inner.add(ChildSpec::new("leaf", &leaf_group, |_| async {
                            Ok::<_, String>(Counter(0))
                        }))?;
                        Ok::<_, SupervisorError>(())
                    },
                ))?;
                Ok::<_, SupervisorError>(())
            },
        ))
        .unwrap();
    let supervisor = tokio::time::timeout(LIMIT, root.start())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(middle.current().unwrap().snapshots()[0].name, "inner");
    assert!(supervisor.shutdown(LIMIT).await.complete);
    assert!(middle.current().is_err());
    assert!(system.shutdown(LIMIT).await.complete);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_leaf_helper_blocks_subtree_replacement_and_remains_owned_after_adapter_exit() {
    let system = ActorSystem::new().unwrap();
    let controls = system.async_group("nested-helper-controls", 1).unwrap();
    let workers = system.thread_group("nested-native-leaf", 1).unwrap();
    let published: Published = Arc::new(Mutex::new(None));
    let exposed = published.clone();
    let placement = workers.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let mut root = system.supervisor(policy(5)).unwrap();
    let subtree = root
        .add_supervisor(
            SupervisorSpec::new("nested-native", &controls, policy(0), move |tree| {
                count.fetch_add(1, Ordering::SeqCst);
                *exposed.lock().unwrap() = Some(
                    tree.add(
                        ChildSpec::new("leaf", &placement, |_| async {
                            Ok::<_, String>(Counter(0))
                        })
                        .shutdown_timeout(Duration::from_millis(15)),
                    )?,
                );
                Ok::<_, SupervisorError>(())
            })
            .shutdown_timeout(Duration::from_millis(20)),
        )
        .unwrap();
    let supervisor = root.start().await.unwrap();
    let leaf = published
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .current()
        .unwrap();
    let (entered_tx, entered) = tokio::sync::oneshot::channel();
    let (release, release_rx) = std::sync::mpsc::channel();
    let helper = leaf
        .context()
        .spawn_blocking_helper(move || {
            let _ = entered_tx.send(());
            release_rx.recv_timeout(LIMIT).unwrap();
        })
        .unwrap();
    tokio::time::timeout(LIMIT, entered).await.unwrap().unwrap();
    assert!(leaf.request(Crash).await.is_err());
    tokio::time::timeout(LIMIT, async {
        while supervisor.failure().is_none() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(subtree.current().is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let report = system.shutdown(Duration::from_millis(5)).await;
    assert!(!report.complete);
    release.send(()).unwrap();
    drop(helper);
    assert!(report.wait(LIMIT).await.complete);
    #[cfg(feature = "lifecycle-trace")]
    tree_record::record("nested-stuck-native-helper", &supervisor);
}

struct HeldStop(Arc<tokio::sync::Notify>);
impl Actor for HeldStop {
    async fn stopped(&mut self, _: &Context<Self>) {
        self.0.notified().await;
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn late_nested_cleanup_failures_are_retained_in_parent_and_system_reports() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("nested-late-cleanup", 2).unwrap();
    let placement = group.clone();
    let release = Arc::new(tokio::sync::Notify::new());
    let released = release.clone();
    let mut root = system.supervisor(policy(5)).unwrap();
    let nested = root
        .add_supervisor(
            SupervisorSpec::new("late-failure", &group, policy(5), move |tree| {
                let released = released.clone();
                tree.add(
                    ChildSpec::new("held-stop", &placement, move |_| {
                        let released = released.clone();
                        async move { Ok::<_, String>(HeldStop(released)) }
                    })
                    .shutdown_timeout(Duration::from_millis(10)),
                )?;
                Ok::<_, SupervisorError>(())
            })
            .shutdown_timeout(Duration::from_secs(2)),
        )
        .unwrap();
    let supervisor = root.start().await.unwrap();
    let inner = nested.current().unwrap();
    supervisor.stop();
    tokio::time::timeout(LIMIT, async {
        while inner.failure().is_none() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    release.notify_one();
    let report = supervisor.shutdown(LIMIT).await;
    assert!(report.complete);
    assert!(report
        .failure
        .unwrap()
        .contains("nested supervisor cleanup failed"));
    let report = system.shutdown(LIMIT).await;
    assert!(report.complete);
    assert!(report
        .supervisor_failures
        .iter()
        .any(|failure| failure.contains("nested supervisor cleanup failed")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aborted_initial_startup_rolls_back_before_a_later_stuck_shutdown_target() {
    let system = Arc::new(ActorSystem::new().unwrap());
    let group = system.async_group("startup-admission-close", 2).unwrap();
    let entered = Arc::new(tokio::sync::Notify::new());
    let seen = entered.clone();
    let mut root = system.supervisor(policy(5)).unwrap();
    root.add(ChildSpec::new("never-constructed", &group, move |_| {
        let seen = seen.clone();
        async move {
            seen.notify_one();
            std::future::pending::<()>().await;
            Ok::<_, String>(Counter(0))
        }
    }))
    .unwrap();
    let startup = tokio::spawn(root.start());
    tokio::time::timeout(LIMIT, entered.notified())
        .await
        .unwrap();
    let release = Arc::new(tokio::sync::Notify::new());
    use spawned_concurrency::tasks::ActorStart;
    let later = HeldStop(release.clone()).start_on(&group).unwrap();
    later.ready().await.unwrap();
    let owner = system.clone();
    let shutdown = tokio::spawn(async move { owner.shutdown(Duration::from_secs(30)).await });
    // The first shutdown grace is deliberately much longer than this bound.
    let result = tokio::time::timeout(LIMIT, startup).await.unwrap().unwrap();
    assert!(matches!(result, Err(SupervisorError::Stopped)));
    release.notify_one();
    assert!(
        tokio::time::timeout(LIMIT, shutdown)
            .await
            .unwrap()
            .unwrap()
            .complete
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wait_ready_resolves_the_running_subtree() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("nested-ready", 1).unwrap();
    let placement = group.clone();
    let mut root = system.supervisor(policy(0)).unwrap();
    let subtree = root
        .add_supervisor(SupervisorSpec::new(
            "subtree",
            &group,
            policy(0),
            move |tree| {
                tree.add(ChildSpec::new("leaf", &placement, |_| async {
                    Ok::<_, String>(Counter(0))
                }))?;
                Ok::<_, SupervisorError>(())
            },
        ))
        .unwrap();
    let supervisor = root.start().await.unwrap();
    let ready = tokio::time::timeout(LIMIT, subtree.wait_ready())
        .await
        .unwrap()
        .unwrap();
    let leaf = &ready.snapshots()[0];
    assert_eq!(leaf.name, "leaf");
    assert_eq!(
        leaf.actor_id,
        subtree.current().unwrap().snapshots()[0].actor_id
    );
    assert!(supervisor.shutdown(LIMIT).await.complete);
    assert!(system.shutdown(LIMIT).await.complete);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subtree_startup_timeout_bounds_a_hung_child_factory() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("nested-startup-timeout", 1).unwrap();
    let placement = group.clone();
    let mut root = system.supervisor(policy(0)).unwrap();
    root.add_supervisor(
        SupervisorSpec::new("subtree", &group, policy(0), move |tree| {
            // The leaf's own startup limit (default 5 s) is far above the
            // subtree's, so only the subtree limit can end this startup early.
            tree.add(ChildSpec::new("hung", &placement, |_| async {
                std::future::pending::<Result<Counter, String>>().await
            }))?;
            Ok::<_, SupervisorError>(())
        })
        .startup_timeout(Duration::from_millis(50)),
    )
    .unwrap();
    let started = tokio::time::timeout(Duration::from_secs(3), root.start())
        .await
        .expect("subtree startup_timeout did not bound the hung child");
    assert!(started.is_err());
    assert!(system.shutdown(LIMIT).await.complete);
}

/// Stops itself `Normal` before readiness when `stop` is set.
struct MaybeStop(bool);
impl Actor for MaybeStop {
    async fn started(&mut self, ctx: &Context<Self>) {
        if self.0 {
            ctx.stop();
        }
    }
}
impl Handler<Crash> for MaybeStop {
    async fn handle(&mut self, _: Crash, _: &Context<Self>) {
        panic!("nested leaf failed");
    }
}

/// A Transient leaf's failed replacement (stops before readiness) is retried
/// inside its own subtree; when the subtree's budget runs out, the subtree
/// fails and escalates, and the parent replaces it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nested_failed_replacement_retries_then_escalates_on_exhaustion() {
    for (inner_budget, escalates) in [(5, false), (1, true)] {
        let always = escalates;
        let system = ActorSystem::new().unwrap();
        let controls = system.async_group("tree-controls", 1).unwrap();
        let workers = system.async_group("tree-workers", 1).unwrap();
        let published: Arc<Mutex<Option<SupervisedChild<MaybeStop>>>> = Arc::new(Mutex::new(None));
        let expose = published.clone();
        let placement = workers.clone();
        let mut root = system.supervisor(policy(5)).unwrap();
        let subtree = root
            .add_supervisor(SupervisorSpec::new(
                "subtree",
                &controls,
                policy(inner_budget),
                move |tree| {
                    // Within one subtree instance: generation 2 stops before
                    // readiness; then generation 3 succeeds (budget 5), or
                    // every replacement stops and the second retry is refused
                    // (budget 1).
                    let leaf = tree.add(
                        ChildSpec::new("leaf", &placement, move |ctx| {
                            let generation = ctx.generation();
                            let stop = generation == 2 || (always && generation > 1);
                            async move { Ok::<_, String>(MaybeStop(stop)) }
                        })
                        .restart(RestartMode::Transient),
                    )?;
                    *expose.lock().unwrap() = Some(leaf);
                    Ok::<_, SupervisorError>(())
                },
            ))
            .unwrap();
        let supervisor = root.start().await.unwrap();
        let old_leaf = published.lock().unwrap().take().unwrap();
        assert!(old_leaf.current().unwrap().request(Crash).await.is_err());
        if escalates {
            tokio::time::timeout(LIMIT, subtree.wait_for_generation(1))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(subtree.snapshot().generation, 2);
            let new_leaf = published.lock().unwrap().take().unwrap();
            assert_eq!(new_leaf.snapshot().generation, 1);
            assert!(new_leaf.current().is_ok());
        } else {
            tokio::time::timeout(LIMIT, old_leaf.wait_for_generation(2))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(old_leaf.snapshot().generation, 3);
            assert_eq!(subtree.snapshot().generation, 1);
        }
        assert!(supervisor.failure().is_none());
        assert!(supervisor.shutdown(LIMIT).await.complete);
        assert!(system.shutdown(LIMIT).await.complete);
    }
}
