//! Seed sweeps over unmodified supervision workloads on the deterministic
//! simulation backend. Run with:
//!
//! ```sh
//! RUSTFLAGS="--cfg spawned_sim --cfg tokio_unstable" cargo test -p spawned-concurrency --test sim -- --nocapture
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

/// Records started/stopped. With `crash_after: Some(k)`, `started()` spawns a
/// task that yields `k` times and then sends the actor a panicking message,
/// so it dies a few scheduling steps after it publishes readiness.
struct Node {
    id: u32,
    crash_after: Option<u32>,
    log: Log,
}
impl Actor for Node {
    async fn started(&mut self, ctx: &Context<Self>) {
        push(&self.log, format!("started:{}", self.id));
        if let Some(yields) = self.crash_after {
            let actor = ctx.actor_ref();
            drop(spawned_rt::tasks::spawn(async move {
                for _ in 0..yields {
                    sim::yield_now().await;
                }
                let _ = actor.send(Crash);
            }));
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
        crash_after: None,
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
                crash_after: None,
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

async fn initial_failure(log: Log, yields: u32) -> Placement {
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
                            crash_after: (index == 0).then_some(yields),
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
    // (crash delay in yields, outcome) -> seeds producing it.
    let mut outcomes: BTreeMap<(u32, Placement), Vec<u64>> = BTreeMap::new();
    let began = Instant::now();
    for yields in 0..4 {
        for seed in 0..n {
            let run = simulate(seed, false, |log| initial_failure(log, yields));
            outcomes
                .entry((yields, run.outcome))
                .or_default()
                .push(seed);
        }
    }
    let elapsed = began.elapsed();
    println!(
        "initial-failure sweep: {} runs in {elapsed:?} ({:.0} seeds/s)",
        4 * n,
        (4 * n) as f64 / elapsed.as_secs_f64()
    );
    for ((yields, outcome), seeds) in &outcomes {
        println!(
            "  yields={yields} {outcome:?}: {} seeds, first {:?}",
            seeds.len(),
            &seeds[..seeds.len().min(5)]
        );
    }
    // Each outcome's first seed replays it, with an identical lifecycle log
    // and schedule, on two further runs.
    for ((yields, outcome), seeds) in &outcomes {
        let (yields, seed) = (*yields, seeds[0]);
        let a = simulate(seed, true, |log| initial_failure(log, yields));
        let b = simulate(seed, true, |log| initial_failure(log, yields));
        assert_eq!(&a.outcome, outcome, "seed {seed} did not replay");
        assert_eq!(a, b, "seed {seed} gave two different runs");
        println!(
            "  replayed yields={yields} seed={seed}: {} decisions, hash {:#x}, log {:?}",
            a.trace.len(),
            a.stats.schedule_hash,
            a.log
        );
    }
    let unstarted = |wanted: &str| {
        outcomes.keys().any(|(_, o)| {
            o.states
                .iter()
                .any(|(state, generation)| *generation == 0 && state == wanted)
        })
    };
    println!(
        "never-started child: Failed seen = {}, Stopped seen = {}",
        unstarted("Failed"),
        unstarted("Stopped")
    );
    // PR #209 closed the race: no schedule leaves a never-started child Stopped.
    assert!(!unstarted("Stopped"), "never-started child ended Stopped");
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
    let wall = sim::wall_now();
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
        drop(spawned_rt::tasks::spawn_blocking(|| ()));
    });
}

#[test]
fn thread_groups_are_rejected() {
    let rt = Runtime::with_seed(1);
    rt.block_on(async {
        let system = ActorSystem::new().unwrap();
        let Err(error) = system.thread_group("native", 1) else {
            panic!("thread group accepted under simulation");
        };
        assert!(error.to_string().contains("spawned_sim"), "{error}");
    });
}

/// PR #204 finding 2, pinned: before the v0.7.1 lifecycle fixes (PR #209),
/// seed 12 left the launched but never-started child `c1` Failed and seed 2
/// left it Stopped. With PR #209 a failed initial placement finishes every
/// never-started child as Failed, so both seeds must now agree.
#[test]
fn pr204_never_started_child_state_pinned_seeds() {
    for (seed, expected) in [(12, "Failed"), (2, "Failed")] {
        let runs: Vec<_> = (0..3)
            .map(|_| simulate(seed, true, |log| initial_failure(log, 0)))
            .collect();
        assert!(
            runs.windows(2).all(|w| w[0] == w[1]),
            "seed {seed} diverged"
        );
        let c1 = &runs[0].outcome.states[1];
        println!("seed {seed}: c1 = {c1:?}, outcome {:?}", runs[0].outcome);
        assert_eq!(c1, &(expected.to_string(), 0), "seed {seed}");
    }
}

// ---------------------------------------------------------------------------
// Virtual time: restart window and jittered backoff (spawned_rt::time).
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
struct WindowRun {
    /// Virtual time at which each replacement generation became current.
    restarts: Vec<Duration>,
    failure: Option<String>,
    elapsed: Duration,
}

/// Budget 3 restarts per 60 s, jittered exponential backoff (1 s .. 8 s) with
/// the jitter seed left to the runtime. Three quick crashes use the budget;
/// after 61 s idle the window has slid past them and three more restarts are
/// admitted; a fourth crash inside the new window exhausts the budget.
async fn restart_window(out: Arc<Mutex<Option<WindowRun>>>) {
    use spawned_rt::time::Instant;
    let began = Instant::now();
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("sim-window", 1).unwrap();
    let mut tree = system
        .supervisor(SupervisorConfig {
            strategy: SupervisorStrategy::OneForOne,
            max_restarts: 3,
            restart_window: Duration::from_secs(60),
            backoff: Backoff {
                initial: Duration::from_secs(1),
                maximum: Duration::from_secs(8),
                stable_after: Duration::from_secs(3600),
                jitter: true,
                // Left to spawned_rt::time::entropy: seeded by the simulation.
                seed: None,
            },
            ..SupervisorConfig::default()
        })
        .unwrap();
    let child: SupervisedChild<Node> = tree
        .add(ChildSpec::new("flappy", &group, |_| async {
            Ok::<_, String>(Node {
                id: 0,
                crash_after: None,
                log: Log::default(),
            })
        }))
        .unwrap();
    let supervisor = tree.start().await.unwrap();
    let mut restarts = Vec::new();
    for round in 0..2 {
        for _ in 0..3 {
            let generation = child.snapshot().generation;
            assert!(child.current().unwrap().request(Crash).await.is_err());
            spawned_rt::tasks::timeout(
                Duration::from_secs(60),
                child.wait_for_generation(generation),
            )
            .await
            .unwrap()
            .unwrap();
            restarts.push(began.elapsed());
        }
        if round == 0 {
            spawned_rt::tasks::sleep(Duration::from_secs(61)).await;
        }
    }
    assert!(child.current().unwrap().request(Crash).await.is_err());
    // The budget is charged when the retry fires, after up to 8 s of backoff.
    spawned_rt::tasks::timeout(Duration::from_secs(60), supervisor.join())
        .await
        .unwrap();
    let failure = supervisor.failure();
    assert!(system.shutdown(LIMIT).await.complete);
    *out.lock().unwrap() = Some(WindowRun {
        restarts,
        failure,
        elapsed: began.elapsed(),
    });
}

#[test]
fn restart_window_and_backoff_run_in_virtual_time() {
    let run = |seed: u64| {
        let out = Arc::new(Mutex::new(None));
        let outcome = sim::run_once(seed, None, &|_| restart_window(out.clone()));
        assert_eq!(outcome.failure, None, "seed {seed}");
        let result = out.lock().unwrap().take().unwrap();
        (outcome, result)
    };
    let wall = Instant::now();
    let mut schedules = std::collections::BTreeSet::new();
    for seed in 0..8 {
        let (a, first) = run(seed);
        let (b, second) = run(seed);
        // Deterministic per seed: identical restart times, tape and hash.
        assert_eq!(first, second, "seed {seed}");
        assert_eq!(a.tape, b.tape, "seed {seed}");
        assert_eq!(a.stats.schedule_hash, b.stats.schedule_hash);
        assert_eq!(first.restarts.len(), 6, "seed {seed}: {first:?}");
        assert!(
            first.restarts[3] >= Duration::from_secs(61),
            "seed {seed}: the slid window admitted restarts before the sleep ended: {first:?}"
        );
        assert!(
            first
                .failure
                .as_deref()
                .is_some_and(|f| f.contains("restart budget exhausted")),
            "seed {seed}: {first:?}"
        );
        if seed == 0 {
            println!(
                "seed 0: restarts at {:?}, gave up at {:?} virtual, {} polls",
                first.restarts, first.elapsed, a.stats.steps
            );
        }
        schedules.insert(first.restarts);
    }
    let real = wall.elapsed();
    println!(
        "16 runs of ~{}s virtual each in {real:?} real; {} distinct jittered schedules over 8 seeds",
        schedules.iter().next().unwrap().last().unwrap().as_secs(),
        schedules.len()
    );
    assert!(real < Duration::from_secs(10), "took {real:?}");
    // Jitter is seeded by the simulation seed, so seeds differ.
    assert!(schedules.len() > 1);
}

// ---------------------------------------------------------------------------
// Decision tape: PR #204's never-started-child race through the harness.
// ---------------------------------------------------------------------------

/// Fails when a launched-but-never-started child (generation 0) ends
/// `Failed` instead of `Stopped` (the PR #204 finding 2 race).
async fn unstarted_children_end_stopped() {
    let placement = initial_failure(Log::default(), 0).await;
    for (index, (state, generation)) in placement.states.iter().enumerate() {
        assert!(
            *generation != 0 || state == "Stopped",
            "never-started child c{index} ended {state}"
        );
    }
}

#[test]
fn pr204_race_replays_from_a_recorded_tape() {
    let body = |_| unstarted_children_end_stopped();
    let (seed, failing) = (0..200)
        .map(|seed| (seed, sim::run_once(seed, None, &body)))
        .find(|(_, outcome)| outcome.failure.is_some())
        .expect("no seed in 0..200 reproduced the race");
    let failure = failing.failure.clone().unwrap();
    println!(
        "seed {seed} fails after {} polls ({} non-canonical): {failure}",
        failing.stats.steps,
        failing.tape.deviations()
    );
    assert!(failure.contains("Failed"), "{failure}");

    // The tape round-trips through its text form.
    let path = std::env::temp_dir()
        .join("spawned-sim")
        .join(format!("pr204-seed{seed}-{}.tape", std::process::id()));
    failing.tape.save(&path).unwrap();
    let tape = sim::Tape::load(&path).unwrap();
    assert_eq!(tape, failing.tape);

    // Replaying the tape reproduces the failure and the exact schedule.
    let replay = sim::run_once(seed, Some(&tape), &body);
    assert_eq!(replay.failure.as_deref(), Some(failure.as_str()));
    assert_eq!(replay.tape, tape);
    assert_eq!(replay.stats.tape_misses, 0);

    // The tape, not the generator, drives the schedule: under other seeds
    // (every generator draw different) it still replays the same run.
    for other in [seed + 1, seed + 1_000, u64::MAX] {
        let replay = sim::run_once(other, Some(&tape), &body);
        assert_eq!(
            replay.failure.as_deref(),
            Some(failure.as_str()),
            "seed {other}"
        );
        assert_eq!(replay.tape.decisions, tape.decisions, "seed {other}");
    }

    // Shrinking keeps the failure with fewer non-canonical decisions.
    let (small, runs) = sim::shrink(&tape, &failure, &body, 300);
    let again = sim::run_once(seed, Some(&small), &body);
    assert_eq!(again.failure.as_deref(), Some(failure.as_str()));
    assert!(small.deviations() <= tape.deviations());
    println!(
        "shrunk {} -> {} non-canonical decisions ({} -> {} polls) in {runs} runs; {}",
        tape.deviations(),
        small.deviations(),
        tape.polls(),
        small.polls(),
        path.display()
    );
    let _ = std::fs::remove_file(&path);
}

/// Manual demo of the harness report: fails on the race's first seed and
/// prints the replay commands and tape paths. Run with
/// `RUSTFLAGS="--cfg spawned_sim --cfg tokio_unstable" cargo test -p spawned-concurrency --test sim -- --ignored harness_reports`.
#[spawned_macros::sim_test(seeds = 64)]
#[ignore = "demonstrates a failing report"]
async fn harness_reports_pr204_race_with_replay_commands() {
    unstarted_children_end_stopped().await;
}

// ---------------------------------------------------------------------------
// Strict replay, tape format and virtual-time policy (review fixes).
// ---------------------------------------------------------------------------

/// A busy task races a 1 ms timeout: with a small spin limit the clock creeps
/// past the deadline before the task finishes, with the default it does not.
async fn spin_vs_timeout() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let done = Arc::new(AtomicBool::new(false));
    let flag = done.clone();
    drop(spawned_rt::tasks::spawn(async move {
        for _ in 0..3_000 {
            sim::yield_now().await;
        }
        flag.store(true, Ordering::Relaxed);
    }));
    let waited = spawned_rt::tasks::timeout(Duration::from_millis(1), async {
        while !done.load(Ordering::Relaxed) {
            sim::yield_now().await;
        }
    })
    .await;
    assert!(waited.is_ok(), "spin-wait timed out");
}

