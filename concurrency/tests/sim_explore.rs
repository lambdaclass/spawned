//! Systematic exploration (PCT, bounded DFS over delays) against the seeded
//! random walk, on planted ordering bugs between real actors and on the PR
//! #204 never-started-child race. Every planted bug must be found, with its
//! own failure message (not just any panic): they are negative controls for
//! the explorer. The PR #204 `Stopped` outcome, closed by PR #209, must not
//! be. Run with:
//!
//! ```sh
//! RUSTFLAGS="--cfg spawned_sim --cfg tokio_unstable" cargo test -p spawned-concurrency --test sim_explore -- --nocapture
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
    explore::{explore, sweep, Report, Strategy},
};
use std::time::Duration;

// ---------------------------------------------------------------------------
// Planted bug: read-modify-write through a store actor (lost update).
// ---------------------------------------------------------------------------

/// One access to the store, in the order the store actor handled them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Access {
    Read(u64),
    Write(u64),
}

struct Store {
    value: u64,
    log: Vec<Access>,
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
/// The store's access log.
struct Log;
impl Message for Log {
    type Result = Vec<Access>;
}
impl Handler<Get> for Store {
    async fn handle(&mut self, _: Get, _: &Context<Self>) -> u64 {
        self.log.push(Access::Read(self.value));
        self.value
    }
}
impl Handler<Set> for Store {
    async fn handle(&mut self, Set(value): Set, _: &Context<Self>) {
        self.log.push(Access::Write(value));
        self.value = value;
    }
}
impl Handler<Log> for Store {
    async fn handle(&mut self, _: Log, _: &Context<Self>) -> Vec<Access> {
        self.log.clone()
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
/// Returns the store's access log.
async fn bump_all(noises: &[u32]) -> Vec<Access> {
    let store = Store {
        value: 0,
        log: Vec::new(),
    }
    .start();
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
    store.request(Log).await.unwrap()
}

/// The values written, in order.
fn writes(log: &[Access]) -> Vec<u64> {
    log.iter()
        .filter_map(|a| match a {
            Access::Write(v) => Some(*v),
            Access::Read(_) => None,
        })
        .collect()
}

/// Reads handled before the first write.
fn reads_before_first_write(log: &[Access]) -> usize {
    log.iter()
        .take_while(|a| matches!(a, Access::Read(_)))
        .count()
}

/// Every write is `read + 1` and the store starts at 0, so a read returns 0
/// exactly when it is handled before every write. All `n` writes being 1
/// therefore means all `n` reads came before any write: the bug needs that
/// whole ordering, not just some lost update.
fn every_write_saw_zero(log: &[Access], n: usize) -> bool {
    writes(log) == vec![1; n]
}

/// Two workers: one lost update between a quick worker and one that first
/// does `NOISE` polls of unrelated work. The slow worker's read must land in
/// the quick worker's window between its read and its write.
const NOISE: u32 = 12;

const TWO_WORKERS_BUG: &str = "both workers read 0 before either wrote";
const THREE_WORKERS_BUG: &str = "all three workers read 0 before any wrote";

async fn lost_update_two_workers() {
    let log = bump_all(&[0, NOISE]).await;
    assert!(!every_write_saw_zero(&log, 2), "{TWO_WORKERS_BUG}: {log:?}");
}

/// Three workers (noise 0, NOISE, 2 * NOISE) all read before any of them
/// writes. (A final value of 1 alone does not need that: A reads 0, B reads
/// 0, A writes 1, C reads 1 and writes 2, B writes 1.) This is a schedule
/// with three ordering constraints, not a claim about its bug depth in the
/// PCT sense: one demotion already reaches it.
async fn lost_update_three_workers() {
    let log = bump_all(&[0, NOISE, 2 * NOISE]).await;
    assert!(
        !every_write_saw_zero(&log, 3),
        "{THREE_WORKERS_BUG}: {log:?}"
    );
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
    F: Fn(u64) -> Fut + Sync,
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
        None if report.complete() => "not found; bounded space exhausted".into(),
        None if report.exhausted => "not found; exhausted up to the cutoffs".into(),
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

/// The strategy found a failure, and it is the planted one: its panic
/// message contains `bug` (a deadlock, livelock or unrelated panic is not
/// the planted bug).
fn assert_found(report: &Report, bug: &str) {
    let found = report
        .found
        .as_ref()
        .unwrap_or_else(|| panic!("{}: {bug:?} not found", report.strategy));
    let failure = found.outcome.failure.as_deref().unwrap_or_default();
    assert!(
        failure.contains(bug),
        "{}: expected {bug:?}, failed with {failure:?}",
        report.strategy
    );
}

/// The failing run's tape replays the same failure strictly, and shrinks,
/// through the existing tooling.
fn assert_replays<F, Fut>(report: &Report, body: &F)
where
    F: Fn(u64) -> Fut + Sync,
    Fut: std::future::Future<Output = ()>,
{
    let found = report.found.as_ref().expect("bug not found");
    let failure = found.outcome.failure.clone().unwrap();
    let replay = sim::run_once(found.seed, Some(&found.outcome.tape), body);
    assert_eq!(replay.failure.as_deref(), Some(failure.as_str()));
    assert_eq!(replay.tape, found.outcome.tape);
    let (small, _) = sim::shrink(&found.outcome.tape, &failure, body, 200);
    let again = sim::run_once(found.seed, Some(&small), body);
    assert_eq!(again.failure.as_deref(), Some(failure.as_str()));
}

#[test]
fn lost_update_two_workers_found_by_pct_and_dfs() {
    let body = |_| lost_update_two_workers();
    println!("lost update, depth 2 (noise {NOISE}):");
    // Random is only reported: its count is not a property of the code.
    let _random = run("random", 200, &body);
    let pct = run("pct:d=2", 2_000, &body);
    let dfs = run("dfs:bound=1", 2_000, &body);
    assert_found(&pct, TWO_WORKERS_BUG);
    assert_found(&dfs, TWO_WORKERS_BUG);
    assert_replays(&pct, &body);
    assert_replays(&dfs, &body);
}

#[test]
fn lost_update_three_workers_found_by_pct_and_dfs() {
    let body = |_| lost_update_three_workers();
    println!(
        "all three read before any write (noise 0, {NOISE}, {}):",
        2 * NOISE
    );
    let _random = run("random", 200, &body);
    let pct = run("pct:d=3", 20_000, &body);
    let dfs = run("dfs:bound=2", 20_000, &body);
    assert_found(&pct, THREE_WORKERS_BUG);
    assert_found(&dfs, THREE_WORKERS_BUG);
    assert_replays(&pct, &body);
    assert_replays(&dfs, &body);
}

/// Codex P2 (#218): the depth-3 control fails exactly when all three reads
/// precede every write. Checked over every schedule with at most 3 delays of
/// a smaller instance (noise 0/2/4), and over 2,000 PCT runs of the real one
/// (the random walk practically never reaches the ordering): the planted
/// failure and the claimed ordering always coincide, and both occur.
#[test]
fn three_worker_bug_requires_all_reads_before_any_write() {
    use std::sync::{Arc, Mutex};
    let check = |strategy: &str, noises: &'static [u32], runs: usize| {
        let seen: Arc<Mutex<Vec<Vec<Access>>>> = Arc::default();
        let record = seen.clone();
        let body = move |_| {
            let record = record.clone();
            async move {
                let log = bump_all(noises).await;
                record.lock().unwrap().push(log);
            }
        };
        let report = sweep(&Strategy::parse(strategy).unwrap(), 0, runs, &body);
        assert_eq!(report.failures, 0);
        assert_eq!(report.divergences, 0);
        let logs = seen.lock().unwrap();
        let mut bad = 0;
        for log in logs.iter() {
            let all_reads_first = reads_before_first_write(log) == 3;
            assert_eq!(every_write_saw_zero(log, 3), all_reads_first, "{log:?}");
            bad += usize::from(all_reads_first);
        }
        println!(
            "  {strategy} over noise {noises:?}: {} runs, {bad} with all reads first",
            logs.len()
        );
        assert!(bad > 0, "{strategy}: ordering never reached");
        assert!(bad < logs.len(), "{strategy}: ordering always reached");
        report
    };
    let dfs = check("dfs:bound=3", &[0, 2, 4], 100_000);
    assert!(dfs.complete(), "DFS did not exhaust the bounded space");
    check("pct:d=3,k=60", &[0, NOISE, 2 * NOISE], 2_000);
}

/// Codex P1 / agy H2 (#218), end to end: three tasks that each record a
/// step and yield, three steps each. PCT with `d = 3` reaches `AAABBCBCC`,
/// which change points sorted by time could not (they reached 66 of these
/// schedules).
#[test]
fn pct_reaches_the_papers_schedules_of_three_yielding_tasks() {
    use std::sync::{Arc, Mutex};
    let schedules: Arc<Mutex<std::collections::BTreeSet<String>>> = Arc::default();
    let seen = schedules.clone();
    let body = move |_| {
        let seen = seen.clone();
        async move {
            let trace = Arc::new(Mutex::new(String::new()));
            let tasks: Vec<_> = ['A', 'B', 'C']
                .into_iter()
                .map(|name| {
                    let trace = trace.clone();
                    spawned_rt::tasks::spawn(async move {
                        for step in 0..3 {
                            trace.lock().unwrap().push(name);
                            if step < 2 {
                                sim::yield_now().await;
                            }
                        }
                    })
                })
                .collect();
            for task in tasks {
                task.await.unwrap();
            }
            let trace = trace.lock().unwrap().clone();
            seen.lock().unwrap().insert(trace);
        }
    };
    sweep(&Strategy::parse("pct:d=3,k=9").unwrap(), 0, 20_000, &body);
    let schedules = schedules.lock().unwrap();
    assert!(schedules.contains("AAABBCBCC"), "AAABBCBCC unreachable");
    println!("{} distinct schedules", schedules.len());
    assert!(schedules.len() > 66, "{}", schedules.len());
}

#[test]
fn dfs_exhausts_a_bug_free_bounded_space() {
    // One bump per worker, run one after the other: no interleaving loses
    // an update, so DFS must exhaust its bound without a failure.
    let body = |_| async {
        let log = bump_all(&[0]).await;
        assert_eq!(writes(&log), vec![1]);
    };
    println!("bug-free control:");
    let report = run("dfs:bound=2", 100_000, &body);
    assert!(report.found.is_none());
    assert!(report.complete(), "bounded space not exhausted");
}

/// Codex P2 / agy M3 (#218): polls past the fairness cutoff are not choice
/// points, so with a cutoff of one poll DFS cannot try the second worker
/// first. It must say the space was truncated rather than claim exhaustion.
#[test]
fn dfs_reports_truncation_at_the_fairness_cutoff() {
    use std::sync::{Arc, Mutex};
    let orders: Arc<Mutex<std::collections::BTreeSet<Vec<u8>>>> = Arc::default();
    let seen = orders.clone();
    let body = move |_| {
        let seen = seen.clone();
        async move {
            let order = Arc::new(Mutex::new(Vec::new()));
            let workers: Vec<_> = [1u8, 2]
                .into_iter()
                .map(|n| {
                    let order = order.clone();
                    spawned_rt::tasks::spawn(async move { order.lock().unwrap().push(n) })
                })
                .collect();
            for worker in workers {
                worker.await.unwrap();
            }
            let order = order.lock().unwrap().clone();
            seen.lock().unwrap().insert(order);
        }
    };
    let cut = run("dfs:bound=2,fair=1", 1_000, &body);
    assert!(cut.exhausted && cut.truncated, "{cut:?}");
    assert!(!cut.complete());
    assert_eq!(
        orders.lock().unwrap().len(),
        1,
        "the cutoff should hide one order"
    );

    orders.lock().unwrap().clear();
    let full = run("dfs:bound=2", 1_000, &body);
    assert!(full.complete(), "{full:?}");
    assert_eq!(orders.lock().unwrap().len(), 2, "both worker orders");
}

#[test]
fn pr204_stopped_outcome_stays_closed() {
    // Sanity check, not a rare race: each strategy reaches the failed
    // placement and sees a never-started child end `Failed`, as PR #209
    // requires (the canonical schedule already does).
    let failed = |_| unstarted_child_never_ends("Failed");
    println!("PR #204: never-started child ends Failed (expected path):");
    for strategy in ["random", "pct:d=2", "dfs:bound=2"] {
        let report = run(strategy, 500, &failed);
        assert_found(&report, "ended Failed");
        assert_replays(&report, &failed);
    }
    // The old `Stopped` outcome, closed by PR #209, is not reachable: DFS
    // exhausts its bounded space without it.
    let stopped = |_| unstarted_child_never_ends("Stopped");
    println!("PR #204: never-started child ends Stopped (closed by PR #209):");
    for strategy in ["random", "pct:d=2", "dfs:bound=2"] {
        let report = run(strategy, 500, &stopped);
        assert!(report.found.is_none(), "{strategy}: race reopened");
        assert!(report.complete() || !strategy.starts_with("dfs"));
    }
}

/// Runs-to-first-failure and hit-rate table with larger budgets (prints
/// only). `SPAWNED_SIM_BUDGET` (default 100,000) caps first-failure searches;
/// hit rates use at most 10,000 runs.
#[test]
#[ignore = "prints the strategy comparison table"]
fn explore_budget_table() {
    let budget: usize = std::env::var("SPAWNED_SIM_BUDGET")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100_000);
    table("lost update, two workers", budget, TWO_WORKERS_BUG, |_| {
        lost_update_two_workers()
    });
    table(
        "all three read before any write",
        budget,
        THREE_WORKERS_BUG,
        |_| lost_update_three_workers(),
    );
    table(
        "PR #204: never-started child ends Stopped",
        budget,
        "ended Stopped",
        |_| unstarted_child_never_ends("Stopped"),
    );
}

fn table<F, Fut>(title: &str, budget: usize, bug: &str, body: F)
where
    F: Fn(u64) -> Fut + Sync,
    Fut: std::future::Future<Output = ()>,
{
    println!("{title}:");
    for s in [
        "random",
        "pct:d=2",
        "pct:d=3",
        "dfs:bound=1",
        "dfs:bound=2",
        "dfs:bound=3",
    ] {
        let report = run(s, budget, &body);
        if report.found.is_some() {
            assert_found(&report, bug);
        }
        let strategy = match Strategy::parse(s).unwrap() {
            Strategy::Dfs { .. } => continue,
            // PCT's k fixed at the longest run seen, so runs are independent.
            Strategy::Pct { depth, .. } => Strategy::Pct {
                depth,
                steps: Some(report.max_polls.max(1)),
                fair_after: None,
            },
            other => other,
        };
        let all = sweep(&strategy, 0, budget.min(10_000), &body);
        println!(
            "  {:<14} hit rate {}/{} = {:.3}%",
            strategy.to_string(),
            all.failures,
            all.runs,
            100.0 * all.failures as f64 / all.runs as f64
        );
    }
}

// ---------------------------------------------------------------------------
// Through #[sim_test]
// ---------------------------------------------------------------------------

/// The harness with `explore`: PCT finds the planted lost update, prints the
/// report (strategy, seed, tape, shrunk tape) and fails the test.
#[spawned_macros::sim_test(explore = "pct:d=2", iterations = 200)]
#[should_panic(expected = "both workers read 0 before either wrote")]
async fn sim_test_pct_finds_lost_update() {
    lost_update_two_workers().await;
}

/// The harness with a bounded DFS on a bug-free body passes, reporting the
/// bounded space exhausted.
#[spawned_macros::sim_test(explore = "dfs:bound=1", iterations = 10_000)]
async fn sim_test_dfs_passes_bug_free_body() {
    assert_eq!(writes(&bump_all(&[0]).await), vec![1]);
}

/// Codex (round 2): a PCT failure found with `k` left to the strategy
/// reproduces from its seed and the reported strategy alone (`k` and the
/// fairness cutoff filled in), without the tape.
#[test]
fn pct_failure_reproduces_from_seed_and_reported_strategy() {
    let body = |_| lost_update_two_workers();
    let report = run("pct:d=2", 2_000, &body);
    assert_found(&report, TWO_WORKERS_BUG);
    let found = report.found.unwrap();
    assert!(
        found.strategy.to_string().contains(",k="),
        "{}",
        found.strategy
    );
    let again = explore(&found.strategy, found.seed, 1, &body);
    let again = again
        .found
        .expect("seed and strategy did not reproduce the failure");
    assert_eq!(again.outcome.failure, found.outcome.failure);
    assert_eq!(again.outcome.tape, found.outcome.tape);
}

/// Codex/agy (round 2): a body that is not deterministic under the
/// simulator (here it spawns an extra task every other run) makes DFS stop
/// at the first diverging run instead of exploring from it, and the report
/// is not `complete()`.
#[test]
fn dfs_stops_and_reports_incomplete_on_divergence() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static RUNS: AtomicU64 = AtomicU64::new(0);
    let body = |_| async {
        let extra = RUNS.fetch_add(1, Ordering::SeqCst) % 2 == 1;
        let a = spawned_rt::tasks::spawn(sim::yield_now());
        let b = extra.then(|| spawned_rt::tasks::spawn(sim::yield_now()));
        let c = spawned_rt::tasks::spawn(sim::yield_now());
        a.await.unwrap();
        c.await.unwrap();
        if let Some(b) = b {
            b.await.unwrap();
        }
    };
    let report = explore(&Strategy::parse("dfs:bound=2").unwrap(), 0, 1_000, &body);
    assert!(report.divergences >= 1, "{report:?}");
    assert_eq!(report.runs, 2, "DFS kept exploring after a divergence");
    assert!(!report.exhausted && !report.complete(), "{report:?}");
}
