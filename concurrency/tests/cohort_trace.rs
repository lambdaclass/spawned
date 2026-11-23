#![cfg(feature = "lifecycle-trace")]
#[macro_use]
#[path = "support/deadline.rs"]
mod deadline;
use spawned_concurrency::{
    lifecycle_trace,
    message::Message,
    supervision::{Backoff, ChildSpec, RestartMode, SupervisorConfig, SupervisorStrategy},
    system::ActorSystem,
    tasks::{Actor, Context, Handler},
};
use std::{path::PathBuf, sync::Arc, time::Duration};
const LIMIT: Duration = Duration::from_secs(5);
struct Crash;
impl Message for Crash {
    type Result = ();
}
struct Worker;
impl Actor for Worker {}
impl Handler<Crash> for Worker {
    async fn handle(
        &mut self,
        _: Crash,
        _reply: spawned_concurrency::PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
        panic!("trace cohort failure");
    }
}
/// The round task records `CEnd complete` after the last affected child is
/// ready, which can be after `wait_for_generation` returns. Shutting down
/// first would end the trace without it, so the trace coverage gate would
/// depend on scheduling.
async fn round_completed(id: u64) {
    spawned_rt::tasks::timeout(LIMIT, async {
        while !lifecycle_trace::recorded_with(id, "CEnd", "complete") {
            spawned_rt::tasks::yield_now().await;
        }
    })
    .await
    .expect("the restart round must complete before shutdown");
}
fn write_trace(name: &str, id: u64, strategy: SupervisorStrategy, modes: &[&str]) {
    let trace = lifecycle_trace::take(id);
    for child in 1..=modes.len() {
        assert_eq!(
            trace
                .iter()
                .filter(|event| event.child == child && event.action == "Finish")
                .count(),
            1,
            "every controller must publish exactly one Finish, including retirement"
        );
    }
    let events = trace
        .into_iter()
        .filter(|event| {
            matches!(
                event.action,
                "CRequest"
                    | "CSelect"
                    | "CAssign"
                    | "CStop"
                    | "CKill"
                    | "CStuck"
                    | "CClean"
                    | "CResume"
                    | "CBegin"
                    | "CReady"
                    | "CJoined"
                    | "CRetire"
                    | "CAck"
                    | "CEnd"
                    | "CInitialReady"
                    | "CClose"
                    | "CDone"
            )
        })
        .map(|event| event.to_json())
        .collect::<Vec<_>>()
        .join(",\n");
    let directory = std::env::var_os("LIFECYCLE_TRACE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/lifecycle-traces")
        });
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join(format!("cohort-{name}.json")), format!(
        "{{\"schema\":\"cohort-v1\",\"name\":\"{name}\",\"strategy\":\"{strategy:?}\",\"n\":{},\"modes\":[{}],\"events\":[{}]}}",
        modes.len(), modes.iter().map(|mode|format!("\"{mode}\"")).collect::<Vec<_>>().join(","),events
    )).unwrap();
}
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn trace_cohort_selection_and_temporary_retirement() {
    for strategy in [
        SupervisorStrategy::OneForAll,
        SupervisorStrategy::RestForOne,
    ] {
        let system = ActorSystem::new().unwrap();
        let group = system.async_group("cohort-trace", 2).unwrap();
        let mut tree = system
            .supervisor(
                SupervisorConfig::default()
                    .strategy(strategy)
                    .max_restarts(30)
                    .backoff(
                        Backoff::default()
                            .initial(Duration::from_millis(1))
                            .maximum(Duration::from_millis(2))
                            .jitter(false),
                    ),
            )
            .unwrap();
        let id = tree.trace_id();
        let mut children = Vec::new();
        for i in 0..3 {
            children.push(
                tree.add(
                    ChildSpec::new(format!("child-{i}"), &group, |_| async {
                        Ok::<_, String>(Worker)
                    })
                    .restart(if i == 2 {
                        RestartMode::Temporary
                    } else {
                        RestartMode::Permanent
                    }),
                )
                .unwrap(),
            );
        }
        let supervisor = within!(tree.start()).await.unwrap();
        assert!(children[1].current().unwrap().request(Crash).await.is_err());
        spawned_rt::tasks::timeout(LIMIT, children[1].wait_for_generation(1))
            .await
            .unwrap()
            .unwrap();
        assert!(
            spawned_rt::tasks::timeout(LIMIT, children[2].wait_for_generation(1))
                .await
                .unwrap()
                .is_err()
        );
        round_completed(id).await;
        assert!(supervisor.shutdown(LIMIT).await.complete);
        write_trace(
            &format!("selection-{strategy:?}"),
            id,
            strategy,
            &["Permanent", "Permanent", "Temporary"],
        );
        assert!(system.shutdown(LIMIT).await.complete);
    }
}
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn trace_superseded_round_with_parked_temporary_tail() {
    for strategy in [
        SupervisorStrategy::OneForAll,
        SupervisorStrategy::RestForOne,
    ] {
        let system = ActorSystem::new().unwrap();
        let group = system.async_group("cohort-superseded-trace", 2).unwrap();
        let entered = Arc::new(tokio::sync::Notify::new());
        let mut tree = system
            .supervisor(
                SupervisorConfig::default()
                    .strategy(strategy)
                    .max_restarts(30),
            )
            .unwrap();
        let id = tree.trace_id();
        let mut children = Vec::new();
        for i in 0..3 {
            let entered = entered.clone();
            children.push(
                tree.add(
                    ChildSpec::new(format!("child-{i}"), &group, move |ctx| {
                        let entered = entered.clone();
                        async move {
                            if i == 1 && ctx.generation() == 2 {
                                entered.notify_one();
                                std::future::pending::<()>().await;
                            }
                            Ok::<_, String>(Worker)
                        }
                    })
                    .startup_timeout(Duration::from_secs(30))
                    .restart(if i == 2 {
                        RestartMode::Temporary
                    } else {
                        RestartMode::Permanent
                    }),
                )
                .unwrap(),
            );
        }
        let supervisor = within!(tree.start()).await.unwrap();
        assert!(children[1].current().unwrap().request(Crash).await.is_err());
        spawned_rt::tasks::timeout(LIMIT, entered.notified())
            .await
            .unwrap();
        let generation = children[0].snapshot().generation;
        assert!(children[0].current().unwrap().request(Crash).await.is_err());
        spawned_rt::tasks::timeout(LIMIT, children[0].wait_for_generation(generation))
            .await
            .unwrap()
            .unwrap();
        spawned_rt::tasks::timeout(LIMIT, children[1].wait_for_generation(2))
            .await
            .unwrap()
            .unwrap();
        assert!(
            spawned_rt::tasks::timeout(LIMIT, children[2].wait_for_generation(1))
                .await
                .unwrap()
                .is_err()
        );
        assert!(supervisor.shutdown(LIMIT).await.complete);
        write_trace(
            &format!("superseded-{strategy:?}"),
            id,
            strategy,
            &["Permanent", "Permanent", "Temporary"],
        );
        assert!(system.shutdown(LIMIT).await.complete);
    }
}

