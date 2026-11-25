//! V3 for a OneForAll/RestForOne round: a collateral child the round stops
//! that has not reached its death point `shutdown_timeout` after its signal
//! is killed (`Killed`, design E3), and its slot is stuck (failing the tree)
//! only if it also misses a second `shutdown_timeout` after the kill. Also
//! for a trapping child, which queues the round's parent exit signal behind
//! its held turn: its deadline still starts at the round's stop. The same
//! holds for a OneForOne supervisor's shutdown, whose own wait is a
//! backstop beyond the kill.
//!
//! The collateral child's handler awaits a release that the test controls
//! (it cannot be preempted, L3), so the timing of the kill and of the
//! stuck decision is observable. Seeded and in virtual time under
//! `--cfg spawned_sim`; once on Tokio otherwise, with wider intervals. A
//! kill in flight is not observable without the lifecycle trace, so the
//! release is timed half way between the kill and the stuck decision;
//! `cohort_trace` waits on the recorded kill instead.
#![cfg(feature = "async")]

use spawned_concurrency::{
    message::Message,
    supervision::{Backoff, ChildSpec, SupervisorConfig, SupervisorStrategy},
    system::ActorSystem,
    tasks::{Actor, Context, Handler},
    ChildHandle, ExitReason, PendingReply,
};
use spawned_rt::tasks::oneshot;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

/// Virtual time under sim; on Tokio, wide enough that scheduling lag does
/// not move the release out of the window between the kill and the stuck
/// decision.
#[cfg(spawned_sim)]
const SHUTDOWN: Duration = Duration::from_millis(100);
#[cfg(not(spawned_sim))]
const SHUTDOWN: Duration = Duration::from_millis(400);
const LIMIT: Duration = Duration::from_secs(10);

fn config(strategy: SupervisorStrategy) -> SupervisorConfig {
    SupervisorConfig::default()
        .strategy(strategy)
        .max_restarts(5)
        .backoff(
            Backoff::default()
                .initial(Duration::from_millis(1))
                .maximum(Duration::from_millis(2))
                .jitter(false)
                .seed(7),
        )
}

struct Crash;
impl Message for Crash {
    type Result = ();
}
/// Awaits the shared gate: a turn that will not return until released. The
/// sender is answered once the turn holds the gate.
struct Hold(oneshot::Sender<()>);
impl Message for Hold {
    type Result = ();
}

type Gate = Arc<Mutex<Option<oneshot::Receiver<()>>>>;

struct Worker {
    gate: Gate,
    trap: bool,
}
impl Actor for Worker {
    async fn started(&mut self, ctx: &Context<Self>) {
        ctx.trap_exit(self.trap);
    }
}
impl Handler<Crash> for Worker {
    async fn handle(&mut self, _: Crash, _: PendingReply<'_, ()>, _: &Context<Self>) {
        panic!("crash for a round");
    }
}
impl Handler<Hold> for Worker {
    async fn handle(
        &mut self,
        Hold(entered): Hold,
        reply: PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
        let gate = self.gate.lock().unwrap().take();
        let _ = entered.send(());
        if let Some(gate) = gate {
            let _ = gate.await;
        }
        reply.reply(());
    }
}

/// Start `origin` (index 0, which crashes) and `held` (index 1, after the
/// origin, so RestForOne stops it too; its turn is held), run the round,
/// and return what the held child ended with and whether the supervisor
/// failed, plus the virtual time the round's outcome took.
/// `release_after`: when the held turn is let go (None: never within the
/// round). `trap`: the held child traps exits.
async fn round(
    strategy: SupervisorStrategy,
    release_after: Option<Duration>,
    trap: bool,
) -> (ExitReason, Option<String>, Duration) {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("collateral", 2).unwrap();
    let mut builder = system.supervisor(config(strategy)).unwrap();
    let origin = builder
        .add(
            ChildSpec::new("origin", &group, |_| async {
                Ok::<_, String>(Worker {
                    gate: Arc::new(Mutex::new(None)),
                    trap: false,
                })
            })
            .shutdown_timeout(SHUTDOWN),
        )
        .unwrap();
    let (release, gate) = oneshot::channel();
    let gate: Gate = Arc::new(Mutex::new(Some(gate)));
    let first_gate = gate.clone();
    let held = builder
        .add(
            ChildSpec::new("held", &group, move |ctx| {
                // Only the first generation is held.
                let gate = if ctx.generation() == 1 {
                    first_gate.clone()
                } else {
                    Arc::new(Mutex::new(None))
                };
                async move { Ok::<_, String>(Worker { gate, trap }) }
            })
            .shutdown_timeout(SHUTDOWN),
        )
        .unwrap();
    let supervisor = builder.start().await.unwrap();
    let held_first = held.wait_ready().await.unwrap();
    let held_handle = held_first.child_handle();
    let (entered, holding) = oneshot::channel();
    held_first.send(Hold(entered)).unwrap();
    spawned_rt::tasks::timeout(LIMIT, holding)
        .await
        .expect("the held turn never began")
        .unwrap();
    let began = spawned_rt::time::Instant::now();
    origin.wait_ready().await.unwrap().send(Crash).unwrap();
    let mut release = Some(release);
    let outcome = loop {
        if let (Some(after), Some(_)) = (release_after, release.as_ref()) {
            if began.elapsed() >= after {
                let _ = release.take().unwrap().send(());
            }
        }
        if let Some(failure) = supervisor.failure() {
            break Some(failure);
        }
        if held.snapshot().generation > 1 && held.current().is_ok() {
            break None;
        }
        assert!(
            began.elapsed() < LIMIT,
            "the round neither restarted nor failed"
        );
        spawned_rt::tasks::sleep(Duration::from_millis(5)).await;
    };
    let took = began.elapsed();
    // Let a still-held turn go, so the first generation can end.
    if let Some(release) = release.take() {
        let _ = release.send(());
    }
    let reason = spawned_rt::tasks::timeout(LIMIT, held_handle.wait_exit_async())
        .await
        .expect("the held child never exited");
    let _ = system.shutdown(LIMIT).await;
    (reason, outcome, took)
}

