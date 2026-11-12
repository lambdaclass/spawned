//! Seeded test harness that runs one test body on Tokio in normal builds and
//! over a range of seeds on the deterministic simulator under
//! `--cfg spawned_sim`. Use it directly, or through `#[spawned_macros::sim_test]`.
//!
//! ```ignore
//! spawned_rt::tasks::sim_test(|seed| async move {
//!     // test body
//! });
//! ```
//!
//! Environment (simulation builds only):
//!
//! | Variable | Effect |
//! |---|---|
//! | `SPAWNED_SIM_SEED=n` | run only seed `n` (also the seed passed to the body on Tokio) |
//! | `SPAWNED_SIM_SEEDS=n` | run seeds `0..n` (default 8) |
//! | `SPAWNED_SIM_TAPE=path` | replay a recorded decision tape instead of seeds |
//! | `SPAWNED_SIM_TAPE_DIR=dir` | where failing tapes are written (default `$TMPDIR/spawned-sim`) |
//! | `SPAWNED_SIM_SHRINK=n` | runs the tape shrinker may spend on a failure (default 200, 0 = off) |
//! | `SPAWNED_SIM_STRATEGY=s` | `random` (default), `pct[:d=3][,k=N]` or `dfs[:bound=2][,depth=N]` (see `sim::explore`); overrides the test's `explore` |
//! | `SPAWNED_SIM_ITERATIONS=n` | runs for that strategy (default 8 for random, 100 otherwise); overrides the test's `iterations` |
//! | `SPAWNED_SIM_FAIR_AFTER=n` | polls after which a PCT/DFS run switches to a fair scheduler (default 20,000) |
//! | `SPAWNED_SIM_TURN_BUDGET=ms` | wall-clock budget of one actor turn inside a poll (default 250, 0 = off) |

use std::future::Future;
use std::ops::Range;

/// Builder for one seeded test. See the module docs.
#[derive(Debug, Clone, Default)]
pub struct SimTest {
    package: Option<String>,
    module: Option<String>,
    function: Option<String>,
    seeds: Option<Range<u64>>,
    workers: Option<usize>,
    explore: Option<String>,
    iterations: Option<usize>,
}

/// Run `body` as a seeded test with the default settings.
pub fn sim_test<F, Fut>(body: F)
where
    F: Fn(u64) -> Fut + Sync,
    Fut: Future<Output = ()>,
{
    SimTest::new().run(body)
}

fn env_u64(name: &str) -> Option<u64> {
    std::env::var(name).ok().and_then(|v| v.trim().parse().ok())
}

impl SimTest {
    pub fn new() -> Self {
        Self::default()
    }
    /// Where the test lives, for the replay command: the package name, the
    /// test's `module_path!()` and its function name.
    pub fn named(mut self, package: &str, module: &str, function: &str) -> Self {
        self.package = Some(package.into());
        self.module = Some(module.into());
        self.function = Some(function.into());
        self
    }
    /// Seeds to run under simulation (default `0..SPAWNED_SIM_SEEDS`, or 0..8).
    pub fn seeds(mut self, seeds: Range<u64>) -> Self {
        self.seeds = Some(seeds);
        self
    }
    /// Under simulation, choose tasks with this strategy (`random`,
    /// `pct[:d=3][,k=N]`, `dfs[:bound=2][,depth=N]`); `SPAWNED_SIM_STRATEGY`
    /// overrides it. Ignored on Tokio.
    pub fn explore(mut self, strategy: &str) -> Self {
        self.explore = Some(strategy.into());
        self
    }
    /// Under simulation, how many runs the strategy may spend (for `random`,
    /// seeds `0..n` unless [`SimTest::seeds`] is set); `SPAWNED_SIM_ITERATIONS`
    /// overrides it. Ignored on Tokio.
    pub fn iterations(mut self, runs: usize) -> Self {
        self.iterations = Some(runs);
        self
    }
    /// On Tokio, use a multi-thread runtime with `workers` workers (the
    /// default is a current-thread runtime, as `#[tokio::test]`). Ignored
    /// under simulation, which always has one thread.
    pub fn multi_thread(mut self, workers: usize) -> Self {
        self.workers = Some(workers);
        self
    }

