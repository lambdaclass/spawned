#![cfg(feature = "async")]
// Tests that need OS threads are compiled out under spawned_sim.
#![cfg_attr(spawned_sim, allow(unused_imports, dead_code))]
use spawned_concurrency::{
    message::Message,
    supervision::{Backoff, ChildSpec, RestartMode, SupervisorConfig, SupervisorStrategy},
    system::ActorSystem,
    tasks::{Actor, Context, Handler},
};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
const LIMIT: Duration = Duration::from_secs(5);
fn policy(strategy: SupervisorStrategy) -> SupervisorConfig {
    SupervisorConfig {
        strategy,
        max_restarts: 100,
        backoff: Backoff {
            initial: Duration::from_millis(1),
            maximum: Duration::from_millis(4),
            jitter: false,
            ..Backoff::default()
        },
        ..SupervisorConfig::default()
    }
}
struct Next;
impl Message for Next {
    type Result = usize;
}
struct Crash;
impl Message for Crash {
    type Result = ();
}
/// Both children panic only after both handlers are running. The rendezvous
/// is async: a std barrier would block a worker, and a task parked in that
/// worker's LIFO slot cannot be stolen, so the other child might never run.
struct CrashTogether(Arc<tokio::sync::Barrier>);
impl Message for CrashTogether {
    type Result = ();
}
struct Probe {
    index: usize,
    generation: usize,
    value: usize,
    live: Arc<Vec<AtomicUsize>>,
    events: Arc<Mutex<Vec<String>>>,
}
impl Actor for Probe {
    async fn started(&mut self, _: &Context<Self>) {
        self.events
            .lock()
            .unwrap()
            .push(format!("start:{}:{}", self.index, self.generation));
    }
    async fn stopped(&mut self, _: &Context<Self>) {
        self.events
            .lock()
            .unwrap()
            .push(format!("stop:{}:{}", self.index, self.generation));
    }
}
impl Drop for Probe {
    fn drop(&mut self) {
        assert_eq!(self.live[self.index].fetch_sub(1, Ordering::SeqCst), 1);
    }
}
impl Handler<Next> for Probe {
    async fn handle(
        &mut self,
        _: Next,
        reply: spawned_concurrency::reply::PendingReply<'_, usize>,
        _: &Context<Self>,
    ) {
        let __reply_value: usize = {
            self.value += 1;
            self.value
        };
        let _ = reply.reply(__reply_value);
    }
}
impl Handler<Crash> for Probe {
    async fn handle(
        &mut self,
        _: Crash,
        reply: spawned_concurrency::reply::PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
        {
            panic!("injected cohort failure");
        }
        let _ = reply.reply(());
    }
}
impl Handler<CrashTogether> for Probe {
    async fn handle(
        &mut self,
        msg: CrashTogether,
        reply: spawned_concurrency::reply::PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
        {
            msg.0.wait().await;
            panic!("simultaneous cohort failure");
        }
        let _ = reply.reply(());
    }
}

