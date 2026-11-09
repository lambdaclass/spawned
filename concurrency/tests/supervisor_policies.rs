#![cfg(feature = "async")]
//! Generated policy scripts against a separate generation/retirement oracle.
//! Action boundaries are synchronized; this does not control Tokio/OS schedules.
use proptest::prelude::*;
use spawned_concurrency::{
    message::Message,
    supervision::{
        Backoff, ChildSpec, ChildState, RestartMode, SupervisedChild, SupervisorConfig,
        SupervisorStrategy,
    },
    system::ActorSystem,
    tasks::{Actor, Context, Handler},
    ActorError, ExitReason,
};
use std::time::Duration;
const LIMIT: Duration = Duration::from_secs(5);
struct Generation;
impl Message for Generation {
    type Result = u64;
}
struct Crash;
impl Message for Crash {
    type Result = ();
}
struct Probe(u64);
impl Actor for Probe {}
impl Handler<Generation> for Probe {
    async fn handle(&mut self, _: Generation, _: &Context<Self>) -> u64 {
        self.0
    }
}
impl Handler<Crash> for Probe {
    async fn handle(&mut self, _: Crash, _: &Context<Self>) {
        panic!("generated supervisor failure");
    }
}
#[derive(Clone, Debug)]
struct Expected {
    generation: u64,
    live: bool,
}
fn mode(value: u8) -> RestartMode {
    match value % 3 {
        0 => RestartMode::Permanent,
        1 => RestartMode::Transient,
        _ => RestartMode::Temporary,
    }
}
// Contract oracle: no calls into the implementation's policy/selection helpers.
fn apply(expected: &mut [Expected], modes: &[u8], origin: usize, abnormal: bool, strategy: u8) {
    let restart = match modes[origin] % 3 {
        0 => true,
        1 => abnormal,
        _ => false,
    };
    if !restart {
        expected[origin].live = false;
        return;
    }
    for (index, child) in expected.iter_mut().enumerate() {
        let affected = match strategy % 3 {
            0 => index == origin,
            1 => true,
            _ => index >= origin,
        };
        if affected && child.live {
            if modes[index] % 3 == 2 {
                child.live = false;
            } else {
                child.generation += 1;
            }
        }
    }
}
async fn observe(children: &[SupervisedChild<Probe>], expected: &[Expected]) {
    tokio::time::timeout(LIMIT, async {
        loop {
            let settled = children.iter().zip(expected).all(|(child, expected)| {
                let actual = child.snapshot();
                assert!(
                    actual.generation <= expected.generation,
                    "unexpected extra reconstruction: {actual:?}; expected {expected:?}"
                );
                actual.generation == expected.generation
                    && actual.state
                        == if expected.live {
                            ChildState::Running
                        } else {
                            ChildState::Stopped
                        }
            });
            if settled {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("policy did not settle");
    for (child, expected) in children.iter().zip(expected) {
        if expected.live {
            let actor = child.current().expect("ready generation");
            assert_eq!(
                actor.request_with_timeout(Generation, LIMIT).await.unwrap(),
                expected.generation
            );
        } else {
            assert!(child.current().is_err());
            assert!(child
                .wait_for_generation(expected.generation)
                .await
                .is_err());
        }
    }
}
async fn scenario(strategy: u8, modes: &[u8], actions: &[(u8, u8)], close_at: usize) {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("policy-oracle", 1).unwrap();
    let mut tree = system
        .supervisor(SupervisorConfig {
            strategy: match strategy % 3 {
                0 => SupervisorStrategy::OneForOne,
                1 => SupervisorStrategy::OneForAll,
                _ => SupervisorStrategy::RestForOne,
            },
            max_restarts: 128,
            restart_window: Duration::from_secs(3600),
            backoff: Backoff {
                initial: Duration::ZERO,
                maximum: Duration::ZERO,
                jitter: false,
                seed: Some(42),
                ..Backoff::default()
            },
            ..SupervisorConfig::default()
        })
        .unwrap();
    let mut children = Vec::new();
    for (index, restart) in modes.iter().enumerate() {
        children.push(
            tree.add(
                ChildSpec::new(format!("child-{index}"), &group, |ctx| async move {
                    Ok::<_, String>(Probe(ctx.generation()))
                })
                .restart(mode(*restart)),
            )
            .unwrap(),
        );
    }
    let supervisor = tree.start().await.unwrap();
    let mut expected = vec![
        Expected {
            generation: 1,
            live: true
        };
        modes.len()
    ];
    observe(&children, &expected).await;
    let mut closed = false;
    for (step, &(target, kind)) in actions.iter().enumerate() {
        if step == close_at {
            assert!(supervisor.shutdown(LIMIT).await.complete);
            closed = true;
        }
        if closed {
            for (child, old) in children.iter().zip(&expected) {
                assert_eq!(child.snapshot().generation, old.generation);
                assert!(child.current().is_err());
            }
            continue;
        }
        let index = usize::from(target) % children.len();
        if !expected[index].live {
            continue;
        }
        let actor = children[index].current().unwrap();
        let reason = match kind % 3 {
            0 => {
                assert!(matches!(
                    actor.request_with_timeout(Crash, LIMIT).await,
                    Err(ActorError::ActorStopped)
                ));
                let reason = actor.wait_exit().await;
                assert!(matches!(reason, ExitReason::Panic(_)));
                true
            }
            1 => {
                actor.context().stop();
                assert_eq!(actor.wait_exit().await, ExitReason::Normal);
                false
            }
            _ => {
                actor.child_handle().shutdown();
                assert_eq!(actor.wait_exit().await, ExitReason::Shutdown);
                false
            }
        };
        apply(&mut expected, modes, index, reason, strategy);
        observe(&children, &expected).await;
    }
    assert!(supervisor.failure().is_none());
    assert!(supervisor.shutdown(LIMIT).await.complete);
    for (child, old) in children.iter().zip(&expected) {
        assert_eq!(child.snapshot().generation, old.generation);
        assert!(child.current().is_err());
    }
    assert!(system.shutdown(LIMIT).await.complete);
}
fn configuration() -> ProptestConfig {
    let mut config = ProptestConfig::default();
    if std::env::var_os("PROPTEST_CASES").is_none() {
        config.cases = 32;
    }
    config
}
proptest! {
    #![proptest_config(configuration())]
    #[test]
    fn generated_restart_modes_strategies_and_shutdown_follow_the_contract(
        strategy in 0u8..3,
        modes in prop::collection::vec(0u8..3, 1..5),
        actions in prop::collection::vec((any::<u8>(),0u8..3), 1..13),
        close_selector in any::<u8>(),
    ) {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let close_at = usize::from(close_selector) % (actions.len() + 1);
        runtime.block_on(scenario(strategy, &modes, &actions, close_at));
    }
}
#[test]
fn retired_temporary_and_transient_slots_stay_retired_across_later_recoveries() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    for strategy in 0..3 {
        runtime.block_on(scenario(
            strategy,
            &[0, 1, 2],
            &[(2, 1), (1, 1), (0, 0), (0, 2), (0, 1)],
            5,
        ));
    }
}
