#![cfg(feature = "async")]
#![cfg(feature = "metrics")]
use spawned_concurrency::{
    metrics,
    tasks::{Actor, ActorStart, Backend, Context, Handler},
    ActorError, ExitReason, Response,
};
use std::time::Duration;
use tokio::sync::oneshot;

#[spawned_concurrency::protocol]
pub trait MetricsProtocol: Send + Sync {
    fn query(&self) -> Response<u64>;
}

struct Ping;
impl spawned_concurrency::message::Message for Ping {
    type Result = ();
}
struct Crash;
impl spawned_concurrency::message::Message for Crash {
    type Result = ();
}
struct Hold {
    entered: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
}
impl spawned_concurrency::message::Message for Hold {
    type Result = ();
}
struct Probe {
    gate: Option<oneshot::Receiver<()>>,
}
impl Actor for Probe {
    async fn started(&mut self, _: &Context<Self>) {
        if let Some(gate) = self.gate.take() {
            let _ = gate.await;
        }
    }
}
impl Handler<metrics_protocol::Query> for Probe {
    async fn handle(
        &mut self,
        _: metrics_protocol::Query,
        reply: spawned_concurrency::reply::PendingReply<'_, u64>,
        _: &Context<Self>,
    ) {
        reply.reply(9);
    }
}
impl Handler<Ping> for Probe {
    async fn handle(
        &mut self,
        _: Ping,
        reply: spawned_concurrency::reply::PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
        reply.reply(());
    }
}
impl Handler<Crash> for Probe {
    async fn handle(
        &mut self,
        _: Crash,
        _reply: spawned_concurrency::reply::PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
        panic!("metric panic regression");
    }
}
impl Handler<Hold> for Probe {
    async fn handle(
        &mut self,
        hold: Hold,
        reply: spawned_concurrency::reply::PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
        let _ = hold.entered.send(());
        let _ = hold.release.await;
        reply.reply(());
    }
}
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