#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn strategies_select_affected_children_and_reconstruct_in_dependency_order() {
    for strategy in [
        SupervisorStrategy::OneForOne,
        SupervisorStrategy::OneForAll,
        SupervisorStrategy::RestForOne,
    ] {
        let system = ActorSystem::new().unwrap();
        let group = system.async_group("strategy-selection", 2).unwrap();
        let counts = Arc::new((0..3).map(|_| AtomicUsize::new(0)).collect::<Vec<_>>());
        let live = Arc::new((0..3).map(|_| AtomicUsize::new(0)).collect::<Vec<_>>());
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut tree = system.supervisor(policy(strategy)).unwrap();
        let mut children = Vec::new();
        for index in 0..3 {
            let counts = counts.clone();
            let live = live.clone();
            let events = events.clone();
            children.push(
                tree.add(ChildSpec::new(
                    format!("child-{index}"),
                    &group,
                    move |_| {
                        assert_eq!(
                            live[index].fetch_add(1, Ordering::SeqCst),
                            0,
                            "overlapping generation"
                        );
                        let generation = counts[index].fetch_add(1, Ordering::SeqCst) + 1;
                        let live = live.clone();
                        let events = events.clone();
                        async move {
                            Ok::<_, String>(Probe {
                                index,
                                generation,
                                value: 0,
                                live,
                                events,
                            })
                        }
                    },
                ))
                .unwrap(),
            );
        }
        let supervisor = tree.start().await.unwrap();
        let old: Vec<_> = children
            .iter()
            .map(|child| child.current().unwrap())
            .collect();
        for actor in &old {
            assert_eq!(actor.request(Next).await.unwrap(), 1);
        }
        assert!(old[1].request(Crash).await.is_err());
        for index in 0..3 {
            let affected = match strategy {
                SupervisorStrategy::OneForOne => index == 1,
                SupervisorStrategy::OneForAll => true,
                SupervisorStrategy::RestForOne => index >= 1,
            };
            if affected {
                let next =
                    spawned_rt::tasks::timeout(LIMIT, children[index].wait_for_generation(1))
                        .await
                        .unwrap()
                        .unwrap();
                assert_ne!(next.id(), old[index].id());
                assert_eq!(next.request(Next).await.unwrap(), 1);
            } else {
                assert_eq!(children[index].current().unwrap().id(), old[index].id());
                assert_eq!(old[index].request(Next).await.unwrap(), 2);
            }
        }
        if strategy != SupervisorStrategy::OneForOne {
            let events = events.lock().unwrap();
            let starts: Vec<_> = events
                .iter()
                .filter(|s| s.starts_with("start:") && s.ends_with(":2"))
                .cloned()
                .collect();
            let expected = if strategy == SupervisorStrategy::OneForAll {
                vec!["start:0:2", "start:1:2", "start:2:2"]
            } else {
                vec!["start:1:2", "start:2:2"]
            };
            assert_eq!(starts, expected);
            let last_stop = events
                .iter()
                .rposition(|s| s.starts_with("stop:") && s.ends_with(":1"))
                .unwrap();
            let first_start = events
                .iter()
                .position(|s| s.starts_with("start:") && s.ends_with(":2"))
                .unwrap();
            assert!(
                last_stop < first_start,
                "reconstructed before cohort cleanup: {events:?}"
            );
        }
        assert!(supervisor.shutdown(LIMIT).await.complete);
        assert!(system.shutdown(LIMIT).await.complete);
        assert!(live.iter().all(|count| count.load(Ordering::SeqCst) == 0));
    }
}

#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn temporary_collateral_child_stops_without_reconstruction() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("temporary-collateral", 1).unwrap();
    let counts = Arc::new((0..2).map(|_| AtomicUsize::new(0)).collect::<Vec<_>>());
    let live = Arc::new((0..2).map(|_| AtomicUsize::new(0)).collect::<Vec<_>>());
    let events = Arc::new(Mutex::new(Vec::new()));
    let mut tree = system
        .supervisor(policy(SupervisorStrategy::OneForAll))
        .unwrap();
    let mut children = Vec::new();
    for index in 0..2 {
        let counts = counts.clone();
        let live = live.clone();
        let events = events.clone();
        children.push(
            tree.add(
                ChildSpec::new(format!("child-{index}"), &group, move |_| {
                    live[index].fetch_add(1, Ordering::SeqCst);
                    let generation = counts[index].fetch_add(1, Ordering::SeqCst) + 1;
                    let live = live.clone();
                    let events = events.clone();
                    async move {
                        Ok::<_, String>(Probe {
                            index,
                            generation,
                            value: 0,
                            live,
                            events,
                        })
                    }
                })
                .restart(if index == 1 {
                    RestartMode::Temporary
                } else {
                    RestartMode::Permanent
                }),
            )
            .unwrap(),
        );
    }
    let supervisor = tree.start().await.unwrap();
    assert!(children[0].current().unwrap().request(Crash).await.is_err());
    spawned_rt::tasks::timeout(LIMIT, children[0].wait_for_generation(1))
        .await
        .unwrap()
        .unwrap();
    assert!(
        spawned_rt::tasks::timeout(LIMIT, children[1].wait_for_generation(1))
            .await
            .unwrap()
            .is_err()
    );
    assert_eq!(counts[1].load(Ordering::SeqCst), 1);
    assert!(supervisor.shutdown(LIMIT).await.complete);
    assert!(system.shutdown(LIMIT).await.complete);
}

