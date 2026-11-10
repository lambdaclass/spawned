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
}

/// Run `body` as a seeded test with the default settings.
pub fn sim_test<F, Fut>(body: F)
where
    F: Fn(u64) -> Fut,
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
    /// On Tokio, use a multi-thread runtime with `workers` workers (the
    /// default is a current-thread runtime, as `#[tokio::test]`). Ignored
    /// under simulation, which always has one thread.
    pub fn multi_thread(mut self, workers: usize) -> Self {
        self.workers = Some(workers);
        self
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
        F: Fn(u64) -> Fut,
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
        F: Fn(u64) -> Fut,
        Fut: Future<Output = ()>,
    {
        use super::sim::{run_once, shrink, Tape};
        let (name, command) = self.locate();
        let replay = |prefix: &str| format!("{prefix} RUSTFLAGS=\"--cfg spawned_sim\" {command}");

        if let Ok(path) = std::env::var("SPAWNED_SIM_TAPE") {
            let tape = Tape::load(path.as_ref())
                .unwrap_or_else(|e| panic!("spawned_sim: cannot load tape {path}: {e}"));
            let outcome = run_once(tape.seed, Some(&tape), &body);
            eprintln!(
                "spawned_sim: `{name}` replayed tape {path} (seed {}, {} polls, {} scripted picks not ready)",
                tape.seed, outcome.stats.steps, outcome.stats.tape_misses
            );
            if let Some(failure) = outcome.failure {
                panic!("spawned_sim: `{name}` failed replaying tape {path}: {failure}");
            }
            return;
        }

        let seeds = match env_u64("SPAWNED_SIM_SEED") {
            Some(seed) => seed..seed + 1,
            None => self
                .seeds
                .clone()
                .unwrap_or(0..env_u64("SPAWNED_SIM_SEEDS").unwrap_or(8)),
        };
        for seed in seeds {
            let outcome = run_once(seed, None, &body);
            let Some(failure) = outcome.failure else {
                continue;
            };
            let dir = std::env::var_os("SPAWNED_SIM_TAPE_DIR")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| std::env::temp_dir().join("spawned-sim"));
            let stem: String = name
                .chars()
                .map(|c| if c.is_alphanumeric() { c } else { '_' })
                .collect();
            let full = dir.join(format!("{stem}-seed{seed}.tape"));
            let mut report = format!(
                "spawned_sim: `{name}` failed with seed {seed} after {} polls at virtual time {:?}: {failure}\n  \
                 replay seed: {}\n",
                outcome.stats.steps,
                outcome.stats.now,
                replay(&format!("SPAWNED_SIM_SEED={seed}")),
            );
            match outcome.tape.save(&full) {
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
                let min = dir.join(format!("{stem}-seed{seed}.min.tape"));
                if small.deviations() < outcome.tape.deviations() && small.save(&min).is_ok() {
                    report.push_str(&format!(
                        "  shrunk tape: {} ({} non-canonical after {runs} runs)\n  replay:      {}\n",
                        min.display(),
                        small.deviations(),
                        replay(&format!("SPAWNED_SIM_TAPE={}", min.display())),
                    ));
                }
            }
            eprintln!("{report}");
            panic!("spawned_sim: `{name}` failed with seed {seed}: {failure}");
        }
    }
}
