//! Identify a stuck handler, retain cleanup ownership, then finish shutdown.
use spawned_concurrency::{
    message::Message,
    supervision::{ChildSpec, RestartMode, SupervisorConfig},
    system::ActorSystem,
    tasks::{Actor, Context, Handler},
    ExitReason,
};
use std::time::Duration;
use tokio::sync::oneshot;

struct Worker;
impl Actor for Worker {}
struct Hold(oneshot::Sender<()>, oneshot::Receiver<()>);
impl Message for Hold {
    type Result = ();
}
impl Handler<Hold> for Worker {
    async fn handle(
        &mut self,
        msg: Hold,
        reply: spawned_concurrency::reply::PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
        
            let _ = msg.0.send(());
            let _ = msg.1.await;
reply.reply(());
    }
}
struct Unhealthy;
impl Message for Unhealthy {
    type Result = ();
}
impl Handler<Unhealthy> for Worker {
    async fn handle(
        &mut self,
        _: Unhealthy,
        reply: spawned_concurrency::reply::PendingReply<'_, ()>,
        ctx: &Context<Self>,
    ) {
        
            // Cooperative abnormal exit, without a panic or forced interruption.
            ctx.stop_with_reason(ExitReason::Panic("reported unhealthy worker".into()));
reply.reply(());
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("critical", 1).unwrap();
    let mut tree = system.supervisor(SupervisorConfig::default()).unwrap();
    let child = tree
        .add(
            ChildSpec::new("router", &group, |_| async { Ok::<_, String>(Worker) })
                .restart(RestartMode::Transient)
                .startup_timeout(Duration::from_millis(500))
                .shutdown_timeout(Duration::from_secs(1)),
        )
        .unwrap();
    let _supervisor = tree.start().await.unwrap();
    child.current().unwrap().request(Unhealthy).await.unwrap();
    let actor = tokio::time::timeout(Duration::from_secs(10), child.wait_for_generation(1))
        .await
        .unwrap()
        .unwrap();
    let (entered, arrival) = oneshot::channel();
    let (release, gate) = oneshot::channel();
    actor.send(Hold(entered, gate)).unwrap();
    tokio::time::timeout(Duration::from_secs(10), arrival)
        .await
        .unwrap()
        .unwrap();
    // Deliberately short report deadline. The system still owns the actor.
    let report = system.shutdown(Duration::from_millis(10)).await;
    assert!(!report.complete);
    let detail = report
        .unfinished_actor_details
        .iter()
        .find(|d| d.id == actor.id())
        .unwrap();
    assert_eq!(detail.name.as_deref(), Some("router"));
    assert_eq!(detail.message_type, Some(std::any::type_name::<Hold>()));
    println!(
        "Actor {} ({:?}, {}) is handling {:?}, turn age {:?}",
        detail.id, detail.name, detail.actor_type, detail.message_type, detail.turn_age
    );
    release.send(()).unwrap();
    assert!(report.wait(Duration::from_secs(10)).await.complete);
}