#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_cohort_construction_rebuilds_the_whole_cohort_like_otp() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("cohort-factory", 1).unwrap();
    let counts = Arc::new((0..3).map(|_| AtomicUsize::new(0)).collect::<Vec<_>>());
    let live = Arc::new((0..3).map(|_| AtomicUsize::new(0)).collect::<Vec<_>>());
    let events = Arc::new(Mutex::new(Vec::new()));
    let mut tree = system
        .supervisor(policy(SupervisorStrategy::OneForAll))
        .unwrap();
    let mut children = Vec::new();
    for index in 0..3 {
        let counts = counts.clone();
        let live = live.clone();
        let events = events.clone();
        children.push(
            tree.add(ChildSpec::new(
                format!("child-{index}"),
                &group,
                move |_| {
                    let generation = counts[index].fetch_add(1, Ordering::SeqCst) + 1;
                    let live = live.clone();
                    let events = events.clone();
                    async move {
                        if index == 1 && generation == 2 {
                            return Err("injected factory error".to_string());
                        }
                        assert_eq!(live[index].fetch_add(1, Ordering::SeqCst), 0);
                        Ok(Probe {
                            index,
                            generation,
                            value: 0,
                            live,
                            events,
                        })
                    }
                },
            ))
            .unwrap(),
        );
    }
    let supervisor = tree.start().await.unwrap();
    assert!(children[0].current().unwrap().request(Crash).await.is_err());
    spawned_rt::tasks::timeout(LIMIT, children[2].wait_for_generation(1))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        counts
            .iter()
            .map(|n| n.load(Ordering::SeqCst))
            .collect::<Vec<_>>(),
        // child-1's failed replacement starts a new OneForAll round, so
        // child-0 is rebuilt again (OTP try_again_restart re-runs the
        // strategy): 0 and 1 three times, 2 twice (its first round never
        // reached it).
        vec![3, 3, 2]
    );
    assert!(supervisor.shutdown(LIMIT).await.complete);
    assert!(system.shutdown(LIMIT).await.complete);
}

#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn cohort_reconstruction_consumes_shared_factory_attempt_budget() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("cohort-budget", 1).unwrap();
    let mut config = policy(SupervisorStrategy::OneForAll);
    config.max_restarts = 2;
    let mut tree = system.supervisor(config).unwrap();
    let mut children = Vec::new();
    let live = Arc::new((0..3).map(|_| AtomicUsize::new(0)).collect::<Vec<_>>());
    let events = Arc::new(Mutex::new(Vec::new()));
    for index in 0..3 {
        let live = live.clone();
        let events = events.clone();
        children.push(
            tree.add(ChildSpec::new(
                format!("child-{index}"),
                &group,
                move |ctx| {
                    let live = live.clone();
                    let events = events.clone();
                    async move {
                        assert_eq!(live[index].fetch_add(1, Ordering::SeqCst), 0);
                        Ok::<_, String>(Probe {
                            index,
                            generation: ctx.generation() as usize,
                            value: 0,
                            live,
                            events,
                        })
                    }
                },
            ))
            .unwrap(),
        );
    }
    let supervisor = tree.start().await.unwrap();
    assert!(children[0].current().unwrap().request(Crash).await.is_err());
    spawned_rt::tasks::timeout(LIMIT, supervisor.join())
        .await
        .unwrap();
    assert!(supervisor.failure().unwrap().contains("budget exhausted"));
    assert!(system.shutdown(LIMIT).await.complete);
    assert!(live.iter().all(|n| n.load(Ordering::SeqCst) == 0));
}

