//! Supervision trees (docs/design/introspection.md, "Supervision trees";
//! I7): `which_children` / `count_children` (OTP) and `tree()`. Each row is
//! consistent with itself, counts agree with the rows of the same call, and
//! an expanded node belongs to the instance captured with its row.
#![cfg(feature = "async")]
#![cfg_attr(spawned_sim, allow(unused_imports, dead_code))]

use spawned_concurrency::introspection::{Field, Lifecycle};
use spawned_concurrency::message::Message;
use spawned_concurrency::supervision::{
    Backoff, ChildCounts, ChildKey, ChildKind, ChildSpec, ChildStart, ChildState, RestartMode,
    SupervisorConfig, SupervisorPhase, SupervisorSpec, SupervisorStrategy, TreeNode,
};
use spawned_concurrency::system::ActorSystem;
use spawned_concurrency::tasks::{Actor, Context, Handler};
use spawned_concurrency::{ExitReason, PendingReply};
use std::time::Duration;

const LIMIT: Duration = Duration::from_secs(5);

fn config() -> SupervisorConfig {
    SupervisorConfig::default()
        .name("root")
        .max_restarts(10)
        .backoff(
            Backoff::default()
                .initial(Duration::from_millis(1))
                .maximum(Duration::from_millis(4))
                .jitter(false),
        )
}

struct Worker;
impl Actor for Worker {}
struct Crash;
impl Message for Crash {
    type Result = ();
}
impl Handler<Crash> for Worker {
    async fn handle(&mut self, _: Crash, _: PendingReply<'_, ()>, _: &Context<Self>) {
        panic!("crash for a restart");
    }
}

#[cfg(not(spawned_sim))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tree_rows_and_counts_agree() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("tree", 2).unwrap();
    let mut builder = system.supervisor(config()).unwrap();
    let a = builder
        .add(ChildSpec::new("a", &group, |_| async {
            Ok::<_, String>(Worker)
        }))
        .unwrap();
    let b = builder
        .add(
            ChildSpec::new("b", &group, |_| async { Ok::<_, String>(Worker) })
                .restart(RestartMode::Transient),
        )
        .unwrap();
    let inner_group = group.clone();
    let nested = builder
        .add_supervisor(SupervisorSpec::new(
            "nested",
            &group,
            SupervisorConfig::default().strategy(SupervisorStrategy::OneForAll),
            move |nested| {
                nested
                    .add(ChildSpec::new("leaf", &inner_group, |_| async {
                        Ok::<_, String>(Worker)
                    }))
                    .map(|_| ())
            },
        ))
        .unwrap();
    let root = builder.start().await.unwrap();
    let a_ref = a.wait_ready().await.unwrap();
    let b_ref = b.wait_ready().await.unwrap();
    nested.wait_ready().await.unwrap();

    let rows = root.which_children();
    let keys: Vec<String> = rows.iter().map(|r| r.key.to_string()).collect();
    assert_eq!(keys, ["a", "b", "nested"]);
    assert_eq!(rows[0].key, ChildKey::Named("a".into()));
    assert_eq!(rows[0].actor, Some(a_ref.id()));
    assert_eq!(rows[1].actor, Some(b_ref.id()));
    assert_eq!(rows[1].restart, RestartMode::Transient);
    assert_eq!(
        rows.iter().map(|r| r.kind).collect::<Vec<_>>(),
        [ChildKind::Worker, ChildKind::Worker, ChildKind::Supervisor]
    );
    for row in &rows {
        assert!(row.ready && row.state == ChildState::Running, "{row:?}");
        assert_eq!((row.generation, row.restarts), (1, 0));
    }
    let counts = root.count_children();
    assert_eq!(
        (
            counts.specs,
            counts.active,
            counts.supervisors,
            counts.workers
        ),
        (3, 3, 1, 2)
    );
    assert_eq!(a.info(), rows[0]);
    assert_eq!(nested.info(), rows[2]);

    let tree = root.tree();
    assert_eq!(tree.name, "root");
    assert_eq!(tree.system, Some(system.id()));
    assert_eq!(tree.strategy, SupervisorStrategy::OneForOne);
    assert_eq!(tree.phase, SupervisorPhase::Running);
    assert_eq!(tree.budget.max_restarts, 10);
    assert_eq!(tree.counts, ChildCounts::of_rows(&tree));
    assert_eq!(tree.children.len(), 3);
    match &tree.children[0].node {
        TreeNode::Worker(Field::Value(info)) => {
            assert_eq!(info.id, a_ref.id());
            assert_eq!(info.lifecycle, Lifecycle::Idle);
            assert_eq!(info.supervisor.as_ref().unwrap().child, "a");
        }
        other => panic!("{other:?}"),
    }
    match &tree.children[2].node {
        TreeNode::Supervisor(Field::Value(sub)) => {
            assert_eq!(sub.name, "nested");
            assert_eq!(sub.strategy, SupervisorStrategy::OneForAll);
            assert_eq!(sub.children.len(), 1);
            assert_eq!(sub.children[0].info.key.to_string(), "leaf");
            assert!(matches!(
                sub.children[0].node,
                TreeNode::Worker(Field::Value(_))
            ));
        }
        other => panic!("{other:?}"),
    }

    // A restart: the row and its expanded node move to generation 2 together.
    a_ref.send(Crash).unwrap();
    let a2 = a.wait_for_generation(1).await.unwrap();
    let row = root.which_children().remove(0);
    assert_eq!((row.generation, row.restarts), (2, 1));
    assert_eq!(row.actor, Some(a2.id()));
    assert!(matches!(row.last_exit, Some(ExitReason::Panic(_))));
    match &root.tree().children[0].node {
        TreeNode::Worker(Field::Value(info)) => {
            assert_eq!(info.id, a2.id());
            assert_eq!(info.supervisor.as_ref().unwrap().generation, 2);
        }
        other => panic!("{other:?}"),
    }

    let report = system.shutdown(LIMIT).await;
    assert!(report.complete);
    let tree = root.tree();
    assert_eq!(tree.phase, SupervisorPhase::Stopped);
    assert_eq!(root.count_children().active, 0);
    assert!(tree.children.iter().all(|c| c.info.actor.is_none()
        && matches!(
            c.node,
            TreeNode::Worker(Field::Unavailable(_)) | TreeNode::Supervisor(Field::Unavailable(_))
        )));
}