    /// First seed and number of runs for `strategy`. `SPAWNED_SIM_SEED`
    /// (`seed`) fixes the body's input: a single run for random and PCT, and
    /// the input of every schedule for DFS (which varies the schedule, not
    /// the seed).
    #[cfg(spawned_sim)]
    fn runs(
        &self,
        strategy: &super::sim::explore::Strategy,
        seed: Option<u64>,
        iterations: Option<usize>,
    ) -> (u64, usize) {
        use super::sim::explore::Strategy;
        match (strategy, seed) {
            (Strategy::Dfs { .. }, seed) => (seed.unwrap_or(0), iterations.unwrap_or(100)),
            (_, Some(seed)) => (seed, 1),
            (Strategy::Random, None) => match (&self.seeds, iterations) {
                (Some(seeds), _) => (seeds.start, seeds.end.saturating_sub(seeds.start) as usize),
                (None, Some(n)) => (0, n),
                (None, None) => (0, env_u64("SPAWNED_SIM_SEEDS").unwrap_or(8) as usize),
            },
            (Strategy::Pct { .. }, None) => (0, iterations.unwrap_or(100)),
        }
    }

    /// The libtest name (`path::to::test`) and a `cargo test` selector.
    #[cfg_attr(not(spawned_sim), allow(dead_code))]
    fn locate(&self) -> (String, String) {
        let thread = std::thread::current();
        let fallback = thread.name().unwrap_or("test").to_string();
        let (Some(package), Some(module), Some(function)) =
            (&self.package, &self.module, &self.function)
        else {
            return (
                fallback.clone(),
                format!("cargo test -- {fallback} --exact"),
            );
        };
        let mut segments = module.split("::");
        let krate = segments.next().unwrap_or_default();
        let path: Vec<&str> = segments.chain(std::iter::once(function.as_str())).collect();
        let path = path.join("::");
        let target = if krate == package.replace('-', "_") {
            "--lib".to_string()
        } else {
            format!("--test {krate}")
        };
        let command = format!("cargo test -p {package} {target} -- {path} --exact");
        (path, command)
    }

    #[cfg(not(spawned_sim))]
    pub fn run<F, Fut>(self, body: F)
    where
        F: Fn(u64) -> Fut + Sync,
        Fut: Future<Output = ()>,
    {
        let seed = env_u64("SPAWNED_SIM_SEED").unwrap_or(0);
        let runtime = match self.workers {
            Some(workers) => ::tokio::runtime::Builder::new_multi_thread()
                .worker_threads(workers)
                .enable_all()
                .build(),
            None => ::tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build(),
        }
        .expect("failed to build the test runtime");
        runtime.block_on(body(seed));
    }

