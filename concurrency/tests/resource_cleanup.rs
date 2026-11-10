// Tests that need OS threads are compiled out under spawned_sim.
#![cfg_attr(spawned_sim, allow(unused_imports, dead_code))]
#[cfg(feature = "async")]
use spawned_concurrency::tasks;
use spawned_concurrency::{message::Message, threads};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};
#[derive(Clone)]
struct Probe(Arc<()>);
impl Message for Probe {
    type Result = ();
}
struct Sink;
#[cfg(feature = "async")]
impl tasks::Actor for Sink {}
#[cfg(feature = "async")]
impl tasks::Handler<Probe> for Sink {
    async fn handle(
        &mut self,
        msg: Probe,
        reply: spawned_concurrency::reply::PendingReply<'_, ()>,
        _: &tasks::Context<Self>,
    ) {
        {
            drop(msg.0);
        }
        let _ = reply.reply(());
    }
}
impl threads::Actor for Sink {}
impl threads::Handler<Probe> for Sink {
    fn handle(
        &mut self,
        msg: Probe,
        reply: spawned_concurrency::reply::PendingReply<'_, ()>,
        _: &threads::Context<Self>,
    ) {
        {
            drop(msg.0);
        }
        let _ = reply.reply(());
    }
}
fn released(weak: &Weak<()>) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while weak.strong_count() > 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(
        weak.strong_count(),
        0,
        "background operation retained payload after shutdown"
    );
}
// Not simulated: builds its own Tokio runtime.
#[cfg(not(spawned_sim))]
#[test]
fn timers_release_payloads_on_actor_exit() {
    #[cfg(feature = "async")]
    for backend in [
        tasks::Backend::Async,
        tasks::Backend::Blocking,
        tasks::Backend::Thread,
    ] {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _entered = rt.enter();
        let actor = tasks::ActorStart::start_with_backend(Sink, backend);
        let probe = Arc::new(());
        let weak = Arc::downgrade(&probe);
        let once = tasks::send_after(
            Duration::from_secs(3600),
            actor.context(),
            Probe(probe.clone()),
        );
        let interval =
            tasks::send_interval(Duration::from_secs(3600), actor.context(), Probe(probe));
        actor.child_handle().stop();
        rt.block_on(actor.join());
        released(&weak);
        drop((once, interval));
    }
    let actor = threads::ActorStart::start(Sink);
    let probe = Arc::new(());
    let weak = Arc::downgrade(&probe);
    let once = threads::send_after(
        Duration::from_secs(3600),
        actor.context(),
        Probe(probe.clone()),
    );
    let interval = threads::send_interval(Duration::from_secs(3600), actor.context(), Probe(probe));
    actor.child_handle().stop();
    actor.join();
    released(&weak);
    drop((once, interval));
}
// Not simulated: builds its own Tokio runtime.
#[cfg(not(spawned_sim))]
#[cfg(feature = "async")]
#[test]
fn pending_async_listener_is_dropped_on_actor_exit() {
    #[cfg(feature = "async")]
    for backend in [
        tasks::Backend::Async,
        tasks::Backend::Blocking,
        tasks::Backend::Thread,
    ] {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let actor = tasks::ActorStart::start_with_backend(Sink, backend);
            let probe = Arc::new(());
            let weak = Arc::downgrade(&probe);
            let stream = futures::stream::unfold(probe, |probe| async move {
                futures::future::pending::<()>().await;
                Some((Probe(probe.clone()), probe))
            });
            let listener = tasks::spawn_listener(actor.context(), stream);
            actor.child_handle().stop();
            actor.join().await;
            tokio::time::timeout(Duration::from_secs(2), listener)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(weak.strong_count(), 0);
        });
    }
}

#[cfg(feature = "async")]
#[test]
fn cancelled_thread_exit_wait_does_not_occupy_blocking_pool() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let actor = threads::ActorStart::start(Sink);
    let child = actor.child_handle();
    let available = rt.block_on(async {
        assert!(
            tokio::time::timeout(Duration::from_millis(10), child.wait_exit_async())
                .await
                .is_err()
        );
        let probe = tokio::task::spawn_blocking(|| 42);
        tokio::time::timeout(Duration::from_secs(1), probe)
            .await
            .is_ok()
    });
    // Always stop the target before asserting, including on the buggy version.
    child.stop();
    actor.join();
    assert!(
        available,
        "cancelled exit wait retained the sole blocking worker"
    );
}

#[test]
fn thread_exit_waits_work_inside_futures_executor() {
    for already_exited in [false, true] {
        let actor = threads::ActorStart::start(Sink);
        let child = actor.child_handle();
        child.stop();
        if already_exited {
            actor.join();
        }
        futures::executor::block_on(async {
            assert_eq!(
                child.wait_exit_blocking(),
                spawned_concurrency::ExitReason::Normal
            );
            assert_eq!(actor.wait_exit(), spawned_concurrency::ExitReason::Normal);
            actor.join();
        });
    }
}