#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn trace_normally_retired_transient_is_skipped_by_later_cohorts() {
    for strategy in [
        SupervisorStrategy::OneForAll,
        SupervisorStrategy::RestForOne,
    ] {
        let system = ActorSystem::new().unwrap();
        let group = system.async_group("retired-transient-trace", 2).unwrap();
        let mut tree = system
            .supervisor(SupervisorConfig::default().strategy(strategy))
            .unwrap();
        let mut children = Vec::new();
        for i in 0..3 {
            children.push(
                tree.add(
                    ChildSpec::new(format!("child-{i}"), &group, |_| async {
                        Ok::<_, String>(Worker)
                    })
                    .restart(if i == 2 {
                        RestartMode::Transient
                    } else {
                        RestartMode::Permanent
                    }),
                )
                .unwrap(),
            );
        }
        let supervisor = within!(tree.start()).await.unwrap();
        children[2].current().unwrap().context().stop();
        spawned_rt::tasks::timeout(LIMIT, async {
            while children[2].snapshot().state
                != spawned_concurrency::supervision::ChildState::Stopped
            {
                spawned_rt::tasks::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(children[1].current().unwrap().request(Crash).await.is_err());
        spawned_rt::tasks::timeout(LIMIT, children[1].wait_for_generation(1))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(children[2].snapshot().generation, 1);
        round_completed(supervisor.trace_id()).await;
        assert!(supervisor.shutdown(LIMIT).await.complete);
        assert!(system.shutdown(LIMIT).await.complete);
        write_trace(
            &format!("retired-transient-{strategy:?}"),
            supervisor.trace_id(),
            strategy,
            &["Permanent", "Permanent", "Transient"],
        );
    }
}

struct MaybeStop(bool);
impl Actor for MaybeStop {
    async fn started(&mut self, ctx: &Context<Self>) {
        if self.0 {
            ctx.stop();
        }
    }
}
impl Handler<Crash> for MaybeStop {
    async fn handle(
        &mut self,
        _: Crash,
        _reply: spawned_concurrency::PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
        panic!("trace cohort failure");
    }
}
/// A replacement stopping before readiness inside a round requests a new
/// round: OneForAll rebuilds the whole cohort again. `Cohorts.tla` allows one
/// start per child and round (`OneStartPerRound`).
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn trace_failed_replacement_starts_a_new_round() {
    for strategy in [
        SupervisorStrategy::OneForAll,
        SupervisorStrategy::RestForOne,
    ] {
        let system = ActorSystem::new().unwrap();
        let group = system.async_group("cohort-pre-ready-trace", 2).unwrap();
        let mut tree = system
            .supervisor(
                SupervisorConfig::default()
                    .strategy(strategy)
                    .max_restarts(30)
                    .backoff(
                        Backoff::default()
                            .initial(Duration::from_millis(1))
                            .maximum(Duration::from_millis(2))
                            .jitter(false),
                    ),
            )
            .unwrap();
        let id = tree.trace_id();
        tree.add(ChildSpec::new("a", &group, |_| async {
            Ok::<_, String>(MaybeStop(false))
        }))
        .unwrap();
        let b = tree
            .add(
                ChildSpec::new("b", &group, |ctx| {
                    let stop = ctx.generation() == 2;
                    async move { Ok::<_, String>(MaybeStop(stop)) }
                })
                .restart(RestartMode::Transient),
            )
            .unwrap();
        let supervisor = within!(tree.start()).await.unwrap();
        assert!(b.current().unwrap().request(Crash).await.is_err());
        spawned_rt::tasks::timeout(LIMIT, b.wait_for_generation(2))
            .await
            .unwrap()
            .unwrap();
        round_completed(id).await;
        assert!(supervisor.shutdown(LIMIT).await.complete);
        write_trace(
            &format!("failed-replacement-{strategy:?}"),
            id,
            strategy,
            &["Permanent", "Transient"],
        );
        assert!(system.shutdown(LIMIT).await.complete);
    }
}

/// A turn held until the test releases it (V3's collateral child); the
/// sender is answered once the turn holds the gate.
struct Hold(spawned_rt::tasks::oneshot::Sender<()>);
impl Message for Hold {
    type Result = ();
}
type Gate = Arc<std::sync::Mutex<Option<spawned_rt::tasks::oneshot::Receiver<()>>>>;
struct Held(Gate);
impl Actor for Held {}
impl Handler<Crash> for Held {
    async fn handle(
        &mut self,
        _: Crash,
        _reply: spawned_concurrency::PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
        panic!("trace cohort failure");
    }
}
impl Handler<Hold> for Held {
    async fn handle(
        &mut self,
        Hold(entered): Hold,
        reply: spawned_concurrency::PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
        let gate = self.0.lock().unwrap().take();
        let _ = entered.send(());
        if let Some(gate) = gate {
            let _ = gate.await;
        }
        reply.reply(());
    }
}

/// V3 in a round: the collateral child misses its shutdown time and is
/// killed (CKill); released after the kill it ends and the round restarts
/// the cohort; never released, its slot is stuck (CStuck) a second
/// interval after the kill and the tree fails.
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn trace_collateral_child_killed_or_stuck() {
    const SHUTDOWN: Duration = Duration::from_millis(100);
    for strategy in [
        SupervisorStrategy::OneForAll,
        SupervisorStrategy::RestForOne,
    ] {
        // Stuck first: with a regression that skips the kill, the stuck
        // run still writes its trace (CStuck without CKill, rejected by
        // Cohorts.tla) before the released run fails waiting for CKill.
        for release in [false, true] {
            let system = ActorSystem::new().unwrap();
            let group = system.async_group("cohort-trace", 2).unwrap();
            let mut tree = system
                .supervisor(
                    SupervisorConfig::default()
                        .strategy(strategy)
                        .max_restarts(30)
                        .backoff(
                            Backoff::default()
                                .initial(Duration::from_millis(1))
                                .maximum(Duration::from_millis(2))
                                .jitter(false),
                        ),
                )
                .unwrap();
            let id = tree.trace_id();
            let origin = tree
                .add(
                    ChildSpec::new("origin", &group, |_| async {
                        Ok::<_, String>(Held(Arc::new(std::sync::Mutex::new(None))))
                    })
                    .shutdown_timeout(SHUTDOWN),
                )
                .unwrap();
            let (open, gate) = spawned_rt::tasks::oneshot::channel();
            let gate: Gate = Arc::new(std::sync::Mutex::new(Some(gate)));
            let first = gate.clone();
            let held = tree
                .add(
                    ChildSpec::new("held", &group, move |ctx| {
                        let gate = if ctx.generation() == 1 {
                            first.clone()
                        } else {
                            Arc::new(std::sync::Mutex::new(None))
                        };
                        async move { Ok::<_, String>(Held(gate)) }
                    })
                    .shutdown_timeout(SHUTDOWN),
                )
                .unwrap();
            let supervisor = within!(tree.start()).await.unwrap();
            let first_held = held.current().unwrap();
            let (entered, holding) = spawned_rt::tasks::oneshot::channel();
            first_held.send(Hold(entered)).unwrap();
            spawned_rt::tasks::timeout(LIMIT, holding)
                .await
                .expect("the held turn never began")
                .unwrap();
            origin.current().unwrap().send(Crash).unwrap();
            let mut open = Some(open);
            if release {
                // After the kill, before the second interval ends.
                spawned_rt::tasks::timeout(LIMIT, async {
                    while !lifecycle_trace::recorded(id, "CKill") {
                        spawned_rt::tasks::sleep(Duration::from_millis(5)).await;
                    }
                })
                .await
                .expect("the collateral child was not killed");
                let _ = open.take().unwrap().send(());
                // A ready generation after the first (strictly greater).
                spawned_rt::tasks::timeout(LIMIT, held.wait_for_generation(1))
                    .await
                    .unwrap()
                    .unwrap();
                round_completed(id).await;
                assert!(supervisor.shutdown(LIMIT).await.complete);
            } else {
                spawned_rt::tasks::timeout(LIMIT, async {
                    while supervisor.failure().is_none() {
                        spawned_rt::tasks::sleep(Duration::from_millis(5)).await;
                    }
                })
                .await
                .unwrap();
                assert!(lifecycle_trace::recorded(id, "CStuck"));
                let _ = open.take().unwrap().send(());
                spawned_rt::tasks::timeout(LIMIT, supervisor.join())
                    .await
                    .unwrap();
            }
            let reason = first_held.wait_exit().await;
            // The trace is written before the reason is checked, so the
            // model judges the run either way.
            write_trace(
                &format!(
                    "collateral-{}-{strategy:?}",
                    if release { "killed" } else { "stuck" }
                ),
                id,
                strategy,
                &["Permanent", "Permanent"],
            );
            assert_eq!(reason, spawned_concurrency::ExitReason::Killed);
            let _ = system.shutdown(LIMIT).await;
        }
    }
}
