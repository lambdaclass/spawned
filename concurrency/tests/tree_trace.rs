#![cfg(feature = "lifecycle-trace")]
use spawned_concurrency::{
    message::Message,
    supervision::{
        Backoff, ChildSpec, SupervisedChild, SupervisorConfig, SupervisorError, SupervisorSpec,
        SupervisorStrategy,
    },
    system::ActorSystem,
    tasks::{Actor, Context, Handler},
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
const LIMIT: Duration = Duration::from_secs(5);
struct Worker;
impl Actor for Worker {}
struct Crash;
impl Message for Crash {
    type Result = ();
}
impl Handler<Crash> for Worker {
    async fn handle(
        &mut self,
        _: Crash,
        _reply: spawned_concurrency::reply::PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
        panic!("nested trace failure");
    }
}
fn config(strategy: SupervisorStrategy) -> SupervisorConfig {
    SupervisorConfig {
        strategy,
        max_restarts: 10,
        backoff: Backoff {
            initial: Duration::from_millis(1),
            maximum: Duration::from_millis(2),
            jitter: false,
            ..Backoff::default()
        },
        ..SupervisorConfig::default()
    }
}
#[path = "support/tree_record.rs"]
mod tree_record;
use tree_record::record;
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn mixed_strategy_three_level_shutdown_and_subtree_replacement() {
    for strategy in [
        SupervisorStrategy::OneForAll,
        SupervisorStrategy::RestForOne,
    ] {
        let system = ActorSystem::new().unwrap();
        let group = system.async_group("tree-trace", 2).unwrap();
        let leaf: Arc<Mutex<Option<SupervisedChild<Worker>>>> = Arc::new(Mutex::new(None));
        let expose = leaf.clone();
        let placement = group.clone();
        let mut root = system.supervisor(config(strategy)).unwrap();
        let subtree = root
            .add_supervisor(SupervisorSpec::new(
                "branch",
                &group,
                SupervisorConfig {
                    max_restarts: 0,
                    ..config(SupervisorStrategy::RestForOne)
                },
                move |middle| {
                    let placement = placement.clone();
                    let expose = expose.clone();
                    let control = placement.clone();
                    middle.add_supervisor(SupervisorSpec::new(
                        "nested",
                        &control,
                        SupervisorConfig {
                            max_restarts: 0,
                            ..config(SupervisorStrategy::OneForAll)
                        },
                        move |inner| {
                            *expose.lock().unwrap() =
                                Some(inner.add(ChildSpec::new("leaf", &placement, |_| async {
                                    Ok::<_, String>(Worker)
                                }))?);
                            Ok::<_, SupervisorError>(())
                        },
                    ))?;
                    Ok::<_, SupervisorError>(())
                },
            ))
            .unwrap();
        let supervisor = root.start().await.unwrap();
        let old = leaf.lock().unwrap().as_ref().unwrap().current().unwrap();
        assert!(old.request(Crash).await.is_err());
        spawned_rt::tasks::timeout(LIMIT, subtree.wait_for_generation(1))
            .await
            .unwrap()
            .unwrap();
        assert!(supervisor.shutdown(LIMIT).await.complete);
        assert!(system.shutdown(LIMIT).await.complete);
        record(&format!("nested-recovery-{strategy:?}"), &supervisor);
    }
}
