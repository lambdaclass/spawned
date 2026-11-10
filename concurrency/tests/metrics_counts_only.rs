#![cfg(feature = "metrics")]
use spawned_concurrency::{
    message::Message,
    metrics,
    threads::{Actor, ActorStart, Context, Handler},
};
struct Probe;
struct Ping;
impl Message for Ping {
    type Result = ();
}
impl Actor for Probe {}
impl Handler<Ping> for Probe {
    fn handle(
        &mut self,
        _: Ping,
        reply: spawned_concurrency::reply::PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
        let _ = reply.reply(());
    }
}
#[test]
fn zero_timing_rate_retains_counters_without_timing_samples() {
    metrics::configure(metrics::Config {
        timing_sample_every: 0,
    })
    .unwrap();
    let actor = Probe.start();
    for _ in 0..16 {
        actor.request(Ping).unwrap();
    }
    actor.context().stop();
    actor.join();
    let s = metrics::snapshot();
    assert_eq!(s.messages_handled_total, 16);
    assert_eq!(s.requests_replied_total, 16);
    assert_eq!(
        s.queue_wait.count + s.handler_duration.count + s.request_latency.count,
        0
    );
    assert_eq!(
        s.actors_active + s.handlers_active + s.requests_active + s.mailbox_depth,
        0
    );
    metrics::configure_composable(metrics::Config {
        timing_sample_every: 8,
    });
    assert_eq!(metrics::configuration().timing_sample_every, 8);
    assert!(metrics::configure(metrics::Config::default()).is_err());
}
