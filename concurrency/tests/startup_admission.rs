//! Managed admission cancellation intentionally differs from legacy start().
#![cfg(feature = "async")]
// Tests that need OS threads are compiled out under spawned_sim.
#![cfg_attr(spawned_sim, allow(unused_imports, dead_code))]
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
struct Queued(Arc<AtomicUsize>);
impl spawned_concurrency::message::Message for Queued {
    type Result = ();
}
impl Drop for Queued {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("unstarted queued destructor");
    }
}
impl spawned_concurrency::tasks::Handler<Queued> for Probe {
    async fn handle(
        &mut self,
        _: Queued,
        _reply: spawned_concurrency::PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
        {
            panic!("unstarted handler ran");
        }
    }
}
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

// Not simulated: blocks the group's OS worker on purpose.
#[cfg(not(spawned_sim))]
#[spawned_macros::sim_test(flavor = "current_thread")]
/// A managed actor stopped (`ChildHandle::stop`) before its first poll: the
/// stop waits for `started()` (OTP `gen_server:stop`), the start succeeds,
/// then the actor exits through `stopped()` without handling its queue.
async fn managed_actor_stopped_before_first_poll_starts_then_stops() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("startup-gate", 1).unwrap();
    let owner = Probe(Arc::default()).with_options(&group).spawn().unwrap();
    spawned_rt::tasks::timeout(LIMIT, owner.ready())
        .await
        .unwrap()
        .unwrap()
        .started()
        .expect("started");
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
    spawned_rt::tasks::timeout(LIMIT, arrival)
        .await
        .unwrap()
        .unwrap();
    let counts = Arc::new(Counts::default());
    let actor = Probe(counts.clone()).with_options(&group).spawn().unwrap();
    let dropped = Arc::new(AtomicUsize::new(0));
    let replies: Vec<_> = (0..2)
        .map(|_| {
            actor
                .send_request(
                    Queued(dropped.clone()),
                    spawned_concurrency::DEFAULT_REQUEST_TIMEOUT,
                )
                .unwrap()
        })
        .collect();
    actor
        .child_handle()
        .stop_with_reason(spawned_concurrency::ExitReason::Shutdown);
    release.send(()).unwrap();
    spawned_rt::tasks::timeout(LIMIT, blocker.join())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        spawned_rt::tasks::timeout(LIMIT, actor.wait_exit())
            .await
            .unwrap(),
        ExitReason::Shutdown
    );
    assert!(matches!(
        spawned_rt::tasks::timeout(LIMIT, actor.ready())
            .await
            .unwrap(),
        Ok(spawned_concurrency::StartOutcome::Started(()))
    ));
    assert_eq!(counts.started.load(Ordering::SeqCst), 1);
    assert_eq!(counts.stopped.load(Ordering::SeqCst), 1);
    assert_eq!(counts.dropped.load(Ordering::SeqCst), 1);
    assert_eq!(dropped.load(Ordering::SeqCst), 2);
    assert_eq!(
        actor.child_handle().cleanup_failure().as_deref(),
        Some("unstarted queued destructor")
    );
    for reply in replies {
        assert!(spawned_rt::tasks::timeout(LIMIT, reply)
            .await
            .unwrap()
            .is_err());
    }
    assert!(system.shutdown(LIMIT).await.complete);
}

#[spawned_macros::sim_test(flavor = "current_thread")]
/// An actor stopped before its first poll runs both hooks: the stop waits
/// for `started()` (OTP `gen_server:stop`; unified start 2.3).
async fn actor_stopped_before_first_poll_keeps_both_hooks() {
    let counts = Arc::new(Counts::default());
    let actor = Probe(counts.clone()).spawn(); // this runtime has not yielded
    actor
        .child_handle()
        .stop_with_reason(spawned_concurrency::ExitReason::Shutdown);
    assert_eq!(
        spawned_rt::tasks::timeout(LIMIT, actor.wait_exit())
            .await
            .unwrap(),
        ExitReason::Shutdown
    );
    assert_eq!(counts.started.load(Ordering::SeqCst), 1);
    assert_eq!(counts.stopped.load(Ordering::SeqCst), 1);
    assert_eq!(counts.dropped.load(Ordering::SeqCst), 1);
}
