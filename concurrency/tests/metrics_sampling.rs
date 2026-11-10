#![cfg(feature = "async")]
#![cfg(feature = "metrics")]
use spawned_concurrency::{
    message::Message,
    metrics,
    tasks::{Actor, ActorStart, Context, Handler},
};
struct Probe;
struct Ping;
impl Message for Ping {
    type Result = ();
}
impl Actor for Probe {}
impl Handler<Ping> for Probe {
    async fn handle(&mut self, _: Ping, reply: spawned_concurrency::reply::PendingReply<'_, ()>, _: &Context<Self>) {
        let _ = reply.reply(());
    }
}
#[test]
fn sampling_retains_every_counter_and_scales_only_timing_observations() {
    metrics::configure(metrics::Config {
        timing_sample_every: 4,
    })
    .unwrap();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let actor = Probe.start();
        for _ in 0..16 {
            actor.request(Ping).await.unwrap();
        }
        actor.context().stop();
        actor.join().await;
    });
    let s = metrics::snapshot();
    assert_eq!(s.messages_handled_total, 16);
    assert_eq!(s.requests_replied_total, 16);
    assert_eq!(s.queue_wait.count, 4);
    assert_eq!(s.handler_duration.count, 4);
    assert_eq!(s.request_latency.count, 4);
    assert_eq!(
        s.actors_active + s.handlers_active + s.requests_active + s.mailbox_depth,
        0
    );
    metrics::configure_composable(metrics::Config {
        timing_sample_every: 8,
    });
    assert_eq!(metrics::configuration().timing_sample_every, 4);
    metrics::configure_composable(metrics::Config {
        timing_sample_every: 2,
    });
    assert_eq!(metrics::configuration().timing_sample_every, 2);
    assert!(metrics::configure(metrics::Config::default()).is_err());
}