/// Codex P2 (#214): a strict replay takes clock jumps from the tape, so a
/// failure recorded under one `SPAWNED_SIM_SPIN` replays under another, and
/// the tape carries the recorded spin limit.
#[test]
fn strict_replay_follows_recorded_clock_jumps() {
    let failure_of = |rt: &Runtime| {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            rt.block_on(spin_vs_timeout())
        }));
        result
            .err()
            .map(|payload| match payload.downcast::<String>() {
                Ok(message) => *message,
                Err(payload) => payload
                    .downcast::<&str>()
                    .map(|m| m.to_string())
                    .unwrap_or_default(),
            })
    };
    let record = |spin: u64| {
        let rt = Runtime::with_seed(4);
        rt.set_spin_limit(spin);
        rt.record_tape();
        (failure_of(&rt), rt.tape())
    };
    let (failure, tape) = record(100);
    let failure = failure.expect("a 100-poll spin limit should time the spin-wait out");
    assert!(failure.contains("spin-wait timed out"), "{failure}");
    assert_eq!(record(1_000_000).0, None, "a large spin limit should not");
    assert_eq!(tape.spin, Some(100));
    assert_eq!(sim::Tape::parse(&tape.to_text()).unwrap(), tape);

    // Strict replay with a different spin limit in force reproduces exactly
    // that failure (not a divergence) and consumes the whole tape.
    let rt = Runtime::with_seed(4);
    rt.replay(&tape);
    rt.set_spin_limit(1_000_000);
    assert_eq!(failure_of(&rt).as_deref(), Some(failure.as_str()));
    assert_eq!(rt.stats().tape_left, 0);
}