    #[cfg(spawned_sim)]
    pub fn run<F, Fut>(self, body: F)
    where
        F: Fn(u64) -> Fut + Sync,
        Fut: Future<Output = ()>,
    {
        use super::sim::explore::{explore, Strategy};
        use super::sim::{run_once, shrink, Tape};
        let (name, command) = self.locate();
        // Scheduling settings that change what a seed does travel with the
        // replay command (a tape records its own).
        let settings: String = [
            "SPAWNED_SIM_SPIN",
            "SPAWNED_SIM_MAX_STEPS",
            "SPAWNED_SIM_FAIR_AFTER",
            "SPAWNED_SIM_TURN_BUDGET",
        ]
        .iter()
        .filter_map(|key| std::env::var(key).ok().map(|v| format!("{key}={v} ")))
        .collect();
        let replay = |prefix: &str| {
            format!(
                "{settings}{prefix} RUSTFLAGS=\"--cfg spawned_sim --cfg tokio_unstable\" {command}"
            )
        };

        if let Ok(path) = std::env::var("SPAWNED_SIM_TAPE") {
            let tape = Tape::load(path.as_ref())
                .unwrap_or_else(|e| panic!("spawned_sim: cannot load tape {path}: {e}"));
            let outcome = run_once(tape.seed, Some(&tape), &body);
            // Strict: a tape that no longer applies fails as `replay diverged`.
            eprintln!(
                "spawned_sim: `{name}` replayed tape {path} (seed {}, {} polls)",
                tape.seed, outcome.stats.steps
            );
            if let Some(failure) = outcome.failure {
                panic!("spawned_sim: `{name}` failed replaying tape {path}: {failure}");
            }
            return;
        }

        let spec = std::env::var("SPAWNED_SIM_STRATEGY")
            .ok()
            .or(self.explore.clone())
            .unwrap_or_else(|| "random".into());
        let strategy = Strategy::parse(&spec)
            .unwrap_or_else(|e| panic!("spawned_sim: `{name}`: bad strategy: {e}"));
        let iterations = env_u64("SPAWNED_SIM_ITERATIONS")
            .map(|n| n as usize)
            .or(self.iterations);
        let (first, runs) = self.runs(&strategy, env_u64("SPAWNED_SIM_SEED"), iterations);
        let report = explore(&strategy, first, runs, &body);
        if report.divergences > 0 {
            panic!(
                "spawned_sim: `{name}`: {} of {} {strategy} runs did not replay their \
                 recorded prefix: the body is not deterministic under the simulator",
                report.divergences, report.runs
            );
        }
        let Some(found) = report.found else {
            if strategy != Strategy::Random {
                let space = describe_space(&strategy, report.exhausted, report.truncated);
                eprintln!(
                    "spawned_sim: `{name}` passed {} runs of {strategy} ({} distinct schedules{space})",
                    report.runs, report.schedules,
                );
            }
            return;
        };
        let (seed, outcome) = (found.seed, found.outcome);
        let failure = outcome.failure.clone().unwrap_or_default();
        let dir = std::env::var_os("SPAWNED_SIM_TAPE_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("spawned-sim"));
        let stem: String = name
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { '_' })
            .collect();
        let stem = match &found.strategy {
            Strategy::Random => format!("{stem}-seed{seed}"),
            Strategy::Pct { .. } => format!("{stem}-pct-seed{seed}"),
            Strategy::Dfs { .. } => format!("{stem}-dfs-run{}", found.run),
        };
        let full = dir.join(format!("{stem}.tape"));
        let mut report = format!(
            "spawned_sim: `{name}` failed under {} at run {} (seed {seed}) after {} polls at virtual time {:?}: {failure}\n",
            found.strategy, found.run, outcome.stats.steps, outcome.stats.now,
        );
        match &found.strategy {
            Strategy::Random => report.push_str(&format!(
                "  replay seed: {}\n",
                replay(&format!("SPAWNED_SIM_SEED={seed}"))
            )),
            Strategy::Pct { .. } => report.push_str(&format!(
                "  replay seed: {}\n",
                replay(&format!(
                    "SPAWNED_SIM_STRATEGY={} SPAWNED_SIM_SEED={seed}",
                    found.strategy
                ))
            )),
            Strategy::Dfs { .. } => {}
        }
        match save(&outcome.tape, &found.strategy, &full) {
            Ok(()) => report.push_str(&format!(
                "  tape:        {} ({} polls, {} non-canonical)\n  replay tape: {}\n",
                full.display(),
                outcome.tape.polls(),
                outcome.tape.deviations(),
                replay(&format!("SPAWNED_SIM_TAPE={}", full.display())),
            )),
            Err(e) => report.push_str(&format!(
                "  (could not write tape {}: {e})\n",
                full.display()
            )),
        }
        let budget = env_u64("SPAWNED_SIM_SHRINK").unwrap_or(200) as usize;
        if budget > 0 {
            let (small, runs) = shrink(&outcome.tape, &failure, &body, budget);
            let min = dir.join(format!("{stem}.min.tape"));
            if shrunk(&small, &outcome.tape) && save(&small, &found.strategy, &min).is_ok() {
                report.push_str(&format!(
                    "  shrunk tape: {} ({} polls, {} non-canonical after {runs} runs)\n  replay:      {}\n",
                    min.display(),
                    small.polls(),
                    small.deviations(),
                    replay(&format!("SPAWNED_SIM_TAPE={}", min.display())),
                ));
            }
        }
        eprintln!("{report}");
        panic!(
            "spawned_sim: `{name}` failed under {} with seed {seed}: {failure}",
            found.strategy
        );
    }
}

