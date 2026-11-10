//! Seed sweeps over unmodified supervision workloads on the deterministic
//! simulation backend. Run with:
//!
//! ```sh
//! RUSTFLAGS="--cfg spawned_sim" cargo test -p spawned-concurrency --test sim -- --nocapture
//! ```
//!
//! `SPAWNED_SIM_SEEDS` sets the sweep size (default 200).
#![cfg(all(spawned_sim, feature = "async"))]

use spawned_concurrency::{
    message::Message,
    supervision::{
        Backoff, ChildSpec, RestartMode, SupervisedChild, SupervisorConfig, SupervisorStrategy,
    },
    system::ActorSystem,
    tasks::{Actor, ActorStart, Context, Handler},
};
use spawned_rt::tasks::{sim, Runtime};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const LIMIT: Duration = Duration::from_secs(5);

fn seeds() -> u64 {
    std::env::var("SPAWNED_SIM_SEEDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(200)
}

type Log = Arc<Mutex<Vec<String>>>;

fn push(log: &Log, event: String) {
    log.lock().unwrap_or_else(|p| p.into_inner()).push(event);
}

fn config(strategy: SupervisorStrategy) -> SupervisorConfig {
    SupervisorConfig {
        strategy,
        max_restarts: 3,
        backoff: Backoff {
            initial: Duration::from_millis(1),
            maximum: Duration::from_millis(4),
            jitter: false,
            // Fixed jitter seed: otherwise it comes from SystemTime.
            seed: Some(7),
            ..Backoff::default()
        },
        ..SupervisorConfig::default()
    }
}

struct Crash;
impl Message for Crash {
    type Result = ();
}

/// Records started/stopped; `crash` sends itself a panicking message from
/// `started()`, so it dies right after it publishes readiness.
struct Node {
    id: u32,
    crash: bool,
    log: Log,
}
impl Actor for Node {
    async fn started(&mut self, ctx: &Context<Self>) {
        push(&self.log, format!("started:{}", self.id));
        if self.crash {
            let _ = ctx.send(Crash);
        }
    }
    async fn stopped(&mut self, _: &Context<Self>) {
        push(&self.log, format!("stopped:{}", self.id));
    }
}
impl Handler<Crash> for Node {
    async fn handle(&mut self, _: Crash, _: &Context<Self>) {
        panic!("injected crash after readiness");
    }
}

/// One simulated run: outcome, lifecycle log and the scheduler's trace.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Run<T> {
    outcome: T,
    log: Vec<String>,
    trace: Vec<(u64, Duration)>,
    stats: sim::Stats,
}

fn simulate<T>(seed: u64, record: bool, workload: impl AsyncFnOnce(Log) -> T) -> Run<T> {
    let rt = Runtime::with_seed(seed);
    if record {
        rt.record_trace();
    }
    let log: Log = Arc::default();
    let outcome = rt.block_on(workload(log.clone()));
    let stats = rt.stats();
    let trace = rt.trace();
    let log = log.lock().unwrap_or_else(|p| p.into_inner()).clone();
    Run {
        outcome,
        log,
        trace,
        stats,
    }
}

// ---------------------------------------------------------------------------
// Workload 1: ordered system shutdown, before=2, sup_children=1, after=2
// (cross_backend::ordered_system_shutdown_matches minimal failing input).
// ---------------------------------------------------------------------------

async fn shutdown_order(log: Log) -> Vec<u32> {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("sim-order", 2).unwrap();
    let start = |id| Node {
        id,
        crash: false,
        log: log.clone(),
    };
    for id in 0..2 {
        start(id).start_on(&group).unwrap().ready().await.unwrap();
    }
    let mut tree = system
        .supervisor(config(SupervisorStrategy::OneForOne))
        .unwrap();
    let child_log = log.clone();
    tree.add(ChildSpec::new("s0", &group, move |_| {
        let log = child_log.clone();
        async move {
            Ok::<_, String>(Node {
                id: 100,
                crash: false,
                log,
            })
        }
    }))
    .unwrap();
    let _tree = tree.start().await.unwrap();
    for id in 200..202 {
        start(id).start_on(&group).unwrap().ready().await.unwrap();
    }
    let report = system.shutdown(LIMIT).await;
    assert!(report.complete, "shutdown incomplete");
    log.lock()
        .unwrap()
        .iter()
        .filter_map(|e| e.strip_prefix("stopped:").map(|id| id.parse().unwrap()))
        .collect()
}

#[test]
fn shutdown_order_race_needs_preemption() {
    let expected = vec![201, 200, 100, 1, 0];
    let mut wrong = Vec::new();
    let mut hashes = std::collections::BTreeSet::new();
    let n = seeds();
    let began = Instant::now();
    for seed in 0..n {
        let run = simulate(seed, false, shutdown_order);
        hashes.insert(run.stats.schedule_hash);
        if run.outcome != expected {
            wrong.push((seed, run.outcome));
        }
    }
    let elapsed = began.elapsed();
    println!(
        "shutdown-order sweep: {n} seeds in {elapsed:?} ({:.0} seeds/s), {} distinct schedules, {} misordered: {:?}",
        n as f64 / elapsed.as_secs_f64(),
        hashes.len(),
        wrong.len(),
        wrong.iter().take(5).collect::<Vec<_>>()
    );
    // The PR #209 race needs an OS-thread preemption between the
    // supervisor's readiness commit and its gate check (no await between
    // them), so a single-thread executor cannot interleave it.
    assert!(wrong.is_empty(), "misordered seeds: {wrong:?}");
}

