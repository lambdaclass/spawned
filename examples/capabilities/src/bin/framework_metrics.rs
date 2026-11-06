//! Optional existing metrics; this process owns configuration and actor lifetime.
use spawned_concurrency::{
    message::Message,
    metrics::{self, Config},
    tasks::{Actor, ActorStart, Context, Handler},
};
struct Worker;
struct Ping;
impl Message for Ping {
    type Result = ();
}
impl Actor for Worker {}
impl Handler<Ping> for Worker {
    async fn handle(&mut self, _: Ping, _: &Context<Self>) {}
}
#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    metrics::configure(Config {
        timing_sample_every: 16,
    })
    .unwrap();
    let actor = Worker.start();
    for _ in 0..32 {
        actor.request(Ping).await.unwrap();
    }
    actor.child_handle().stop();
    actor.join().await;
    let snapshot = metrics::snapshot();
    assert_eq!(snapshot.actors_active, 0);
    assert_eq!(snapshot.handlers_completed_total, 32);
    assert_eq!(snapshot.requests_replied_total, 32);
    assert_eq!(snapshot.requests_active, 0);
    assert_eq!(snapshot.handlers_active, 0);
    assert_eq!(snapshot.mailbox_depth, 0);
    assert!(snapshot.queue_wait.count > 0);
    assert!(snapshot.handler_duration.count > 0);
    assert!(snapshot.request_latency.count > 0);
    print!("{}", metrics::render_prometheus());
    // No exporter or actor/message labels: the application chooses publication.
}
