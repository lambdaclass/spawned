#![cfg(feature = "async")]
//! Generated policy scripts against a separate generation/retirement oracle.
//! Action boundaries are synchronized; this does not control Tokio/OS schedules.
use proptest::prelude::*;
use spawned_concurrency::{
    message::Message,
    supervision::{
        Backoff, ChildSnapshot, ChildSpec, ChildState, RestartMode, SupervisedChild,
        SupervisorConfig, SupervisorStrategy,
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
    async fn handle(
        &mut self,
        _: Generation,
        reply: spawned_concurrency::PendingReply<'_, u64>,
        _: &Context<Self>,
    ) {
        reply.reply(self.0);
    }
}
impl Handler<Crash> for Probe {
    async fn handle(
        &mut self,
        _: Crash,
        _reply: spawned_concurrency::PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
        {
            panic!("generated supervisor failure");
        }
    }
}
#[derive(Clone, Debug)]
struct Expected {
    generation: u64,
    live: bool,
}
#[derive(Clone, Copy, Debug)]
enum Action {
    Panic,
    Normal,
    Shutdown,
}
fn restart_mode() -> impl Strategy<Value = RestartMode> {
    prop_oneof![
        Just(RestartMode::Permanent),
        Just(RestartMode::Transient),
        Just(RestartMode::Temporary)
    ]
}
fn strategy() -> impl Strategy<Value = SupervisorStrategy> {
    prop_oneof![
        Just(SupervisorStrategy::OneForOne),
        Just(SupervisorStrategy::OneForAll),
        Just(SupervisorStrategy::RestForOne)
    ]
}
fn action() -> impl Strategy<Value = Action> {
    prop_oneof![
        Just(Action::Panic),
        Just(Action::Normal),
        Just(Action::Shutdown)
    ]
}
// Contract oracle: no calls into the implementation's policy/selection helpers.
fn apply(
    expected: &mut [Expected],
    modes: &[RestartMode],
    origin: usize,
    abnormal: bool,
    strategy: SupervisorStrategy,
) {
    let restart = match modes[origin] {
        RestartMode::Permanent => true,
        RestartMode::Transient => abnormal,
        RestartMode::Temporary => false,
    };
    if !restart {
        expected[origin].live = false;
        return;
    }
    for (index, child) in expected.iter_mut().enumerate() {
        let affected = match strategy {
            SupervisorStrategy::OneForOne => index == origin,
            SupervisorStrategy::OneForAll => true,
            SupervisorStrategy::RestForOne => index >= origin,
            other => unreachable!("strategy {other:?} not modelled"),
        };
        if affected && child.live {
            if modes[index] == RestartMode::Temporary {
                child.live = false;
            } else {
                child.generation += 1;
            }
        }
    }
}
/// The snapshot fields the checker reads (`ChildSnapshot` is
/// `#[non_exhaustive]`, so the negative test below builds these instead).
#[derive(Debug)]
struct Seen {
    name: String,
    state: ChildState,
    generation: u64,
}
impl From<ChildSnapshot> for Seen {
    fn from(s: ChildSnapshot) -> Self {
        Seen {
            name: s.name,
            state: s.state,
            generation: s.generation,
        }
    }
}
fn seen(children: &[SupervisedChild<Probe>]) -> Vec<Seen> {
    children.iter().map(|c| c.snapshot().into()).collect()
}
fn unsettled(actual: &[Seen], expected: &[Expected]) -> Vec<usize> {
    assert_eq!(actual.len(), expected.len());
    let mut pending = Vec::new();
    // Do not short-circuit: a later child may have been over-reconstructed even
    // while an earlier child is still starting or retiring.
    for (index, (actual, expected)) in actual.iter().zip(expected).enumerate() {
        assert_ne!(
            actual.state,
            ChildState::Failed,
            "unexpected child failure: {actual:?}"
        );
        assert!(
            actual.generation <= expected.generation,
            "unexpected extra reconstruction: {}; actual {actual:?}; expected {expected:?}",
            actual.name
        );
        let state = if expected.live {
            ChildState::Running
        } else {
            ChildState::Stopped
        };
        if actual.generation != expected.generation || actual.state != state {
            pending.push(index);
        }
    }
    pending
}
async fn observe(children: &[SupervisedChild<Probe>], expected: &[Expected]) {
    let outcome = spawned_rt::tasks::timeout(LIMIT, async {
        loop {
            let actual = seen(children);
            let pending = unsettled(&actual, expected);
            if pending.is_empty() {
                break;
            }
            let waits: Vec<_> = pending
                .iter()
                .map(|&index| {
                    let after = if expected[index].live {
                        expected[index].generation.saturating_sub(1)
                    } else {
                        expected[index].generation
                    };
                    Box::pin(children[index].wait_for_generation(after))
                })
                .collect();
            // Existing watch-backed notification: no test-owned busy polling.
            let (result, index, _) = futures::future::select_all(waits).await;
            if let Err(error) = result {
                assert!(
                    !expected[pending[index]].live && error.0 == ChildState::Stopped,
                    "unexpected generation wait failure: child {}; {error:?}",
                    pending[index]
                );
                assert_eq!(
                    children[pending[index]].snapshot().generation,
                    expected[pending[index]].generation,
                    "child retired before its expected generation"
                );
            }
            // A terminal notification can be immediately ready. Always give
            // the watchdog timer a polling opportunity, even for broken state.
            spawned_rt::tasks::yield_now().await;
        }
    })
    .await;
    if outcome.is_err() {
        let actual = seen(children);
        // Preserve the concrete over-reconstruction diagnosis at the watchdog
        // boundary too, rather than hiding it behind a generic timeout.
        let pending = unsettled(&actual, expected);
        panic!(
            "policy did not settle: children {pending:?}; actual {actual:?}; expected {expected:?}"
        );
    }
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
async fn scenario(
    strategy: SupervisorStrategy,
    modes: &[RestartMode],
    actions: &[(u8, Action)],
    close_at: usize,
) {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("policy-oracle", 1).unwrap();
    let mut tree = system
        .supervisor(
            SupervisorConfig::default()
                .strategy(strategy)
                .max_restarts(128)
                .restart_window(Duration::from_secs(3600))
                .backoff(
                    Backoff::default()
                        .initial(Duration::ZERO)
                        .maximum(Duration::ZERO)
                        .jitter(false)
                        .seed(42),
                ),
        )
        .unwrap();
    let mut children = Vec::new();
    for (index, restart) in modes.iter().enumerate() {
        children.push(
            tree.add(
                ChildSpec::new(format!("child-{index}"), &group, |ctx| async move {
                    Ok::<_, String>(Probe(ctx.generation()))
                })
                .restart(*restart),
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
        let reason = match kind {
            Action::Panic => {
                assert!(matches!(
                    actor.request_with_timeout(Crash, LIMIT).await,
                    Err(ActorError::ActorStopped(ExitReason::Panic(_)))
                ));
                let reason = actor.wait_exit().await;
                assert!(matches!(reason, ExitReason::Panic(_)));
                true
            }
            Action::Normal => {
                actor.context().stop();
                assert_eq!(actor.wait_exit().await, ExitReason::Normal);
                false
            }
            Action::Shutdown => {
                actor
                    .child_handle()
                    .stop_with_reason(spawned_concurrency::ExitReason::Shutdown);
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
        strategy in strategy(),
        modes in prop::collection::vec(restart_mode(), 1..5),
        actions in prop::collection::vec((any::<u8>(), action()), 1..13),
        close_selector in any::<u8>(),
    ) {
        let close_at = usize::from(close_selector) % (actions.len() + 1);
        spawned_rt::tasks::sim_test(|_| scenario(strategy, &modes, &actions, close_at));
    }
}
#[test]
fn retired_temporary_and_transient_slots_stay_retired_across_later_recoveries() {
    for strategy in [
        SupervisorStrategy::OneForOne,
        SupervisorStrategy::OneForAll,
        SupervisorStrategy::RestForOne,
    ] {
        spawned_rt::tasks::sim_test(|_| {
            scenario(
                strategy,
                &[
                    RestartMode::Permanent,
                    RestartMode::Transient,
                    RestartMode::Temporary,
                ],
                &[
                    (2, Action::Normal),
                    (1, Action::Normal),
                    (0, Action::Panic),
                    (0, Action::Shutdown),
                    (0, Action::Normal),
                ],
                5,
            )
        });
    }
}

#[test]
#[should_panic(expected = "unexpected extra reconstruction: child-1")]
fn a_waiting_earlier_child_cannot_hide_a_later_extra_reconstruction() {
    let snapshot = |name: &str, state, generation| Seen {
        name: name.into(),
        state,
        generation,
    };
    let actual = [
        snapshot("child-0", ChildState::Backoff, 1),
        snapshot("child-1", ChildState::Running, 3),
    ];
    let expected = vec![
        Expected {
            generation: 2,
            live: true
        };
        2
    ];
    unsettled(&actual, &expected);
}

#[spawned_macros::sim_test]
async fn an_already_retired_child_reports_a_generation_error_without_spinning() {
    use futures::FutureExt;
    use std::panic::AssertUnwindSafe;
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("retired-observer-control", 1).unwrap();
    let mut tree = system.supervisor(SupervisorConfig::default()).unwrap();
    let child = tree
        .add(
            ChildSpec::new("retired", &group, |ctx| async move {
                Ok::<_, String>(Probe(ctx.generation()))
            })
            .restart(RestartMode::Temporary),
        )
        .unwrap();
    let supervisor = tree.start().await.unwrap();
    child.current().unwrap().context().stop();
    assert!(
        spawned_rt::tasks::timeout(LIMIT, child.wait_for_generation(1))
            .await
            .unwrap()
            .is_err()
    );
    // Deliberately wrong oracle: this child retired at generation one. Its
    // generation notification returns immediately, so the observer must diagnose
    // the mismatch rather than repeatedly polling a ready future forever.
    let invalid = [Expected {
        generation: 2,
        live: false,
    }];
    let result = spawned_rt::tasks::timeout(
        Duration::from_millis(200),
        AssertUnwindSafe(observe(&[child], &invalid)).catch_unwind(),
    )
    .await;
    assert!(supervisor.shutdown(LIMIT).await.complete);
    assert!(system.shutdown(LIMIT).await.complete);
    let panic = result
        .expect("observer did not cooperate with its watchdog")
        .expect_err("invalid retirement was accepted");
    let message = panic
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| panic.downcast_ref::<&str>().copied())
        .unwrap_or("unknown panic");
    assert!(
        message.contains("child retired before its expected generation"),
        "{message}"
    );
}
