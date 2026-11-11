#![cfg(feature = "metrics")]

use spawned_concurrency::{
    message::Message,
    metrics::{self, Config},
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
        reply.reply(());
    }
}

#[test]
fn embedded_components_can_add_sampling_requirements_after_actors_start() {
    let actor = Probe.start();
    actor.request(Ping).unwrap();
    assert_eq!(metrics::snapshot().queue_wait.count, 1);
    // The implicit default did not register an application requirement.
    metrics::configure_composable(Config::default().timing_sample_every(0));
    actor.request(Ping).unwrap();
    assert_eq!(metrics::snapshot().queue_wait.count, 1);

    // Two libraries initialize independently, after the host is already running.
    metrics::configure_composable(Config::default().timing_sample_every(8));
    metrics::configure_composable(Config::default().timing_sample_every(2));
    metrics::configure_composable(Config::default().timing_sample_every(0));
    metrics::configure_composable(Config::default().timing_sample_every(16));
    assert_eq!(metrics::configuration().timing_sample_every, 2);
    for _ in 0..32 {
        actor.request(Ping).unwrap();
    }
    actor.context().stop();
    actor.join();
    let snapshot = metrics::snapshot();
    assert_eq!(snapshot.messages_handled_total, 34);
    assert_eq!(snapshot.queue_wait.count, 17);
    assert_eq!(snapshot.handler_duration.count, 17);
    assert_eq!(snapshot.request_latency.count, 17);
    assert_eq!(
        snapshot.actors_active + snapshot.handlers_active + snapshot.mailbox_depth,
        0
    );
    assert!(metrics::configure(Config::default()).is_err());
}