// One test in its own integration-test process: process aggregates are never
// reset, and these assertions must not race unrelated actors in other tests.
#[test]
fn metrics_conserve_work_across_all_lifecycle_paths() {
    metrics::configure(metrics::Config {
        timing_sample_every: 1,
    })
    .unwrap();
    runtime().block_on(async {
        let (release, gate) = oneshot::channel();
        let actor = Probe { gate: Some(gate) }.start();
        let replies: Vec<_> = (0..10).map(|_| actor.request_raw(Ping).unwrap()).collect();
        assert_eq!(actor.mailbox_depth(), 10);
        assert_eq!(actor.context().mailbox_depth(), 10);
        assert!(matches!(
            actor.request_with_timeout(Ping, Duration::ZERO).await,
            Err(ActorError::RequestTimeout)
        ));
        assert!(matches!(
            Response::from(
                actor.request_raw_until(Ping, spawned_concurrency::Deadline::after(Duration::ZERO))
            )
            .await,
            Err(ActorError::RequestTimeout)
        ));
        // Dropping either kind of caller wrapper does not cancel actor work.
        drop(Response::from(actor.request_raw(Ping)));
        let mut cancelled = Box::pin(actor.request(Ping));
        assert!(futures::poll!(cancelled.as_mut()).is_pending());
        drop(cancelled);
        release.send(()).unwrap();
        for reply in replies {
            reply.await.unwrap();
        }
        actor.request(Ping).await.unwrap();
        let before = metrics::snapshot().requests_started_total;
        assert_eq!(actor.query().await.unwrap(), 9);
        assert_eq!(metrics::snapshot().requests_started_total, before + 1);
        actor.context().request(Ping).await.unwrap();
        let recipient = actor.recipient::<Ping>();
        spawned_concurrency::tasks::request(recipient.as_ref(), Ping, Duration::from_secs(2))
            .await
            .unwrap();
        Response::from(actor.request_raw(Ping)).await.unwrap();
        Response::from(actor.request_raw_until(
            Ping,
            spawned_concurrency::Deadline::after(Duration::from_secs(2)),
        ))
        .await
        .unwrap();
        actor.context().stop();
        assert_eq!(actor.wait_exit().await, ExitReason::Normal);
        assert_eq!(actor.mailbox_depth(), 0);
        assert!(actor.send(Ping).is_err());
        assert!(matches!(
            Response::from(actor.request_raw_until(
                Ping,
                spawned_concurrency::Deadline::after(Duration::from_secs(2))
            ))
            .await,
            Err(ActorError::NoProc)
        ));

        let (release, gate) = oneshot::channel();
        let actor = Probe { gate: Some(gate) }.start();
        let raw = actor.request_raw(Ping).unwrap();
        actor.send(Ping).unwrap();
        let mut pending = Box::pin(actor.request(Ping));
        assert!(futures::poll!(pending.as_mut()).is_pending());
        actor.context().stop();
        release.send(()).unwrap();
        assert!(matches!(
            pending.await,
            Err(ActorError::ActorStopped(ExitReason::Normal))
        ));
        assert!(raw.await.is_err());
        actor.join().await;
        assert_eq!(actor.mailbox_depth(), 0);

        let actor = Probe { gate: None }.start();
        assert!(matches!(
            actor.request(Crash).await,
            Err(ActorError::ActorStopped(ExitReason::Panic(_)))
        ));
        assert!(matches!(actor.wait_exit().await, ExitReason::Panic(_)));
        // Managed restart/shutdown must conserve the same aggregate accounting.
        let system = spawned_concurrency::system::ActorSystem::new().unwrap();
        let group = system.async_group("metrics-recovery", 1).unwrap();
        let mut tree = system
            .supervisor(spawned_concurrency::supervision::SupervisorConfig::default())
            .unwrap();
        let child = tree
            .add(spawned_concurrency::supervision::ChildSpec::new(
                "probe",
                &group,
                |_| async { Ok::<_, String>(Probe { gate: None }) },
            ))
            .unwrap();
        let supervisor = tree.start().await.unwrap();
        let first = child.current().unwrap();
        assert_eq!(first.query().await.unwrap(), 9);
        assert!(first.request(Crash).await.is_err());
        let next = tokio::time::timeout(Duration::from_secs(3), child.wait_for_generation(1))
            .await
            .unwrap()
            .unwrap();
        assert_ne!(first.id(), next.id());
        next.request(Ping).await.unwrap();
        assert!(supervisor.shutdown(Duration::from_secs(3)).await.complete);
        assert!(system.shutdown(Duration::from_secs(3)).await.complete);
        assert_eq!(first.mailbox_depth(), 0);
        assert_eq!(next.mailbox_depth(), 0);
    });
    // Both never-polled and actively-handling runtime aborts close accounting.
    for poll_first in [false, true] {
        let owner = runtime();
        let actor = {
            let _entered = owner.enter();
            Probe { gate: None }.start()
        };
        let (entered_tx, entered) = oneshot::channel();
        let (_release, release) = oneshot::channel();
        actor
            .send(Hold {
                entered: entered_tx,
                release,
            })
            .unwrap();
        actor.send(Ping).unwrap();
        if poll_first {
            owner.block_on(entered).unwrap();
        }
        drop(owner);
        assert_eq!(actor.exit_reason(), Some(ExitReason::Killed));
        assert_eq!(actor.mailbox_depth(), 0);
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        for backend in [Backend::Blocking, Backend::Thread] {
            let actor = Probe { gate: None }.start_with_backend(backend);
            actor.request(Ping).await.unwrap();
            actor.context().stop();
            actor.join().await;
        }
    });
    drop(rt);

    use spawned_concurrency::threads::{
        Actor as ThreadActor, ActorStart as ThreadStart, Context as ThreadContext,
        Handler as ThreadHandler,
    };
    struct Native;
    impl ThreadActor for Native {}
    impl ThreadHandler<metrics_protocol::Query> for Native {
        fn handle(
            &mut self,
            _: metrics_protocol::Query,
            reply: spawned_concurrency::reply::PendingReply<'_, u64>,
            _: &ThreadContext<Self>,
        ) {
            reply.reply(9);
        }
    }
    impl ThreadHandler<Ping> for Native {
        fn handle(
            &mut self,
            _: Ping,
            reply: spawned_concurrency::reply::PendingReply<'_, ()>,
            _: &ThreadContext<Self>,
        ) {
            reply.reply(());
        }
    }
    impl ThreadHandler<Crash> for Native {
        fn handle(
            &mut self,
            _: Crash,
            _reply: spawned_concurrency::reply::PendingReply<'_, ()>,
            _: &ThreadContext<Self>,
        ) {
            panic!("native metric panic");
        }
    }
    let actor = Native.start();
    actor.request(Ping).unwrap();
    let before = metrics::snapshot().requests_started_total;
    assert_eq!(actor.query().unwrap(), 9);
    assert_eq!(metrics::snapshot().requests_started_total, before + 1);
    actor.context().request(Ping).unwrap();
    let recipient = actor.recipient::<Ping>();
    spawned_concurrency::threads::request(recipient.as_ref(), Ping, Duration::from_secs(2))
        .unwrap();
    // Ready results are already observed by the synchronous request, not twice.
    Response::ready(actor.request(Ping)).unwrap();
    assert!(actor.request(Crash).is_err());
    assert!(matches!(actor.wait_exit(), ExitReason::Panic(_)));
    assert!(actor.send(Ping).is_err());
    assert_eq!(actor.mailbox_depth(), 0);

    let s = metrics::snapshot();
    assert_eq!(s.actors_active, 0);
    assert_eq!(s.handlers_active, 0);
    assert_eq!(s.requests_active, 0);
    assert_eq!(s.mailbox_depth, 0);
    assert_eq!(
        s.actors_spawned_total,
        s.actors_exited_normal_total
            + s.actors_exited_shutdown_total
            + s.actors_exited_panic_total
            + s.actors_exited_kill_total
            + s.actors_exited_killed_total
            + s.actors_exited_noproc_total
    );
    assert_eq!(
        s.messages_enqueued_total,
        s.messages_handled_total + s.messages_discarded_total
    );
    assert_eq!(
        s.messages_handled_total,
        s.handlers_completed_total + s.handlers_panicked_total + s.handlers_aborted_total
    );
    assert_eq!(
        s.requests_started_total,
        s.requests_replied_total
            + s.requests_timed_out_total
            + s.requests_actor_stopped_total
            // Requests to an actor past its death point (rank 11 x 5e).
            + s.requests_noproc_total
            + s.requests_cancelled_total
    );
    assert!(s.actors_exited_shutdown_total >= 1);
    assert!(s.handlers_panicked_total >= 3);
    assert!(s.handlers_aborted_total >= 1);
    assert!(s.messages_discarded_total >= 4);
    assert!(s.messages_rejected_total >= 3);
    assert!(s.requests_timed_out_total >= 2);
    assert!(s.requests_cancelled_total >= 2);
    assert!(s.reply_receivers_dropped_total >= 4);
    assert_eq!(s.queue_wait.count, s.messages_handled_total);
    assert_eq!(s.handler_duration.count, s.messages_handled_total);
    assert_eq!(s.request_latency.count, s.requests_started_total);
    let text = metrics::render_prometheus();
    assert!(text.contains("# TYPE spawned_handler_duration_seconds histogram"));
    assert!(text.contains("spawned_mailbox_depth 0\n"));
    assert!(!text.contains("Probe"));
    assert!(!text.contains("ActorId"));
    assert!(text.len() < 16_384);
}