/// Write `tape` with the strategy that produced it as a header comment
/// (informational: a strict replay needs only the tape).
#[cfg(spawned_sim)]
fn save(
    tape: &super::sim::Tape,
    strategy: &super::sim::explore::Strategy,
    path: &std::path::Path,
) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, format!("# strategy {strategy}\n{}", tape.to_text()))
}

/// How much of a strategy's space a passing exploration covered, naming the
/// cutoffs that were active when some run had alternatives past them.
#[cfg(spawned_sim)]
fn describe_space(
    strategy: &super::sim::explore::Strategy,
    exhausted: bool,
    truncated: bool,
) -> String {
    use super::sim::explore::Strategy;
    let mut cutoffs = Vec::new();
    if let Strategy::Dfs {
        depth: Some(depth), ..
    } = strategy
    {
        cutoffs.push(format!("depth {depth}"));
    }
    cutoffs.push(format!("fair after {} polls", strategy.fair_after()));
    let cut = format!(
        "alternatives past the cutoffs ({}) not explored",
        cutoffs.join(", ")
    );
    match (exhausted, truncated) {
        (true, false) => ", bounded space exhausted".into(),
        (true, true) => format!(", bounded space exhausted up to the cutoffs; {cut}"),
        (false, true) => format!(", budget spent; {cut}"),
        (false, false) => String::new(),
    }
}

/// A shrunk tape is worth writing if it has fewer non-canonical picks or is
/// shorter (prefix bisection can shorten a tape without removing a pick).
#[cfg(spawned_sim)]
fn shrunk(small: &super::sim::Tape, original: &super::sim::Tape) -> bool {
    small.deviations() < original.deviations() || small.decisions.len() < original.decisions.len()
}

#[cfg(all(test, spawned_sim))]
mod tests {
    use super::super::sim::explore::Strategy;
    use super::super::sim::{Decision, Tape};
    use super::{shrunk, SimTest};

    /// agy (round 2): the summary names the active cutoff (a DFS depth, not
    /// only the fairness cutoff) and reports truncation even when the budget
    /// ran out before exhaustion.
    #[test]
    fn exploration_summary_names_the_active_cutoffs() {
        use super::describe_space;
        let dfs = Strategy::parse("dfs:bound=2,depth=10,fair=500").unwrap();
        let text = describe_space(&dfs, true, true);
        assert!(
            text.contains("depth 10") && text.contains("fair after 500"),
            "{text}"
        );
        let text = describe_space(&dfs, false, true);
        assert!(
            text.contains("budget spent") && text.contains("not explored"),
            "{text}"
        );
        assert_eq!(
            describe_space(&dfs, true, false),
            ", bounded space exhausted"
        );
    }

    /// Codex P2 (#218): DFS keeps the requested `SPAWNED_SIM_SEED` as the
    /// body's input for every schedule instead of always using 0.
    #[test]
    fn dfs_keeps_the_requested_seed() {
        let dfs = Strategy::parse("dfs:bound=1").unwrap();
        let test = SimTest::new();
        assert_eq!(test.runs(&dfs, Some(42), Some(7)), (42, 7));
        assert_eq!(test.runs(&dfs, None, None), (0, 100));
        let pct = Strategy::parse("pct").unwrap();
        assert_eq!(test.runs(&pct, Some(42), None), (42, 1));
    }

    /// agy L1: a shorter tape with as many non-canonical picks is still a
    /// smaller reproduction.
    #[test]
    fn shorter_tape_with_equal_deviations_counts_as_shrunk() {
        let tape = |n: usize| Tape {
            seed: 1,
            spin: None,
            decisions: std::iter::once(Decision::Poll(3))
                .chain(std::iter::repeat_n(Decision::Canonical(None), n))
                .collect(),
        };
        assert!(shrunk(&tape(2), &tape(9)));
        assert!(!shrunk(&tape(9), &tape(9)));
    }
}