// Not simulated: children meet at a std::sync::Barrier, which needs OS threads.
#[cfg(not(spawned_sim))]
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn simultaneous_failures_and_shutdown_do_not_overlap_cohorts_or_generations() {
    for strategy in [
        SupervisorStrategy::OneForAll,
        SupervisorStrategy::RestForOne,
    ] {
        for _ in 0..16 {
            let system = ActorSystem::new().unwrap();
            let group = system.async_group("simultaneous-cohorts", 2).unwrap();
            let live = Arc::new((0..2).map(|_| AtomicUsize::new(0)).collect::<Vec<_>>());
            let events = Arc::new(Mutex::new(Vec::new()));
            let mut tree = system.supervisor(policy(strategy)).unwrap();
            let mut children = Vec::new();
            for index in 0..2 {
                let live = live.clone();
                let events = events.clone();
                children.push(
                    tree.add(ChildSpec::new(
                        format!("child-{index}"),
                        &group,
                        move |ctx| {
                            let live = live.clone();
                            let events = events.clone();
                            async move {
                                assert_eq!(
                                    live[index].fetch_add(1, Ordering::SeqCst),
                                    0,
                                    "overlapping cohort generations"
                                );
                                Ok::<_, String>(Probe {
                                    index,
                                    generation: ctx.generation() as usize,
                                    value: 0,
                                    live,
                                    events,
                                })
                            }
                        },
                    ))
                    .unwrap(),
                );
            }
            let supervisor = tree.start().await.unwrap();
            let barrier = Arc::new(tokio::sync::Barrier::new(2));
            for child in &children {
                child
                    .current()
                    .unwrap()
                    .send(CrashTogether(barrier.clone()))
                    .unwrap();
            }
            for child in &children {
                spawned_rt::tasks::timeout(LIMIT, child.wait_for_generation(1))
                    .await
                    .unwrap()
                    .unwrap();
            }
            assert!(supervisor.shutdown(LIMIT).await.complete);
            assert!(system.shutdown(LIMIT).await.complete);
            assert!(live.iter().all(|n| n.load(Ordering::SeqCst) == 0));
        }
    }
}