/// Released between the kill and the second interval: the child ends
/// `Killed` (not the round's `Shutdown`) and the round restarts the cohort.
#[spawned_concurrency::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn collateral_child_is_killed_at_shutdown_timeout() {
    for strategy in [
        SupervisorStrategy::OneForAll,
        SupervisorStrategy::RestForOne,
    ] {
        for trap in [false, true] {
            let (reason, failure, _) = round(strategy, Some(SHUTDOWN * 3 / 2), trap).await;
            assert_eq!(reason, ExitReason::Killed, "{strategy:?} trap={trap}");
            assert_eq!(
                failure, None,
                "{strategy:?} trap={trap}: the round gave up before the kill"
            );
        }
    }
}

/// Never released within the round: the slot is stuck and the tree fails,
/// but only after the kill and a second interval, and the child's final
/// reason is the kill's.
#[spawned_concurrency::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn collateral_child_is_stuck_only_after_the_kill_and_a_second_interval() {
    for strategy in [
        SupervisorStrategy::OneForAll,
        SupervisorStrategy::RestForOne,
    ] {
        for trap in [false, true] {
            let (reason, failure, took) = round(strategy, None, trap).await;
            // Stuck by the child's controller after its kill, not given up
            // on by the round coordinator's backstop.
            let failure = failure.unwrap_or_default();
            assert!(
                failure.contains("actor cleanup stuck"),
                "{strategy:?} trap={trap}: {failure:?}"
            );
            assert!(
                took >= SHUTDOWN * 2,
                "{strategy:?} trap={trap}: stuck after {took:?}"
            );
            assert_eq!(reason, ExitReason::Killed, "{strategy:?} trap={trap}");
        }
    }
}

/// The same in a supervisor's shutdown (OneForOne): a stopped child released
/// after the kill, within the second interval, ends `Killed` and the
/// shutdown completes without failing the tree. The shutdown loop's own wait
/// used to give up at one `shutdown_timeout`, racing the kill. A trapping
/// child queues the shutdown behind its held turn; its deadline still starts
/// at the stop turn.
#[spawned_concurrency::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_kills_a_stopped_child_without_failing_the_tree() {
    for trap in [false, true] {
        let system = ActorSystem::new().unwrap();
        let group = system.async_group("shutdown-kill", 2).unwrap();
        let mut builder = system
            .supervisor(config(SupervisorStrategy::OneForOne))
            .unwrap();
        let (release, gate) = oneshot::channel();
        let gate: Gate = Arc::new(Mutex::new(Some(gate)));
        let held = builder
            .add(
                ChildSpec::new("held", &group, move |_| {
                    let gate = gate.clone();
                    async move { Ok::<_, String>(Worker { gate, trap }) }
                })
                .shutdown_timeout(SHUTDOWN),
            )
            .unwrap();
        let supervisor = builder.start().await.unwrap();
        let actor = held.wait_ready().await.unwrap();
        let handle = actor.child_handle();
        let (entered, holding) = oneshot::channel();
        actor.send(Hold(entered)).unwrap();
        spawned_rt::tasks::timeout(LIMIT, holding)
            .await
            .expect("the held turn never began")
            .unwrap();
        let releaser = spawned_rt::tasks::spawn(async move {
            // After the kill, before the second interval ends.
            spawned_rt::tasks::sleep(SHUTDOWN * 3 / 2).await;
            let _ = release.send(());
        });
        let report = supervisor.shutdown(LIMIT).await;
        let _ = releaser.await;
        assert!(report.complete, "trap={trap}");
        assert_eq!(
            supervisor.failure(),
            None,
            "trap={trap}: the shutdown gave up before the kill"
        );
        let reason = spawned_rt::tasks::timeout(LIMIT, handle.wait_exit_async())
            .await
            .expect("the held child never exited");
        assert_eq!(reason, ExitReason::Killed, "trap={trap}");
        assert!(system.shutdown(LIMIT).await.complete, "trap={trap}");
    }
}

