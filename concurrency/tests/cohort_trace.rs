#![cfg(feature = "lifecycle-trace")]
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
    async fn handle(&mut self, _: Crash, _: &Context<Self>) {
        panic!("trace cohort failure");
    }
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
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trace_cohort_selection_and_temporary_retirement() {
    for strategy in [
        SupervisorStrategy::OneForAll,
        SupervisorStrategy::RestForOne,
    ] {
        let system = ActorSystem::new().unwrap();
        let group = system.async_group("cohort-trace", 2).unwrap();
        let mut tree = system
            .supervisor(SupervisorConfig {
                strategy,
                max_restarts: 30,
                backoff: Backoff {
                    initial: Duration::from_millis(1),
                    maximum: Duration::from_millis(2),
                    jitter: false,
                    ..Backoff::default()
                },
                ..SupervisorConfig::default()
            })
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
        let supervisor = tree.start().await.unwrap();
        assert!(children[1].current().unwrap().request(Crash).await.is_err());
        tokio::time::timeout(LIMIT, children[1].wait_for_generation(1))
            .await
            .unwrap()
            .unwrap();
        assert!(
            tokio::time::timeout(LIMIT, children[2].wait_for_generation(1))
                .await
                .unwrap()
                .is_err()
        );
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
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trace_superseded_round_with_parked_temporary_tail() {
    for strategy in [
        SupervisorStrategy::OneForAll,
        SupervisorStrategy::RestForOne,
    ] {
        let system = ActorSystem::new().unwrap();
        let group = system.async_group("cohort-superseded-trace", 2).unwrap();
        let entered = Arc::new(tokio::sync::Notify::new());
        let mut tree = system
            .supervisor(SupervisorConfig {
                strategy,
                max_restarts: 30,
                ..SupervisorConfig::default()
            })
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
        let supervisor = tree.start().await.unwrap();
        assert!(children[1].current().unwrap().request(Crash).await.is_err());
        tokio::time::timeout(LIMIT, entered.notified())
            .await
            .unwrap();
        let generation = children[0].snapshot().generation;
        assert!(children[0].current().unwrap().request(Crash).await.is_err());
        tokio::time::timeout(LIMIT, children[0].wait_for_generation(generation))
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(LIMIT, children[1].wait_for_generation(2))
            .await
            .unwrap()
            .unwrap();
        assert!(
            tokio::time::timeout(LIMIT, children[2].wait_for_generation(1))
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trace_normally_retired_transient_is_skipped_by_later_cohorts() {
    for strategy in [
        SupervisorStrategy::OneForAll,
        SupervisorStrategy::RestForOne,
    ] {
        let system = ActorSystem::new().unwrap();
        let group = system.async_group("retired-transient-trace", 2).unwrap();
        let mut tree = system
            .supervisor(SupervisorConfig {
                strategy,
                ..SupervisorConfig::default()
            })
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
        let supervisor = tree.start().await.unwrap();
        children[2].current().unwrap().context().stop();
        tokio::time::timeout(LIMIT, async {
            while children[2].snapshot().state
                != spawned_concurrency::supervision::ChildState::Stopped
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(children[1].current().unwrap().request(Crash).await.is_err());
        tokio::time::timeout(LIMIT, children[1].wait_for_generation(1))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(children[2].snapshot().generation, 1);
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
    async fn handle(&mut self, _: Crash, _: &Context<Self>) {
        panic!("trace cohort failure");
    }
}
/// A replacement stopping before readiness inside a round requests a new
/// round: OneForAll rebuilds the whole cohort again. `Cohorts.tla` allows one
/// start per child and round (`OneStartPerRound`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trace_failed_replacement_starts_a_new_round() {
    for strategy in [
        SupervisorStrategy::OneForAll,
        SupervisorStrategy::RestForOne,
    ] {
        let system = ActorSystem::new().unwrap();
        let group = system.async_group("cohort-pre-ready-trace", 2).unwrap();
        let mut tree = system
            .supervisor(SupervisorConfig {
                strategy,
                max_restarts: 30,
                backoff: Backoff {
                    initial: Duration::from_millis(1),
                    maximum: Duration::from_millis(2),
                    jitter: false,
                    ..Backoff::default()
                },
                ..SupervisorConfig::default()
            })
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
        let supervisor = tree.start().await.unwrap();
        assert!(b.current().unwrap().request(Crash).await.is_err());
        tokio::time::timeout(LIMIT, b.wait_for_generation(2))
            .await
            .unwrap()
            .unwrap();
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
