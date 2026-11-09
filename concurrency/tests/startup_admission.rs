//! Managed admission cancellation intentionally differs from legacy start().
#![cfg(feature = "async")]
use spawned_concurrency::{
    system::ActorSystem,
    tasks::{Actor, ActorStart, Context},
    ExitReason,
};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
const LIMIT: Duration = Duration::from_secs(10);
#[derive(Default)]
struct Counts {
    started: AtomicUsize,
    stopped: AtomicUsize,
    dropped: AtomicUsize,
}
struct Probe(Arc<Counts>);
impl Actor for Probe {
    async fn started(&mut self, _: &Context<Self>) {
        self.0.started.fetch_add(1, Ordering::SeqCst);
    }
    async fn stopped(&mut self, _: &Context<Self>) {
        self.0.stopped.fetch_add(1, Ordering::SeqCst);
    }
}
impl Drop for Probe {
    fn drop(&mut self) {
        self.0.dropped.fetch_add(1, Ordering::SeqCst);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn managed_actor_stopped_before_first_poll_runs_no_hooks_but_drops_state() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("startup-gate", 1).unwrap();
    let owner = Probe(Arc::default()).start_on(&group).unwrap();
    tokio::time::timeout(LIMIT, owner.ready())
        .await
        .unwrap()
        .unwrap();
    let (entered, arrival) = tokio::sync::oneshot::channel();
    let (release, gate) = std::sync::mpsc::channel();
    // Intentionally occupy the sole worker to force the next root to remain
    // unpolled. Production helpers must not block an async worker like this.
    let blocker = owner
        .context()
        .spawn_helper(async move {
            entered.send(()).unwrap();
            gate.recv_timeout(LIMIT)
                .expect("test failed to release worker");
        })
        .unwrap();
    tokio::time::timeout(LIMIT, arrival).await.unwrap().unwrap();
    let counts = Arc::new(Counts::default());
    let actor = Probe(counts.clone()).start_on(&group).unwrap();
    actor.child_handle().shutdown();
    release.send(()).unwrap();
    tokio::time::timeout(LIMIT, blocker.join())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        tokio::time::timeout(LIMIT, actor.wait_exit())
            .await
            .unwrap(),
        ExitReason::Shutdown
    );
    assert!(tokio::time::timeout(LIMIT, actor.ready())
        .await
        .unwrap()
        .is_err());
    assert_eq!(counts.started.load(Ordering::SeqCst), 0);
    assert_eq!(counts.stopped.load(Ordering::SeqCst), 0);
    assert_eq!(counts.dropped.load(Ordering::SeqCst), 1);
    assert!(system.shutdown(LIMIT).await.complete);
}

#[tokio::test(flavor = "current_thread")]
async fn legacy_actor_stopped_before_first_poll_keeps_both_hooks() {
    let counts = Arc::new(Counts::default());
    let actor = Probe(counts.clone()).start(); // this runtime has not yielded
    actor.child_handle().shutdown();
    assert_eq!(
        tokio::time::timeout(LIMIT, actor.wait_exit())
            .await
            .unwrap(),
        ExitReason::Shutdown
    );
    assert_eq!(counts.started.load(Ordering::SeqCst), 1);
    assert_eq!(counts.stopped.load(Ordering::SeqCst), 1);
    assert_eq!(counts.dropped.load(Ordering::SeqCst), 1);
}
