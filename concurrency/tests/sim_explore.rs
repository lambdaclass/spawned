//! Systematic exploration (PCT, bounded DFS over delays) against the seeded
//! random walk, on planted ordering bugs between real actors and on the PR
//! #204 never-started-child race. Every test here asserts that the bug IS
//! found: they are negative controls for the explorer. Run with:
//!
//! ```sh
//! RUSTFLAGS="--cfg spawned_sim" cargo test -p spawned-concurrency --test sim_explore -- --nocapture
//! ```
//!
//! `explore_budget_table` (ignored) prints runs-to-first-failure for each
//! strategy with larger budgets.
#![cfg(all(spawned_sim, feature = "async"))]

use spawned_concurrency::{
    message::Message,
    supervision::{Backoff, ChildSpec, RestartMode, SupervisorConfig, SupervisorStrategy},
    system::ActorSystem,
    tasks::{Actor, ActorRef, ActorStart, Context, Handler},
};
use spawned_rt::tasks::sim::{
    self,
    explore::{explore, Report, Strategy},
};
use std::time::Duration;

// ---------------------------------------------------------------------------
// Planted bug: read-modify-write through a store actor (lost update).
// ---------------------------------------------------------------------------

struct Store {
    value: u64,
}
impl Actor for Store {}

struct Get;
impl Message for Get {
    type Result = u64;
}
struct Set(u64);
impl Message for Set {
    type Result = ();
}
impl Handler<Get> for Store {
    async fn handle(&mut self, _: Get, _: &Context<Self>) -> u64 {
        self.value
    }
}
impl Handler<Set> for Store {
    async fn handle(&mut self, Set(value): Set, _: &Context<Self>) {
        self.value = value;
    }
}

/// On `Bump`, yields `noise` times (unrelated work), then increments the
/// store with a non-atomic get-then-set: two requests, so another worker's
/// requests can land between them.
struct Worker {
    store: ActorRef<Store>,
    noise: u32,
}
impl Actor for Worker {}

struct Bump;
impl Message for Bump {
    type Result = ();
}
impl Handler<Bump> for Worker {
    async fn handle(&mut self, _: Bump, _: &Context<Self>) {
        for _ in 0..self.noise {
            spawned_rt::tasks::yield_now().await;
        }
        let value = self.store.request(Get).await.unwrap();
        self.store.request(Set(value + 1)).await.unwrap();
    }
}

/// Workers with the given noise bump the store once each, concurrently.
/// Returns the final value; it is `noises.len()` unless updates were lost.
async fn bump_all(noises: &[u32]) -> u64 {
    let store = Store { value: 0 }.start();
    let workers: Vec<_> = noises
        .iter()
        .map(|&noise| {
            Worker {
                store: store.clone(),
                noise,
            }
            .start()
        })
        .collect();
    let replies: Vec<_> = workers
        .iter()
        .map(|w| w.request_raw(Bump).unwrap())
        .collect();
    for reply in replies {
        reply.await.unwrap();
    }
    store.request(Get).await.unwrap()
}

/// Depth 2: one lost update between a quick worker and one that first does
/// `NOISE` polls of unrelated work. The slow worker's read must land in the
/// quick worker's one-poll window between its read and its write.
const NOISE: u32 = 12;

async fn lost_update_depth2() {
    let value = bump_all(&[0, NOISE]).await;
    assert_eq!(value, 2, "lost update: two bumps left {value}");
}

/// Depth 3: all three workers (noise 0, NOISE, 2 * NOISE) read before any of
/// them writes, so two updates are lost and the store ends at 1.
async fn lost_update_depth3() {
    let value = bump_all(&[0, NOISE, 2 * NOISE]).await;
    assert_ne!(value, 1, "two lost updates: three bumps left 1");
}

// ---------------------------------------------------------------------------
// PR #204 finding 2 (see tests/sim.rs): a launched-but-never-started child's
// terminal state depends on which of two finishes runs last.
// ---------------------------------------------------------------------------

struct Crash;
impl Message for Crash {
    type Result = ();
}
struct Node {
    crash: bool,
}
/// As in `tests/sim.rs` with zero yields: `started()` spawns a task that sends
/// the actor a panicking message.
impl Actor for Node {
    async fn started(&mut self, ctx: &Context<Self>) {
        if self.crash {
            let actor = ctx.actor_ref();
            drop(spawned_rt::tasks::spawn(async move {
                let _ = actor.send(Crash);
            }));
        }
    }
}
impl Handler<Crash> for Node {
    async fn handle(&mut self, _: Crash, _: &Context<Self>) {
        panic!("injected crash after readiness");
    }
}

/// States (`Debug` of the state, generation) of three OneForAll children
/// whose first child crashes right after `started()`.
async fn initial_failure() -> Vec<(String, u64)> {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("explore-initial", 2).unwrap();
    let mut tree = system
        .supervisor(SupervisorConfig {
            strategy: SupervisorStrategy::OneForAll,
            max_restarts: 3,
            backoff: Backoff {
                initial: Duration::from_millis(1),
                maximum: Duration::from_millis(4),
                jitter: false,
                seed: Some(7),
                ..Backoff::default()
            },
            ..SupervisorConfig::default()
        })
        .unwrap();
    let children: Vec<_> = (0..3)
        .map(|index| {
            tree.add(
                ChildSpec::new(format!("c{index}"), &group, move |_| async move {
                    Ok::<_, String>(Node { crash: index == 0 })
                })
                .restart(RestartMode::Permanent),
            )
            .unwrap()
        })
        .collect();
    let _ = tree.start().await;
    assert!(system.shutdown(Duration::from_secs(5)).await.complete);
    children
        .iter()
        .map(|c| {
            let s = c.snapshot();
            (format!("{:?}", s.state), s.generation)
        })
        .collect()
}

