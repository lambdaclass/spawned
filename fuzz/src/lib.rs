//! Coverage-guided scenario fuzzing of spawned on the deterministic simulator.
//!
//! A fuzz input decodes (via `arbitrary`) to a [`Scenario`]: a simulator seed,
//! a scheduling strategy and a sequence of operations over actors, links,
//! monitors, timers, supervisors and actor systems. [`run_body`] interprets it
//! inside one simulated world and checks the semantics contract
//! (`docs/design/semantics.md`) as it goes; [`fuzz_one`] runs it under the
//! chosen strategy and aborts with the failure, its decision tape and a replay
//! command when an oracle is violated. See `docs/TESTING.md` ("Scenario
//! fuzzing") for the workflow.
//!
//! Only compiles under `--cfg spawned_sim --cfg tokio_unstable`.

#[cfg(not(spawned_sim))]
compile_error!(
    "spawned-fuzz runs on the simulator: build with \
     RUSTFLAGS=\"--cfg spawned_sim --cfg tokio_unstable\""
);

mod driver;
pub mod scenario;
mod world;

pub use driver::run_body;

/// Violation details (kind: detail) of the last failing run on this thread.
pub fn last_details() -> Vec<String> {
    driver::LAST_DETAILS.with(|d| d.borrow().clone())
}
pub use scenario::*;

use spawned_rt::tasks::sim::{
    self,
    explore::{explore, Strategy},
    Tape,
};
use std::path::PathBuf;

/// The strategy a [`Sched`] stands for (one run).
pub fn strategy(sched: Sched) -> Strategy {
    match sched {
        Sched::Random => Strategy::Random,
        Sched::Pct { depth, steps } => Strategy::Pct {
            depth: 1 + (depth % 4) as u32,
            steps: Some(1 + (steps % 4000) as u64),
            fair_after: None,
        },
        Sched::Canonical => Strategy::Dfs {
            bound: 0,
            depth: None,
            fair_after: None,
        },
    }
}

/// Result of one scenario execution.
#[derive(Debug, Clone)]
pub struct Run {
    pub failure: Option<String>,
    /// Decision tape of the run (always for random runs and failures).
    pub tape: Option<Tape>,
    pub polls: u64,
}

/// Run `scenario` once under its own seed and strategy, or strictly replay
/// `tape` instead.
pub fn execute(scenario: &Scenario, tape: Option<&Tape>) -> Run {
    init();
    let body = |seed: u64| run_body(scenario.clone(), seed);
    if let Some(tape) = tape {
        let outcome = sim::run_once(tape.seed, Some(tape), &body);
        return Run {
            failure: outcome.failure,
            polls: outcome.stats.steps,
            tape: Some(outcome.tape),
        };
    }
    match scenario.sched {
        Sched::Random => {
            let outcome = sim::run_once(scenario.seed, None, &body);
            Run {
                failure: outcome.failure,
                polls: outcome.stats.steps,
                tape: Some(outcome.tape),
            }
        }
        sched => {
            let report = explore(&strategy(sched), scenario.seed, 1, &body);
            match report.found {
                Some(found) => Run {
                    failure: found.outcome.failure,
                    polls: found.outcome.stats.steps,
                    tape: Some(found.outcome.tape),
                },
                None => Run {
                    failure: None,
                    polls: report.max_polls,
                    tape: None,
                },
            }
        }
    }
}

/// The fuzz target: run, and on failure print the scenario, save the tape
/// and abort (libFuzzer then saves the input as a crash).
pub fn fuzz_one(scenario: &Scenario) {
    let run = execute(scenario, None);
    let Some(failure) = run.failure else {
        return;
    };
    eprintln!("spawned-fuzz: FAILED: {failure}");
    for detail in last_details() {
        eprintln!("spawned-fuzz:   {detail}");
    }
    eprintln!("spawned-fuzz: strategy {}, seed {}", strategy(scenario.sched), scenario.seed);
    if let Some(tape) = &run.tape {
        let dir = std::env::var_os("SPAWNED_FUZZ_TAPE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("spawned-fuzz"));
        let path = dir.join(format!("failure-{:016x}.tape", tape_hash(tape)));
        match tape.save(&path) {
            Ok(()) => eprintln!("spawned-fuzz: decision tape {}", path.display()),
            Err(e) => eprintln!("spawned-fuzz: could not save tape {}: {e}", path.display()),
        }
    }
    if std::env::var_os("SPAWNED_FUZZ_SCENARIO").is_some() {
        eprintln!("spawned-fuzz: scenario {scenario:#?}");
    }
    std::process::abort();
}

fn tape_hash(tape: &Tape) -> u64 {
    tape.to_text().bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ b as u64).wrapping_mul(0x0100_0000_01b3)
    })
}

/// Decode a fuzz input the way `libfuzzer_sys::fuzz_target!` does.
pub fn decode(bytes: &[u8]) -> Option<Scenario> {
    use arbitrary::{Arbitrary, Unstructured};
    if bytes.len() < <Scenario as Arbitrary>::size_hint(0).0 {
        return None;
    }
    Scenario::arbitrary_take_rest(Unstructured::new(bytes)).ok()
}

/// Regression check: `input` must pass under its own seed and strategy and
/// under the strict replay of `tape`.
pub fn assert_passes(input: &[u8], tape: Option<&str>) {
    let scenario = decode(input).expect("regression input does not decode");
    let run = execute(&scenario, None);
    assert_eq!(run.failure, None, "scenario fails under its own schedule");
    if let Some(text) = tape {
        let tape = Tape::parse(text).expect("regression tape does not parse");
        let mut failure = execute(&scenario, Some(&tape)).failure;
        // After a fix the recorded schedule may no longer apply exactly
        // (other tasks, other picks): follow it as far as it goes instead.
        if failure.as_deref().is_some_and(|f| f.contains("replay diverged")) {
            let body = |seed: u64| run_body(scenario.clone(), seed);
            failure = sim::run_with(tape.seed, Some((&tape, sim::ReplayMode::Permissive)), &body)
                .failure;
        }
        assert_eq!(failure, None, "scenario fails under its recorded tape");
    }
}

/// Process-wide setup: a quiet panic hook (injected panics are expected and
/// libfuzzer-sys's hook would abort on them) and simulator limits suited to
/// short runs. `SPAWNED_FUZZ_VERBOSE=1` keeps the default hook.
pub fn init() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        if std::env::var_os("SPAWNED_FUZZ_VERBOSE").is_none() {
            std::panic::set_hook(Box::new(|_| {}));
        }
        for (key, value) in [
            // A scenario that needs millions of polls is a livelock.
            ("SPAWNED_SIM_MAX_STEPS", "2000000"),
            // libFuzzer's -timeout bounds wall time; skip the per-poll clock.
            ("SPAWNED_SIM_POLL_TIMEOUT", "0"),
        ] {
            if std::env::var_os(key).is_none() {
                std::env::set_var(key, value);
            }
        }
    });
}
