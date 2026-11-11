//! Nested recovery and ordered dependency cohorts; no state or message replay.
use spawned_concurrency::{
    message::Message,
    supervision::{
        ChildSpec, SupervisedChild, SupervisorConfig, SupervisorError, SupervisorSpec,
        SupervisorStrategy,
    },
    system::ActorSystem,
    tasks::{Actor, Context, Handler},
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
struct Worker;
impl Actor for Worker {}
struct Fail;
impl Message for Fail {
    type Result = ();
}
impl Handler<Fail> for Worker {
    async fn handle(
        &mut self,
        _: Fail,
        _reply: spawned_concurrency::reply::PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
        {
            panic!("example failure");
        }
    }
}
#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    for strategy in [
        SupervisorStrategy::OneForAll,
        SupervisorStrategy::RestForOne,
    ] {
        let system = ActorSystem::new().unwrap();
        let controls = system.async_group("tree-control", 1).unwrap();
        let workers = system.thread_group("tree-workers", 3).unwrap();
        let mut tree = system
            .supervisor(
                SupervisorConfig::default()
                    .strategy(strategy)
                    .max_restarts(20),
            )
            .unwrap();
        let earlier = tree
            .add(ChildSpec::new("earlier", &workers, |_| async {
                Ok::<_, String>(Worker)
            }))
            .unwrap();
        let published: Arc<Mutex<Option<SupervisedChild<Worker>>>> = Arc::new(Mutex::new(None));
        let exposed = published.clone();
        let placement = workers.clone();
        let nested = tree
            .add_supervisor(SupervisorSpec::new(
                "subsystem",
                &controls,
                SupervisorConfig::default().max_restarts(0),
                move |subtree| {
                    let child = subtree.add(ChildSpec::new("leaf", &placement, |_| async {
                        Ok::<_, String>(Worker)
                    }))?;
                    *exposed.lock().unwrap() = Some(child);
                    Ok::<_, SupervisorError>(())
                },
            ))
            .unwrap();
        let later = tree
            .add(ChildSpec::new("dependent", &workers, |_| async {
                Ok::<_, String>(Worker)
            }))
            .unwrap();
        let supervisor = tree.start().await.unwrap();
        let old_earlier = earlier.current().unwrap();
        let old_leaf = published
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .current()
            .unwrap();
        assert!(old_leaf.request(Fail).await.is_err());
        tokio::time::timeout(Duration::from_secs(5), nested.wait_for_generation(1))
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), later.wait_for_generation(1))
            .await
            .unwrap()
            .unwrap();
        assert!(old_leaf.send(Fail).is_err());
        match strategy {
            SupervisorStrategy::OneForAll => {
                assert_ne!(earlier.current().unwrap().id(), old_earlier.id())
            }
            SupervisorStrategy::RestForOne => {
                assert_eq!(earlier.current().unwrap().id(), old_earlier.id())
            }
            _ => unreachable!(),
        }
        println!("{strategy:?}: {:?}", supervisor.snapshots());
        assert!(supervisor.shutdown(Duration::from_secs(5)).await.complete);
        assert!(system.shutdown(Duration::from_secs(5)).await.complete);
    }
}