/// A generation whose `started()` waits on a gate; it reports its handle
/// once `started()` holds the gate.
struct SlowStart {
    gate: Gate,
    entered: Option<oneshot::Sender<ChildHandle>>,
}
impl Actor for SlowStart {
    async fn started(&mut self, ctx: &Context<Self>) {
        let gate = self.gate.lock().unwrap().take();
        if let Some(entered) = self.entered.take() {
            let _ = entered.send(ctx.actor_ref().child_handle());
        }
        if let Some(gate) = gate {
            let _ = gate.await;
        }
    }
}
impl Handler<Crash> for SlowStart {
    async fn handle(&mut self, _: Crash, _: PendingReply<'_, ()>, _: &Context<Self>) {
        panic!("crash for a restart");
    }
}

/// A supervisor shut down while a restarted generation is still in
/// `started()`: the generation is never published, and its startup is given
/// up when admission closes. Returns its final reason, the supervisor's
/// failure and the virtual time from the shutdown to the outcome.
/// `release_after`: when `started()` is let go (None: never).
async fn unpublished(release_after: Option<Duration>) -> (ExitReason, Option<String>, Duration) {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("unpublished", 2).unwrap();
    let mut builder = system
        .supervisor(config(SupervisorStrategy::OneForOne))
        .unwrap();
    let (release, gate) = oneshot::channel();
    let gate: Gate = Arc::new(Mutex::new(Some(gate)));
    let (entered, entered_rx) = oneshot::channel();
    let entered = Arc::new(Mutex::new(Some(entered)));
    let held = builder
        .add(
            ChildSpec::new("held", &group, move |ctx| {
                // The second generation is held in started().
                let (gate, entered) = if ctx.generation() == 2 {
                    (gate.clone(), entered.lock().unwrap().take())
                } else {
                    (Arc::new(Mutex::new(None)), None)
                };
                async move { Ok::<_, String>(SlowStart { gate, entered }) }
            })
            .startup_timeout(LIMIT)
            .shutdown_timeout(SHUTDOWN),
        )
        .unwrap();
    let supervisor = builder.start().await.unwrap();
    held.wait_ready().await.unwrap().send(Crash).unwrap();
    let handle = spawned_rt::tasks::timeout(LIMIT, entered_rx)
        .await
        .expect("the second generation never reached started()")
        .unwrap();
    let began = spawned_rt::time::Instant::now();
    let shutdown = {
        let supervisor = supervisor.clone();
        spawned_rt::tasks::spawn(async move { supervisor.shutdown(LIMIT).await })
    };
    let mut release = Some(release);
    let outcome = loop {
        if let (Some(after), Some(_)) = (release_after, release.as_ref()) {
            if began.elapsed() >= after {
                let _ = release.take().unwrap().send(());
            }
        }
        if let Some(failure) = supervisor.failure() {
            break Some(failure);
        }
        if handle.exit_reason().is_some() {
            break None;
        }
        assert!(
            began.elapsed() < LIMIT,
            "the generation neither ended nor was stuck"
        );
        spawned_rt::tasks::sleep(Duration::from_millis(5)).await;
    };
    let took = began.elapsed();
    if let Some(release) = release.take() {
        let _ = release.send(());
    }
    let _ = shutdown.await;
    let reason = spawned_rt::tasks::timeout(LIMIT, handle.wait_exit_async())
        .await
        .expect("the held generation never exited");
    let _ = system.shutdown(LIMIT).await;
    (reason, outcome, took)
}

/// V3 for a generation stopped before it was published: released after the
/// kill, within the second interval, it ends `Killed` and the shutdown does
/// not fail the tree.
#[spawned_concurrency::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn unpublished_generation_is_killed_at_shutdown_timeout() {
    let (reason, failure, _) = unpublished(Some(SHUTDOWN * 3 / 2)).await;
    assert_eq!(reason, ExitReason::Killed);
    assert_eq!(
        failure, None,
        "the unpublished generation was stuck without its kill"
    );
}

/// Never released: stuck (failing the tree) only after the kill and a
/// second interval, and its final reason is the kill's.
#[spawned_concurrency::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn unpublished_generation_is_stuck_only_after_the_kill() {
    let (reason, failure, took) = unpublished(None).await;
    let failure = failure.unwrap_or_default();
    assert!(failure.contains("cleanup stuck"), "{failure:?}");
    assert!(took >= SHUTDOWN * 2, "stuck after {took:?}");
    assert_eq!(reason, ExitReason::Killed);
}
