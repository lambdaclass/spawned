//! Explicit scheduling ownership without changing ordinary message handlers.
use spawned_concurrency::{
    message::Message,
    system::ActorSystem,
    tasks::{Actor, ActorRef, ActorStart, Context, Handler},
};
use std::time::Duration;
struct Double(u64);
impl Message for Double {
    type Result = u64;
}
struct Worker;
impl Actor for Worker {}
impl Handler<Double> for Worker {
    async fn handle(
        &mut self,
        msg: Double,
        reply: spawned_concurrency::reply::PendingReply<'_, u64>,
        _: &Context<Self>,
    ) {
        let __reply_value: u64 = { msg.0 * 2 };
        let _ = reply.reply(__reply_value);
    }
}
struct Router(ActorRef<Worker>);
impl Actor for Router {}
impl Handler<Double> for Router {
    async fn handle(
        &mut self,
        msg: Double,
        reply: spawned_concurrency::reply::PendingReply<'_, u64>,
        _: &Context<Self>,
    ) {
        let __reply_value: u64 = { self.0.request(msg).await.unwrap() };
        let _ = reply.reply(__reply_value);
    }
}
#[tokio::main(flavor = "current_thread")]
async fn main() {
    let system = ActorSystem::new().unwrap();
    let critical = system.async_group("critical", 1).unwrap();
    let general = system.async_group("general", 2).unwrap();
    let worker = Worker.start_on(&general).unwrap();
    let router = Router(worker).start_on(&critical).unwrap();
    router.ready().await.unwrap();
    assert_eq!(router.request(Double(21)).await.unwrap(), 42);
    let helper = router
        .context()
        .spawn_helper(async {
            spawned_rt::tasks::sleep(Duration::from_millis(1)).await;
            std::thread::current().name().unwrap().to_string()
        })
        .unwrap();
    assert!(helper.join().await.unwrap().starts_with("spawned-critical"));
    let report = system.shutdown(Duration::from_secs(3)).await;
    assert!(
        report.complete,
        "unfinished groups: {:?}",
        report.unfinished_groups
    );
    assert!(router.request(Double(1)).await.is_err());
    println!("Cross-group request and owned shutdown passed; stale references stay stopped.");
}