/// Fails when a never-started child ends `wanted`.
async fn unstarted_child_never_ends(wanted: &str) {
    for (index, (state, generation)) in initial_failure().await.iter().enumerate() {
        assert!(
            *generation != 0 || state != wanted,
            "never-started child c{index} ended {state}"
        );
    }
}

// ---------------------------------------------------------------------------
// Driver
// ---------------------------------------------------------------------------

fn run<F, Fut>(strategy: &str, budget: usize, body: &F) -> Report
where
    F: Fn(u64) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let strategy = Strategy::parse(strategy).unwrap();
    let report = explore(&strategy, 0, budget, body);
    let found = match &report.found {
        Some(found) => format!(
            "found at run {} (seed {}, {} polls, {} non-canonical picks; reproduce: {})",
            found.run,
            found.seed,
            found.outcome.stats.steps,
            found.outcome.tape.deviations(),
            found.strategy
        ),
        None if report.exhausted => "not found; bounded space exhausted".into(),
        None => "not found within budget".into(),
    };
    println!(
        "  {:<14} {:>6} runs, {:>6} distinct schedules, longest {:>4} polls: {found}",
        strategy.to_string(),
        report.runs,
        report.schedules,
        report.max_polls
    );
    assert_eq!(report.divergences, 0, "{strategy}: nondeterministic body");
    report
}

/// The failing run's tape replays the same failure, and shrinks, through the
/// existing tooling.
fn assert_replays<F, Fut>(report: &Report, body: &F)
where
    F: Fn(u64) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let found = report.found.as_ref().expect("bug not found");
    let failure = found.outcome.failure.clone().unwrap();
    let replay = sim::run_once(found.seed, Some(&found.outcome.tape), body);
    assert_eq!(replay.failure.as_deref(), Some(failure.as_str()));
    assert_eq!(replay.tape, found.outcome.tape);
    assert_eq!(replay.stats.tape_misses, 0);
    let (small, _) = sim::shrink(&found.outcome.tape, &failure, body, 200);
    let again = sim::run_once(found.seed, Some(&small), body);
    assert_eq!(again.failure.as_deref(), Some(failure.as_str()));
}

#[test]
fn lost_update_depth2_found_by_pct_and_dfs() {
    let body = |_| lost_update_depth2();
    println!("lost update, depth 2 (noise {NOISE}):");
    let random = run("random", 200, &body);
    let pct = run("pct:d=2", 2_000, &body);
    let dfs = run("dfs:bound=1", 2_000, &body);
    assert!(pct.found.is_some(), "PCT d=2 missed a depth-2 bug");
    assert!(dfs.found.is_some(), "DFS bound 1 missed a depth-2 bug");
    assert_replays(&pct, &body);
    assert_replays(&dfs, &body);
    // Random is only reported: its count is not a property of the code.
    let _ = random;
}

#[test]
fn lost_update_depth3_found_by_pct_and_dfs() {
    let body = |_| lost_update_depth3();
    println!("two lost updates, depth 3 (noise {NOISE}, {}):", 2 * NOISE);
    let _random = run("random", 200, &body);
    let pct = run("pct:d=3", 20_000, &body);
    let dfs = run("dfs:bound=2", 20_000, &body);
    assert!(pct.found.is_some(), "PCT d=3 missed a depth-3 bug");
    assert!(dfs.found.is_some(), "DFS bound 2 missed a depth-3 bug");
    assert_replays(&pct, &body);
    assert_replays(&dfs, &body);
}

#[test]
fn dfs_exhausts_a_bug_free_bounded_space() {
    // One bump per worker, run one after the other: no interleaving loses
    // an update, so DFS must exhaust its bound without a failure.
    let body = |_| async {
        let value = bump_all(&[0]).await;
        assert_eq!(value, 1);
    };
    println!("bug-free control:");
    let report = run("dfs:bound=2", 100_000, &body);
    assert!(report.found.is_none());
    assert!(report.exhausted, "bounded space not exhausted");
}

#[test]
fn pr204_race_found_by_each_strategy() {
    for wanted in ["Failed", "Stopped"] {
        let body = move |_| unstarted_child_never_ends(wanted);
        println!("PR #204: never-started child ends {wanted}:");
        let random = run("random", 500, &body);
        let pct = run("pct:d=2", 500, &body);
        let dfs = run("dfs:bound=2", 500, &body);
        for report in [&random, &pct, &dfs] {
            assert!(report.found.is_some(), "{}: not found", report.strategy);
            assert_replays(report, &body);
        }
    }
}

/// Runs-to-first-failure table with larger budgets (prints only).
#[test]
#[ignore = "prints the strategy comparison table"]
fn explore_budget_table() {
    let budget: usize = std::env::var("SPAWNED_SIM_BUDGET")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100_000);
    let strategies = [
        "random",
        "pct:d=2",
        "pct:d=3",
        "dfs:bound=1",
        "dfs:bound=2",
        "dfs:bound=3",
    ];
    println!("lost update, depth 2:");
    for s in strategies {
        run(s, budget, &|_| lost_update_depth2());
    }
    println!("two lost updates, depth 3:");
    for s in strategies {
        run(s, budget, &|_| lost_update_depth3());
    }
    for wanted in ["Failed", "Stopped"] {
        println!("PR #204: never-started child ends {wanted}:");
        for s in strategies {
            run(s, budget, &move |_| unstarted_child_never_ends(wanted));
        }
    }
}