/// I7 under seeded schedules: a OneForAll cohort restarting while `tree()`
/// runs between steps. Every level's counts agree with its rows, and every
/// expanded worker is the instance its row names.
#[spawned_concurrency::sim_test]
async fn tree_during_restarts() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("tree-sim", 2).unwrap();
    let mut builder = system
        .supervisor(config().strategy(SupervisorStrategy::OneForAll))
        .unwrap();
    let first = builder
        .add(ChildSpec::new("first", &group, |_| async {
            Ok::<_, String>(Worker)
        }))
        .unwrap();
    builder
        .add(ChildSpec::new("second", &group, |_| async {
            Ok::<_, String>(Worker)
        }))
        .unwrap();
    let root = builder.start().await.unwrap();
    let check = |tree: &spawned_concurrency::supervision::SupervisorTree| {
        assert_eq!(tree.counts, ChildCounts::of_rows(tree));
        for child in &tree.children {
            if let TreeNode::Worker(Field::Value(info)) = &child.node {
                assert_eq!(Some(info.id), child.info.actor, "{child:?}");
                let position = info.supervisor.as_ref().unwrap();
                assert_eq!(position.generation, child.info.generation, "{child:?}");
            }
        }
    };
    for round in 1..=3u64 {
        let actor = first.wait_for_generation(round - 1).await.unwrap();
        check(&root.tree());
        actor.send(Crash).unwrap();
        for _ in 0..8 {
            check(&root.tree());
            spawned_rt::tasks::yield_now().await;
        }
    }
    first.wait_for_generation(3).await.unwrap();
    check(&root.tree());
    assert!(system.shutdown(LIMIT).await.complete);
}

