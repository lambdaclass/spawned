//! Supervisor decisions that only differ in a narrow interleaving, swept over
//! simulator seeds so the window is hit deterministically. Each body also runs
//! once on Tokio in a normal build, as a smoke test of the same scenario.
//!
//! ```sh
//! RUSTFLAGS="--cfg spawned_sim --cfg tokio_unstable" cargo test -p spawned-concurrency --test supervision_races -- --nocapture
//! ```
//!
//! `SPAWNED_SIM_SEEDS` sets the sweep size (default 64).
#![cfg(feature = "async")]

#[macro_use]
#[path = "support/deadline.rs"]
mod deadline;
use spawned_concurrency::{
    supervision::{
        Backoff, ChildSpec, ChildState, RestartMode, SupervisorConfig, SupervisorStrategy,
    },
    system::ActorSystem,
    tasks::Actor,
    ExitReason,
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

struct Idle;
impl Actor for Idle {}

/// OneForAll. `a` (Transient) exits `Normal` and `b` (Permanent) crashes
/// while `c` is still starting, so both decisions wait for the tree's
/// readiness (S1 deferral). Readiness wakes both controllers at once: when
/// `b`'s recovery round is assigned to `a` before `a` decides, `a` must not
/// retire (its `Normal` exit says stop) but serve the round. Retiring it
/// instead (`run_child`'s non-restart exit taken for OneForAll) leaves the
/// round waiting for `a`'s cleanup until it fails the tree.
///
/// Returns whether this schedule hit the window (`a` was rebuilt).
async fn deferred_stop_serves_a_round_assigned_before_the_decision() -> bool {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("deferred-stop-round", 2).unwrap();
    let mut builder = system
        .supervisor(
            SupervisorConfig::default()
                .strategy(SupervisorStrategy::OneForAll)
                .max_restarts(3)
                .backoff(
                    Backoff::default()
                        .initial(Duration::ZERO)
                        .maximum(Duration::ZERO)
                        .jitter(false)
                        .seed(7),
                ),
        )
        .unwrap();
    let idle = |name: &str| {
        ChildSpec::new(name, &group, |_| async { Ok::<_, String>(Idle) })
            .shutdown_timeout(Duration::from_secs(1))
    };
    let a = builder
        .add(idle("a").restart(RestartMode::Transient))
        .unwrap();
    let b = builder.add(idle("b")).unwrap();
    let (release, gate) = spawned_rt::tasks::oneshot::channel::<()>();
    let gate = Arc::new(Mutex::new(Some(gate)));
    let c = builder
        .add(
            ChildSpec::new("c", &group, move |_| {
                let gate = gate.lock().unwrap().take();
                async move {
                    if let Some(gate) = gate {
                        let _ = gate.await;
                    }
                    Ok::<_, String>(Idle)
                }
            })
            .shutdown_timeout(Duration::from_secs(1)),
        )
        .unwrap();
    let start = spawned_rt::tasks::spawn(builder.start());

    let first_a = within!(a.wait_ready()).await.unwrap();
    let first_b = within!(b.wait_ready()).await.unwrap();
    first_a.child_handle().stop_with_reason(ExitReason::Normal);
    first_b
        .child_handle()
        .stop_with_reason(ExitReason::Panic("b crashes during startup".into()));
    within!(async {
        while a.snapshot().last_exit.is_none() || b.snapshot().last_exit.is_none() {
            spawned_rt::tasks::sleep(Duration::from_millis(1)).await;
        }
    })
    .await;
    // On the simulator a sleep returns only once every other task is idle:
    // both controllers have parked in the deferral.
    spawned_rt::tasks::sleep(Duration::from_millis(1)).await;
    release.send(()).unwrap();
    let tree = within!(start).await.unwrap().unwrap();

    let rebuilt = within!(b.wait_for_generation(1)).await;
    assert!(
        tree.failure().is_none(),
        "tree failed: {:?}",
        tree.failure()
    );
    rebuilt.unwrap();
    within!(c.wait_for_generation(1)).await.unwrap();
    let a_state = a.snapshot();
    let hit = a_state.generation > 1;
    if hit {
        within!(a.wait_for_generation(1)).await.unwrap();
    } else {
        assert_eq!(a_state.state, ChildState::Stopped, "{a_state:?}");
    }
    assert!(
        tree.failure().is_none(),
        "tree failed: {:?}",
        tree.failure()
    );
    assert!(
        within!(system.shutdown(Duration::from_secs(10)))
            .await
            .complete
    );
    hit
}

#[cfg(not(spawned_sim))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deferred_stop_serves_a_round_assigned_before_the_decision_on_tokio() {
    deferred_stop_serves_a_round_assigned_before_the_decision().await;
}

#[cfg(spawned_sim)]
fn seeds() -> u64 {
    std::env::var("SPAWNED_SIM_SEEDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(64)
}

#[cfg(spawned_sim)]
#[test]
fn deferred_stop_serves_a_round_assigned_before_the_decision_over_seeds() {
    let mut hits = Vec::new();
    for seed in 0..seeds() {
        let rt = spawned_rt::tasks::Runtime::with_seed(seed);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            rt.block_on(deferred_stop_serves_a_round_assigned_before_the_decision())
        }));
        match result {
            Ok(true) => hits.push(seed),
            Ok(false) => (),
            Err(payload) => {
                eprintln!("failing seed: SPAWNED_SIM_SEED={seed}");
                std::panic::resume_unwind(payload)
            }
        }
    }
    println!("round assigned before the deferred decision on seeds {hits:?}");
    // The sweep must reach the window, or it checks nothing.
    assert!(
        !hits.is_empty(),
        "no seed assigned the round before a decided"
    );
}