/// Body for the divergence tests: two tasks and some yields.
async fn small_schedule() {
    let a = spawned_rt::tasks::spawn(async {
        for _ in 0..3 {
            sim::yield_now().await;
        }
    });
    let b = spawned_rt::tasks::spawn(sim::yield_now());
    a.await.unwrap();
    b.await.unwrap();
}

/// Codex P2 (#214): a tape pick whose task is not ready is a divergence, not
/// a silent canonical choice.
#[test]
fn strict_replay_fails_on_a_pick_that_is_not_ready() {
    let body = |_| small_schedule();
    let recorded = sim::run_once(1, None, &body);
    assert_eq!(recorded.failure, None);
    let mut tape = recorded.tape.clone();
    let first_poll = tape
        .decisions
        .iter()
        .position(|d| !matches!(d, sim::Decision::Fire(_)))
        .unwrap();
    tape.decisions[first_poll] = sim::Decision::Poll(999);
    let replay = sim::run_once(1, Some(&tape), &body);
    let failure = replay.failure.expect("diverging tape replayed silently");
    assert!(failure.contains("replay diverged"), "{failure}");
    assert!(failure.contains("task 999"), "{failure}");
}

/// Codex P2 (#214): a tape that runs out before the run ends, or has
/// decisions left over when it ends, is a divergence.
#[test]
fn strict_replay_fails_on_a_short_or_long_tape() {
    let body = |_| small_schedule();
    let recorded = sim::run_once(2, None, &body);
    let mut short = recorded.tape.clone();
    short.decisions.truncate(short.decisions.len() / 2);
    let failure = sim::run_once(2, Some(&short), &body)
        .failure
        .unwrap_or_default();
    assert!(
        failure.contains("replay diverged: the tape ended"),
        "{failure:?}"
    );

    let mut long = recorded.tape.clone();
    long.decisions.push(sim::Decision::Canonical(None));
    let failure = sim::run_once(2, Some(&long), &body)
        .failure
        .unwrap_or_default();
    assert!(failure.contains("unused"), "{failure:?}");

    // The recorded tape itself replays cleanly, and permissive replay still
    // accepts the edited tapes (for the shrinker).
    assert_eq!(sim::run_once(2, Some(&recorded.tape), &body).failure, None);
    let permissive = sim::run_with(2, Some((&short, sim::ReplayMode::Permissive)), &body);
    assert_eq!(permissive.failure, None);
}

