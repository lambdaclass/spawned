#![cfg(feature = "async")]
//! Deterministic characterization of executor capacity; no machine-wide tuning.
// Tests that need OS threads are compiled out under spawned_sim.
#![cfg_attr(spawned_sim, allow(unused_imports, dead_code))]
#[macro_use]
#[path = "support/deadline.rs"]
mod deadline;
use spawned_concurrency::{
    message::Message,
    tasks::{Actor, ActorStart, Backend, Context, Handler},
    ChildHandle,
};
use std::time::Duration;
const LIMIT: Duration = Duration::from_secs(5);
struct Cleanup(Vec<ChildHandle>);
impl Drop for Cleanup {
    fn drop(&mut self) {
        for actor in &self.0 {
            actor.stop();
        }
    }
}
struct Ping;
impl Message for Ping {
    type Result = ();
}
struct Worker(Option<tokio::sync::oneshot::Sender<()>>);
impl Actor for Worker {
    async fn started(&mut self, _: &Context<Self>) {
        if let Some(tx) = self.0.take() {
            let _ = tx.send(());
        }
    }
}
impl Handler<Ping> for Worker {
    async fn handle(
        &mut self,
        _: Ping,
        reply: spawned_concurrency::PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
        reply.reply(());
    }
}
// Not simulated: Blocking backend workers are OS threads.
#[cfg(not(spawned_sim))]
#[test]
fn persistent_blocking_actors_queue_until_a_worker_is_released() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .max_blocking_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let mut cleanup = Cleanup(Vec::new());
        let mut actors = Vec::new();
        // Await both started hooks so the finite pool is occupied deterministically.
        for _ in 0..2 {
            let (tx, rx) = tokio::sync::oneshot::channel();
            let actor = Worker(Some(tx))
                .with_options(Backend::Blocking)
                .spawn()
                .unwrap();
            cleanup.0.push(actor.child_handle());
            tokio::time::timeout(LIMIT, rx).await.unwrap().unwrap();
            actors.push(actor);
        }
        let (tx, mut started) = tokio::sync::oneshot::channel();
        let queued = Worker(Some(tx))
            .with_options(Backend::Blocking)
            .spawn()
            .unwrap();
        cleanup.0.push(queued.child_handle());
        assert!(matches!(
            started.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(25), &mut started)
                .await
                .is_err(),
            "a third persistent actor unexpectedly obtained a blocking worker"
        );
        // Stopping an occupied actor releases a worker, allowing the queued actor to start.
        actors[0].child_handle().stop();
        tokio::time::timeout(LIMIT, actors[0].join()).await.unwrap();
        tokio::time::timeout(LIMIT, started).await.unwrap().unwrap();
        queued.request(Ping).await.unwrap();
        for actor in &cleanup.0 {
            actor.stop();
        }
        for actor in &cleanup.0 {
            tokio::time::timeout(LIMIT, actor.wait_exit_async())
                .await
                .unwrap();
        }
    });
}