/// A nested supervisor instance that has not published its subtree yet (its
/// children are still starting) reads `Unavailable(Starting)`, not `Dead`;
/// once it is ready the node expands.
#[cfg(not(spawned_sim))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_starting_nested_supervisor_reads_starting() {
    use spawned_concurrency::introspection::Unavailable;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("tree-starting", 2).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let (open, gate) = tokio::sync::oneshot::channel::<()>();
    let gate = Arc::new(Mutex::new(Some(gate)));
    let mut builder = system.supervisor(config()).unwrap();
    let inner_group = group.clone();
    let leaves = Arc::new(Mutex::new(Vec::new()));
    let published = leaves.clone();
    let nested = builder
        .add_supervisor(
            SupervisorSpec::new(
                "nested",
                &group,
                // The leaf's crash ends its supervisor, which the root restarts.
                SupervisorConfig::default().max_restarts(0),
                move |nested| {
                    let calls = calls.clone();
                    let gate = gate.clone();
                    let published = published.clone();
                    nested
                        .add(ChildSpec::new("leaf", &inner_group, move |_| {
                            let first = calls.fetch_add(1, Ordering::SeqCst) == 0;
                            let gate = if first {
                                None
                            } else {
                                gate.lock().unwrap().take()
                            };
                            async move {
                                if let Some(gate) = gate {
                                    let _ = gate.await;
                                }
                                Ok::<_, String>(Worker)
                            }
                        }))
                        .map(|leaf| published.lock().unwrap().push(leaf))
                },
            )
            .startup_timeout(LIMIT),
        )
        .unwrap();
    let root = builder.start().await.unwrap();
    nested.wait_ready().await.unwrap();
    let first_leaf = leaves.lock().unwrap()[0].clone();
    first_leaf.wait_ready().await.unwrap().send(Crash).unwrap();
    let until = std::time::Instant::now() + LIMIT;
    loop {
        let tree = root.tree();
        let child = &tree.children[0];
        if child.info.generation == 2 && child.info.actor.is_some() {
            assert!(
                matches!(
                    child.node,
                    TreeNode::Supervisor(Field::Unavailable(Unavailable::Starting))
                ),
                "{child:?}"
            );
            assert_eq!(child.info.state, ChildState::Starting);
            break;
        }
        assert!(std::time::Instant::now() < until, "{child:?}");
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    open.send(()).unwrap();
    nested.wait_for_generation(1).await.unwrap();
    assert!(matches!(
        root.tree().children[0].node,
        TreeNode::Supervisor(Field::Value(_))
    ));
    assert!(system.shutdown(LIMIT).await.complete);
}

/// An ignored child (OTP `ignore`) keeps its row with no actor: it counts
/// as a spec and a worker but not as active, and its node is not expanded.
#[cfg(not(spawned_sim))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ignored_child_counts_as_a_spec_not_active() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("tree-ignore", 2).unwrap();
    let mut builder = system.supervisor(config()).unwrap();
    let ignored = builder
        .add(ChildSpec::new("optional", &group, |_| async {
            Ok::<_, String>(ChildStart::<Worker>::Ignore)
        }))
        .unwrap();
    let running = builder
        .add(ChildSpec::new("running", &group, |_| async {
            Ok::<_, String>(Worker)
        }))
        .unwrap();
    let root = builder.start().await.unwrap();
    running.wait_ready().await.unwrap();

    let rows = root.which_children();
    assert_eq!(rows[0].key, ChildKey::Named("optional".into()));
    assert_eq!(rows[0].state, ChildState::Ignored);
    assert_eq!((rows[0].actor, rows[0].ready), (None, false));
    assert_eq!(ignored.info(), rows[0]);
    let counts = root.count_children();
    assert_eq!(
        (
            counts.specs,
            counts.active,
            counts.supervisors,
            counts.workers
        ),
        (2, 1, 0, 2)
    );
    let tree = root.tree();
    assert_eq!(tree.counts, ChildCounts::of_rows(&tree));
    assert!(matches!(
        tree.children[0].node,
        TreeNode::Worker(Field::Unavailable(_))
    ));
    assert!(system.shutdown(LIMIT).await.complete);
}

trait CountsOfTree {
    fn of_rows(tree: &spawned_concurrency::supervision::SupervisorTree) -> ChildCounts;
}
impl CountsOfTree for ChildCounts {
    fn of_rows(tree: &spawned_concurrency::supervision::SupervisorTree) -> ChildCounts {
        let rows: Vec<_> = tree.children.iter().map(|c| &c.info).collect();
        let mut counts = ChildCounts::default();
        counts.specs = rows.len();
        counts.active = rows.iter().filter(|r| r.actor.is_some()).count();
        counts.supervisors = rows
            .iter()
            .filter(|r| r.kind == ChildKind::Supervisor)
            .count();
        counts.workers = rows.iter().filter(|r| r.kind == ChildKind::Worker).count();
        counts
    }
}