/// agy L1: clock jumps are written as u128 nanoseconds and must parse back.
#[test]
fn tape_parses_clock_jumps_beyond_u64_nanos() {
    let far = (u64::MAX as u128) * 1_000;
    let tape = sim::Tape::parse(&format!("seed 1\nt {far}\nc\n")).unwrap();
    match tape.decisions[0] {
        sim::Decision::Fire(at) => assert_eq!(at.as_nanos(), far),
        other => panic!("{other:?}"),
    }
    assert_eq!(sim::Tape::parse(&tape.to_text()).unwrap(), tape);
}

/// agy H3: a spin-wait bounded by a 5 s timeout reaches the timeout instead
/// of tripping the livelock guard (1 ms per 10,000 polls needs 50 million
/// polls for 5 s; the guard stops at 10 million).
#[test]
fn spin_wait_reaches_a_multi_second_timeout() {
    let rt = Runtime::with_seed(6);
    let waited = rt.block_on(spawned_rt::tasks::timeout(Duration::from_secs(5), async {
        loop {
            sim::yield_now().await;
        }
    }));
    assert!(waited.is_err());
    let stats = rt.stats();
    assert!(stats.now >= Duration::from_secs(5), "{stats:?}");
    println!("5 s spin-wait timed out after {} polls", stats.steps);
}