// ---------------------------------------------------------------------------
// Workload 2: PR #204 finding 2. The supervisor fails during initial
// placement; a launched-but-unstarted child's terminal state depends on
// whether its controller's finish(failed) or the supervisor's
// finish_unstarted sweep runs last.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Placement {
    started: bool,
    /// (state, generation) per child, in spec order.
    states: Vec<(String, u64)>,
}

async fn initial_failure(log: Log) -> Placement {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("sim-initial", 2).unwrap();
    let mut tree = system
        .supervisor(config(SupervisorStrategy::OneForAll))
        .unwrap();
    let mut children: Vec<SupervisedChild<Node>> = Vec::new();
    for index in 0..3u32 {
        let log = log.clone();
        children.push(
            tree.add(
                ChildSpec::new(format!("c{index}"), &group, move |_| {
                    let log = log.clone();
                    async move {
                        Ok::<_, String>(Node {
                            id: index,
                            crash: index == 0,
                            log,
                        })
                    }
                })
                .restart(RestartMode::Permanent),
            )
            .unwrap(),
        );
    }
    let started = tree.start().await.is_ok();
    let report = system.shutdown(LIMIT).await;
    assert!(report.complete, "shutdown incomplete");
    Placement {
        started,
        states: children
            .iter()
            .map(|c| {
                let s = c.snapshot();
                (format!("{:?}", s.state), s.generation)
            })
            .collect(),
    }
}

#[test]
fn never_started_child_state_replays_from_seed() {
    let n = seeds();
    let mut outcomes: BTreeMap<Placement, Vec<u64>> = BTreeMap::new();
    let began = Instant::now();
    for seed in 0..n {
        let run = simulate(seed, false, initial_failure);
        outcomes.entry(run.outcome).or_default().push(seed);
    }
    let elapsed = began.elapsed();
    println!(
        "initial-failure sweep: {n} seeds in {elapsed:?} ({:.0} seeds/s)",
        n as f64 / elapsed.as_secs_f64()
    );
    for (outcome, seeds) in &outcomes {
        println!(
            "  {outcome:?}: {} seeds, first {:?}",
            seeds.len(),
            &seeds[..seeds.len().min(5)]
        );
    }
    // Each outcome's first seed replays it, with an identical lifecycle log
    // and schedule, on two further runs.
    for (outcome, seeds) in &outcomes {
        let seed = seeds[0];
        let a = simulate(seed, true, initial_failure);
        let b = simulate(seed, true, initial_failure);
        assert_eq!(&a.outcome, outcome, "seed {seed} did not replay");
        assert_eq!(a, b, "seed {seed} gave two different runs");
        println!(
            "  seed {seed} replayed: {} scheduling decisions, log {:?}",
            a.trace.len(),
            a.log
        );
    }
    let unstarted_failed = outcomes.keys().any(|o| {
        o.states
            .iter()
            .any(|(state, generation)| *generation == 0 && state == "Failed")
    });
    let unstarted_stopped = outcomes.keys().any(|o| {
        o.states
            .iter()
            .any(|(state, generation)| *generation == 0 && state == "Stopped")
    });
    println!(
        "never-started child: Failed seen = {unstarted_failed}, Stopped seen = {unstarted_stopped}"
    );
}

#[test]
fn same_seed_same_trace_different_seeds_differ() {
    let a = simulate(42, true, shutdown_order);
    let b = simulate(42, true, shutdown_order);
    assert_eq!(a, b);
    let hashes: std::collections::BTreeSet<_> = (0..20)
        .map(|seed| simulate(seed, false, shutdown_order).stats.schedule_hash)
        .collect();
    println!(
        "seed 42: {} decisions, hash {:#x}; 20 seeds gave {} distinct schedules",
        a.trace.len(),
        a.stats.schedule_hash,
        hashes.len()
    );
    assert!(hashes.len() > 1, "every seed produced the same schedule");
}

#[test]
fn virtual_time_advances_only_when_idle() {
    let rt = Runtime::with_seed(1);
    let wall = Instant::now();
    rt.block_on(async {
        spawned_rt::tasks::sleep(Duration::from_secs(3600)).await;
        let late = spawned_rt::tasks::timeout(
            Duration::from_secs(1),
            spawned_rt::tasks::sleep(Duration::from_secs(10)),
        )
        .await;
        assert!(late.is_err());
    });
    assert_eq!(rt.stats().now, Duration::from_secs(3601));
    assert!(wall.elapsed() < Duration::from_secs(1));
}

#[test]
#[should_panic(expected = "spawned_sim: spawn_blocking is not supported")]
fn spawn_blocking_is_rejected() {
    let rt = Runtime::with_seed(1);
    rt.block_on(async {
        let _ = spawned_rt::tasks::spawn_blocking(|| ());
    });
}

#[test]
fn thread_groups_are_rejected() {
    let rt = Runtime::with_seed(1);
    rt.block_on(async {
        let system = ActorSystem::new().unwrap();
        let error = system.thread_group("native", 1).err().expect("rejected");
        assert!(error.to_string().contains("spawned_sim"), "{error}");
    });
}