// Not simulated: uses spawn_blocking.
#[cfg(not(spawned_sim))]
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn blocked_collateral_helper_prevents_entire_cohort_reconstruction() {
    for strategy in [
        SupervisorStrategy::OneForAll,
        SupervisorStrategy::RestForOne,
    ] {
        let system = ActorSystem::new().unwrap();
        let group = system.async_group("cohort-helper-barrier", 2).unwrap();
        let counts = Arc::new((0..2).map(|_| AtomicUsize::new(0)).collect::<Vec<_>>());
        let live = Arc::new((0..2).map(|_| AtomicUsize::new(0)).collect::<Vec<_>>());
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut tree = system.supervisor(policy(strategy)).unwrap();
        let mut children = Vec::new();
        for index in 0..2 {
            let counts = counts.clone();
            let live = live.clone();
            let events = events.clone();
            children.push(
                tree.add(
                    ChildSpec::new(format!("slot-{index}"), &group, move |_| {
                        let generation = counts[index].fetch_add(1, Ordering::SeqCst) + 1;
                        assert_eq!(live[index].fetch_add(1, Ordering::SeqCst), 0);
                        let live = live.clone();
                        let events = events.clone();
                        async move {
                            Ok::<_, String>(Probe {
                                index,
                                generation,
                                value: 0,
                                live,
                                events,
                            })
                        }
                    })
                    .shutdown_timeout(Duration::from_millis(15)),
                )
                .unwrap(),
            );
        }
        let supervisor = tree.start().await.unwrap();
        let (entered_tx, entered) = tokio::sync::oneshot::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        let collateral = children[1].current().unwrap();
        let helper = collateral
            .context()
            .spawn_blocking_helper(move || {
                let _ = entered_tx.send(());
                release_rx.recv_timeout(LIMIT).unwrap();
            })
            .unwrap();
        spawned_rt::tasks::timeout(LIMIT, entered)
            .await
            .unwrap()
            .unwrap();
        assert!(children[0].current().unwrap().request(Crash).await.is_err());
        let failure = spawned_rt::tasks::timeout(LIMIT, async {
            loop {
                if let Some(failure) = supervisor.failure() {
                    break failure;
                }
                spawned_rt::tasks::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(failure.contains("cleanup"), "{failure}");
        assert!(
            counts.iter().all(|c| c.load(Ordering::SeqCst) == 1),
            "a dependency restarted before collateral cleanup"
        );
        let report = supervisor.shutdown(Duration::from_millis(5)).await;
        assert!(!report.complete);
        release.send(()).unwrap();
        drop(helper);
        assert!(supervisor.shutdown(LIMIT).await.complete);
        assert!(system.shutdown(LIMIT).await.complete);
    }
}

#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn upstream_failure_supersedes_a_round_while_a_dependent_factory_is_pending() {
    for strategy in [
        SupervisorStrategy::OneForAll,
        SupervisorStrategy::RestForOne,
    ] {
        for temporary in [false, true] {
            let system = ActorSystem::new().unwrap();
            let group = system.async_group("superseded-cohort", 2).unwrap();
            let pending = Arc::new(tokio::sync::Notify::new());
            let live = Arc::new((0..3).map(|_| AtomicUsize::new(0)).collect::<Vec<_>>());
            let events = Arc::new(Mutex::new(Vec::new()));
            let mut tree = system.supervisor(policy(strategy)).unwrap();
            let mut children = Vec::new();
            for index in 0..3 {
                let live = live.clone();
                let events = events.clone();
                let pending = pending.clone();
                children.push(
                    tree.add(
                        ChildSpec::new(format!("dependency-{index}"), &group, move |ctx| {
                            let live = live.clone();
                            let events = events.clone();
                            let pending = pending.clone();
                            async move {
                                if index == 1 && ctx.generation() == 2 {
                                    pending.notify_one();
                                    std::future::pending::<()>().await;
                                }
                                assert_eq!(live[index].fetch_add(1, Ordering::SeqCst), 0);
                                Ok::<_, String>(Probe {
                                    index,
                                    generation: ctx.generation() as usize,
                                    value: 0,
                                    live,
                                    events,
                                })
                            }
                        })
                        .startup_timeout(Duration::from_secs(30))
                        .restart(if index == 2 && temporary {
                            RestartMode::Temporary
                        } else {
                            RestartMode::Permanent
                        }),
                    )
                    .unwrap(),
                );
            }
            let supervisor = tree.start().await.unwrap();
            assert!(children[1].current().unwrap().request(Crash).await.is_err());
            spawned_rt::tasks::timeout(LIMIT, pending.notified())
                .await
                .unwrap();
            let generation = children[0].snapshot().generation;
            assert!(children[0].current().unwrap().request(Crash).await.is_err());
            spawned_rt::tasks::timeout(LIMIT, children[0].wait_for_generation(generation))
                .await
                .unwrap()
                .unwrap();
            spawned_rt::tasks::timeout(LIMIT, children[1].wait_for_generation(2))
                .await
                .unwrap()
                .unwrap();
            // This slot was still parked on the superseded round's resume token.
            let tail = spawned_rt::tasks::timeout(LIMIT, children[2].wait_for_generation(1))
                .await
                .unwrap();
            assert_eq!(tail.is_err(), temporary);
            assert!(supervisor.failure().is_none());
            assert!(system.shutdown(LIMIT).await.complete);
        }
    }
}

#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn ready_dependency_failure_rolls_back_slow_initial_tree_startup() {
    for strategy in [
        SupervisorStrategy::OneForAll,
        SupervisorStrategy::RestForOne,
    ] {
        let system = ActorSystem::new().unwrap();
        let group = system.async_group("initial-cohort", 2).unwrap();
        let live = Arc::new((0..3).map(|_| AtomicUsize::new(0)).collect::<Vec<_>>());
        let events = Arc::new(Mutex::new(Vec::new()));
        let entered = Arc::new(tokio::sync::Notify::new());
        let mut tree = system.supervisor(policy(strategy)).unwrap();
        let mut children = Vec::new();
        for index in 0..3 {
            let live = live.clone();
            let events = events.clone();
            let entered = entered.clone();
            children.push(
                tree.add(
                    ChildSpec::new(format!("startup-dependency-{index}"), &group, move |ctx| {
                        let live = live.clone();
                        let events = events.clone();
                        let entered = entered.clone();
                        async move {
                            if index == 2 {
                                entered.notify_one();
                                std::future::pending::<()>().await;
                            }
                            assert_eq!(live[index].fetch_add(1, Ordering::SeqCst), 0);
                            Ok::<_, String>(Probe {
                                index,
                                generation: ctx.generation() as usize,
                                value: 0,
                                live,
                                events,
                            })
                        }
                    })
                    .startup_timeout(Duration::from_secs(30)),
                )
                .unwrap(),
            );
        }
        let startup = spawned_rt::tasks::spawn(tree.start());
        spawned_rt::tasks::timeout(LIMIT, entered.notified())
            .await
            .unwrap();
        assert!(children[0].current().unwrap().request(Crash).await.is_err());
        assert!(spawned_rt::tasks::timeout(LIMIT, startup)
            .await
            .unwrap()
            .unwrap()
            .is_err());
        assert_eq!(
            children[0].snapshot().generation,
            1,
            "must not replace the dependency alone"
        );
        assert!(system.shutdown(LIMIT).await.complete);
        assert!(live.iter().all(|n| n.load(Ordering::SeqCst) == 0));
    }
}

/// A replacement that stops `Normal` before readiness inside a recovery round
/// is a failed restart: OneForAll rebuilds the whole cohort again (A, already
/// ready in the round, is stopped and replaced), RestForOne only the failed
/// child and its later siblings, as OTP re-runs the strategy on
/// `try_again_restart`.
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn replacement_stopping_before_readiness_restarts_the_strategy_scope() {
    for (strategy, a_generation) in [
        (SupervisorStrategy::OneForAll, 3),
        (SupervisorStrategy::RestForOne, 1),
    ] {
        let system = ActorSystem::new().unwrap();
        let group = system.async_group("cohort-pre-ready", 1).unwrap();
        let mut tree = system.supervisor(policy(strategy)).unwrap();
        let a = tree
            .add(ChildSpec::new("a", &group, |_| async {
                Ok::<_, String>(Plain)
            }))
            .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let b = tree
            .add(
                ChildSpec::new("b", &group, move |ctx| {
                    count.fetch_add(1, Ordering::SeqCst);
                    let stop = ctx.generation() == 2;
                    async move { Ok::<_, String>(MaybeStop(stop)) }
                })
                .restart(RestartMode::Transient),
            )
            .unwrap();
        let supervisor = tree.start().await.unwrap();
        // B crashes: OneForAll rebuilds A and B (A2 ready, B2 stops Normal
        // before readiness); RestForOne rebuilds only B.
        assert!(b.current().unwrap().request(CrashPlain).await.is_err());
        spawned_rt::tasks::timeout(LIMIT, b.wait_for_generation(2))
            .await
            .unwrap()
            .unwrap();
        assert!(supervisor.failure().is_none(), "{strategy:?}");
        assert_eq!(b.snapshot().generation, 3, "{strategy:?}");
        assert_eq!(calls.load(Ordering::SeqCst), 3, "{strategy:?}");
        assert_eq!(a.snapshot().generation, a_generation, "{strategy:?}");
        assert!(supervisor.shutdown(LIMIT).await.complete);
        assert!(system.shutdown(LIMIT).await.complete);
    }
}

struct Plain;
impl Actor for Plain {}
struct CrashPlain;
impl Message for CrashPlain {
    type Result = ();
}
struct MaybeStop(bool);
impl Actor for MaybeStop {
    async fn started(&mut self, ctx: &Context<Self>) {
        if self.0 {
            ctx.stop();
        }
    }
}
impl Handler<CrashPlain> for MaybeStop {
    async fn handle(
        &mut self,
        _: CrashPlain,
        reply: spawned_concurrency::reply::PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
        {
            panic!("injected cohort failure");
        }
        let _ = reply.reply(());
    }
}
