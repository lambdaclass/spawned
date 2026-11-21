//! V3 for a OneForAll/RestForOne round: a collateral child the round stops
//! that has not reached its death point `shutdown_timeout` after its signal
//! is killed (`Killed`, design E3), and its slot is stuck (failing the tree)
//! only if it also misses a second `shutdown_timeout` after the kill.
//!
//! The collateral child's handler awaits a release that the test controls
//! (it cannot be preempted, L3), so the timing of the kill and of the
//! stuck decision is observable. Seeded and in virtual time under
//! `--cfg spawned_sim`; once on Tokio otherwise.
#![cfg(feature = "async")]

use spawned_concurrency::{
    message::Message,
    supervision::{Backoff, ChildSpec, SupervisorConfig, SupervisorStrategy},
    system::ActorSystem,
    tasks::{Actor, Context, Handler},
    ExitReason, PendingReply,
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

const SHUTDOWN: Duration = Duration::from_millis(100);
const LIMIT: Duration = Duration::from_secs(5);

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
/// Awaits the shared gate: a turn that will not return until released.
struct Hold;
impl Message for Hold {
    type Result = ();
}

type Gate = Arc<Mutex<Option<spawned_rt::tasks::oneshot::Receiver<()>>>>;

struct Worker {
    gate: Gate,
}
impl Actor for Worker {}
impl Handler<Crash> for Worker {
    async fn handle(&mut self, _: Crash, _: PendingReply<'_, ()>, _: &Context<Self>) {
        panic!("crash for a round");
    }
}
impl Handler<Hold> for Worker {
    async fn handle(&mut self, _: Hold, reply: PendingReply<'_, ()>, _: &Context<Self>) {
        let gate = self.gate.lock().unwrap().take();
        if let Some(gate) = gate {
            let _ = gate.await;
        }
        reply.reply(());
    }
}

/// Start `origin` (index 0, which crashes) and `held` (index 1, whose turn
/// is held), run the round, and return what the held child ended with and
/// whether the supervisor failed, plus the virtual time the round's
/// outcome took. `release_after`: when the held turn is let go (None:
/// never within the round).
async fn round(
    strategy: SupervisorStrategy,
    release_after: Option<Duration>,
) -> (ExitReason, Option<String>, Duration) {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("collateral", 2).unwrap();
    let mut builder = system.supervisor(config(strategy)).unwrap();
    let origin = builder
        .add(
            ChildSpec::new("origin", &group, |_| async {
                Ok::<_, String>(Worker {
                    gate: Arc::new(Mutex::new(None)),
                })
            })
            .shutdown_timeout(SHUTDOWN),
        )
        .unwrap();
    let (release, gate) = spawned_rt::tasks::oneshot::channel();
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
                async move { Ok::<_, String>(Worker { gate }) }
            })
            .shutdown_timeout(SHUTDOWN),
        )
        .unwrap();
    let supervisor = builder.start().await.unwrap();
    let held_first = held.wait_ready().await.unwrap();
    let held_handle = held_first.child_handle();
    held_first.send(Hold).unwrap();
    spawned_rt::tasks::sleep(Duration::from_millis(1)).await;
    let began = spawned_rt::time::Instant::now();
    origin.wait_ready().await.unwrap().send(Crash).unwrap();
    // RestForOne stops the children after the origin: make `held` later.
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
    drop(system);
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
        let (reason, failure, _) = round(strategy, Some(SHUTDOWN * 3 / 2)).await;
        assert_eq!(reason, ExitReason::Killed, "{strategy:?}");
        assert_eq!(
            failure, None,
            "{strategy:?}: the round gave up after one interval"
        );
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
        let (reason, failure, took) = round(strategy, None).await;
        assert!(failure.is_some(), "{strategy:?}");
        assert!(took >= SHUTDOWN * 2, "{strategy:?}: stuck after {took:?}");
        assert_eq!(reason, ExitReason::Killed, "{strategy:?}");
    }
}