/// agy L2: a task that keeps its own `Runtime` handle must not keep the
/// world, and the tasks parked in it, alive after `run_once`.
#[test]
fn run_once_drains_a_world_kept_alive_by_a_task() {
    let probe = Arc::new(());
    let weak = Arc::downgrade(&probe);
    let body = move |_| {
        let probe = probe.clone();
        async move {
            let handle = Runtime::new().unwrap();
            drop(spawned_rt::tasks::spawn(async move {
                let _keep = (handle, probe);
                std::future::pending::<()>().await;
            }));
            sim::yield_now().await;
        }
    };
    assert_eq!(sim::run_once(3, None, &body).failure, None);
    drop(body);
    assert_eq!(weak.strong_count(), 0, "parked task leaked with its world");
}

/// Codex (round 2): a strict replay that stops with decisions left over has
/// not reproduced the run, even if it fails; and a bare `Runtime::replay`
/// that finishes with decisions left over fails too.
#[test]
fn strict_replay_rejects_leftover_decisions_after_a_failure_and_on_bare_replay() {
    let failing = |_| async {
        small_schedule().await;
        panic!("planted failure");
    };
    let recorded = sim::run_once(2, None, &failing);
    assert_eq!(recorded.failure.as_deref(), Some("planted failure"));
    let mut long = recorded.tape.clone();
    long.decisions.push(sim::Decision::Canonical(None));
    let failure = sim::run_once(2, Some(&long), &failing)
        .failure
        .unwrap_or_default();
    assert!(failure.contains("replay diverged"), "{failure}");
    assert!(failure.contains("planted failure"), "{failure}");

    let ok = sim::run_once(3, None, &|_| small_schedule());
    let mut long = ok.tape.clone();
    long.decisions.push(sim::Decision::Canonical(None));
    let rt = Runtime::with_seed(3);
    rt.replay(&long);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        rt.block_on(small_schedule())
    }));
    let message = *result.unwrap_err().downcast::<String>().unwrap();
    assert!(message.contains("1 tape decisions unused"), "{message}");
}

/// A root that sleeps 10 ms, then 20 ms more.
async fn two_sleeps() {
    spawned_rt::tasks::sleep(Duration::from_millis(10)).await;
    spawned_rt::tasks::sleep(Duration::from_millis(20)).await;
}

/// Codex/agy (round 2): strict replay checks clock jumps against the pending
/// timers (no backward jump, no jump past the earliest timer, a jump to it
/// when idle) and canonical picks against the recorded task id.
#[test]
fn strict_replay_validates_clock_jumps_and_canonical_ids() {
    let body = |_| two_sleeps();
    let recorded = sim::run_once(1, None, &body);
    assert_eq!(recorded.failure, None);
    assert_eq!(sim::run_once(1, Some(&recorded.tape), &body).failure, None);
    let fires: Vec<usize> = recorded
        .tape
        .decisions
        .iter()
        .enumerate()
        .filter_map(|(i, d)| matches!(d, sim::Decision::Fire(_)).then_some(i))
        .collect();
    assert_eq!(fires.len(), 2, "{:?}", recorded.tape);
    let edited = |at: usize, decision: sim::Decision| {
        let mut tape = recorded.tape.clone();
        tape.decisions[at] = decision;
        sim::run_once(1, Some(&tape), &body)
            .failure
            .unwrap_or_default()
    };
    for (at, decision, expected) in [
        (
            fires[0],
            sim::Decision::Fire(Duration::from_millis(15)),
            "past the earliest",
        ),
        (
            fires[0],
            sim::Decision::Fire(Duration::from_millis(5)),
            "while no task is ready",
        ),
        (
            fires[1],
            sim::Decision::Fire(Duration::from_millis(1)),
            "moves the clock back",
        ),
        (
            0,
            sim::Decision::Canonical(Some(77)),
            "polls task 77 as the canonical choice",
        ),
    ] {
        let failure = edited(at, decision);
        assert!(
            failure.contains("replay diverged"),
            "{decision:?}: {failure}"
        );
        assert!(failure.contains(expected), "{decision:?}: {failure}");
    }
}

/// Codex (round 2): a timer that keeps firing during a spin-wait (a 1 ms
/// sleeper) must not keep the clock creeping at 1 ms per 10,000 polls: the
/// 5 s timeout still fires before the livelock guard.
#[test]
fn spin_wait_times_out_while_another_timer_keeps_firing() {
    let rt = Runtime::with_seed(6);
    let waited = rt.block_on(async {
        let ticker = spawned_rt::tasks::spawn(async {
            loop {
                spawned_rt::tasks::sleep(Duration::from_millis(1)).await;
            }
        });
        let waited = spawned_rt::tasks::timeout(Duration::from_secs(5), async {
            loop {
                sim::yield_now().await;
            }
        })
        .await;
        ticker.abort();
        waited
    });
    assert!(waited.is_err());
    let stats = rt.stats();
    assert!(stats.now >= Duration::from_secs(5), "{stats:?}");
    println!(
        "5 s spin-wait beside a 1 ms ticker timed out after {} polls",
        stats.steps
    );
}

/// agy (round 2): replaying on a world that already ran rebuilds its Tokio
/// runtime with the tape's seed, so `select!` takes the recorded branches.
#[test]
fn replay_on_a_used_world_reseeds_tokio() {
    async fn branches() -> Vec<bool> {
        let mut picks = Vec::new();
        for _ in 0..16 {
            picks.push(tokio::select! {
                _ = async {} => true,
                _ = async {} => false,
            });
            sim::yield_now().await;
        }
        picks
    }
    let fresh = Runtime::with_seed(2);
    fresh.record_tape();
    let expected = fresh.block_on(branches());
    let tape = fresh.tape();

    let used = Runtime::with_seed(1);
    let other = used.block_on(branches());
    assert_ne!(other, expected, "seeds 1 and 2 should differ");
    used.replay(&tape);
    assert_eq!(used.block_on(branches()), expected);
}

/// Codex (round 2): the shrinker never spends more runs than its budget
/// (a candidate costs a permissive and a strict run).
#[test]
fn shrink_stays_within_its_run_budget() {
    let body = |_| unstarted_children_end_stopped();
    let failing = (0..200)
        .map(|seed| sim::run_once(seed, None, &body))
        .find(|outcome| outcome.failure.is_some())
        .expect("no failing seed");
    let failure = failing.failure.clone().unwrap();
    for budget in [0, 1, 2, 3, 7] {
        let (_, runs) = sim::shrink(&failing.tape, &failure, &body, budget);
        assert!(runs <= budget, "budget {budget}: spent {runs}");
    }
}
