//! Simulator turn budget (ROADMAP rank 8b): a run fails when one actor turn
//! runs longer than `SPAWNED_SIM_TURN_BUDGET` inside a single poll, naming
//! the actor and the message type. Run with:
//!
//! ```sh
//! RUSTFLAGS="--cfg spawned_sim --cfg tokio_unstable" cargo test -p spawned-concurrency --test sim_turn_budget
//! ```
#![cfg(all(spawned_sim, feature = "async"))]

use spawned_concurrency::{
    message::Message,
    tasks::{Actor, ActorStart, Context, Handler},
};
use spawned_rt::tasks::{sim, SimTest};
use std::time::{Duration, Instant};

/// Budget used by the in-process tests; the blocking sections run well past
/// it and the yielding ones stay well under it.
const BUDGET: Duration = Duration::from_millis(200);
const OVER: Duration = Duration::from_millis(500);

struct Worker;
impl Actor for Worker {}

/// Blocks the thread (`std::thread::sleep`) for `OVER`.
struct Nap;
impl Message for Nap {
    type Result = ();
}
impl Handler<Nap> for Worker {
    async fn handle(&mut self, _msg: Nap, _ctx: &Context<Self>) {
        std::thread::sleep(OVER);
    }
}

/// Spins on the CPU for `OVER` with no await point.
struct Spin;
impl Message for Spin {
    type Result = u64;
}
impl Handler<Spin> for Worker {
    async fn handle(&mut self, _msg: Spin, _ctx: &Context<Self>) -> u64 {
        let start = Instant::now();
        let mut n = 0u64;
        while start.elapsed() < OVER {
            n = std::hint::black_box(n.wrapping_add(1));
        }
        n
    }
}

/// The same total work as `Nap`, in short stretches separated by awaits, plus
/// a long virtual sleep.
struct Chunked;
impl Message for Chunked {
    type Result = ();
}
impl Handler<Chunked> for Worker {
    async fn handle(&mut self, _msg: Chunked, _ctx: &Context<Self>) {
        for _ in 0..25 {
            std::thread::sleep(Duration::from_millis(20));
            sim::yield_now().await;
        }
        sim::sleep(Duration::from_secs(1)).await;
    }
}

#[derive(Clone, Copy)]
enum Work {
    Nap,
    Spin,
    Chunked,
}

/// One simulated run: start a worker, send it `work` as a request, return the
/// run's failure, if any.
fn run(budget: Option<Duration>, work: Work) -> Option<String> {
    let body = move |_seed| async move {
        sim::set_turn_budget(budget);
        let worker = Worker.start();
        match work {
            Work::Nap => worker.request(Nap).await.unwrap(),
            Work::Spin => {
                worker.request(Spin).await.unwrap();
            }
            Work::Chunked => worker.request(Chunked).await.unwrap(),
        }
    };
    sim::run_once(1, None, &body).failure
}

fn assert_names(failure: &str, message: &str) {
    assert!(failure.contains("turn over budget"), "{failure}");
    assert!(
        failure.contains("actor `sim_turn_budget::Worker`"),
        "the failure names the actor: {failure}"
    );
    assert!(
        failure.contains(&format!("handling `sim_turn_budget::{message}`")),
        "the failure names the message type: {failure}"
    );
    assert!(failure.contains("more than 200 ms"), "{failure}");
}

#[test]
fn blocking_handler_fails_naming_actor_and_message() {
    let failure = run(Some(BUDGET), Work::Nap).expect("a blocking turn must fail the run");
    assert_names(&failure, "Nap");
}

#[test]
fn spinning_handler_fails_naming_actor_and_message() {
    let failure = run(Some(BUDGET), Work::Spin).expect("a spinning turn must fail the run");
    assert_names(&failure, "Spin");
}

/// The budget is per stretch between awaits, not per turn: a handler that
/// does the same total work in short stretches passes, and virtual sleeps
/// cost no wall-clock time.
#[test]
fn handler_that_awaits_passes() {
    assert_eq!(run(Some(BUDGET), Work::Chunked), None);
}

#[test]
fn disabled_budget_lets_a_blocking_handler_pass() {
    assert_eq!(run(None, Work::Nap), None);
}

/// Run by `env_var_tunes_and_disables_the_check` in a child process.
#[test]
#[ignore = "run in a child process by env_var_tunes_and_disables_the_check"]
fn blocking_child() {
    SimTest::new()
        .named(env!("CARGO_PKG_NAME"), module_path!(), "blocking_child")
        .seeds(0..1)
        .run(|_seed| async {
            Worker.start().request(Nap).await.unwrap();
        });
}

/// `SPAWNED_SIM_TURN_BUDGET` end to end, through `SimTest`: the default
/// budget fails a 500 ms blocking turn and the report carries the seed and
/// tape replay lines; a larger budget or 0 lets it pass.
#[test]
fn env_var_tunes_and_disables_the_check() {
    let tapes = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("sim-turn-budget");
    let child = |budget: Option<&str>| {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args(["blocking_child", "--exact", "--ignored", "--nocapture"])
            .env("SPAWNED_SIM_SHRINK", "0")
            .env("SPAWNED_SIM_TAPE_DIR", &tapes)
            .env_remove("SPAWNED_SIM_SEED")
            .env_remove("SPAWNED_SIM_TAPE")
            .env_remove("SPAWNED_SIM_STRATEGY");
        match budget {
            Some(budget) => command.env("SPAWNED_SIM_TURN_BUDGET", budget),
            None => command.env_remove("SPAWNED_SIM_TURN_BUDGET"),
        };
        let output = command.output().expect("cannot run the test binary");
        (
            output.status.success(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    };

    let (passed, stderr) = child(None);
    assert!(!passed, "the default budget must fail the run:\n{stderr}");
    assert!(
        stderr.contains("actor `sim_turn_budget::Worker` handling `sim_turn_budget::Nap`"),
        "{stderr}"
    );
    assert!(stderr.contains("more than 250 ms"), "{stderr}");
    assert!(
        stderr.contains("replay seed:") && stderr.contains("SPAWNED_SIM_SEED=0"),
        "{stderr}"
    );
    assert!(stderr.contains("replay tape:"), "{stderr}");

    let (passed, stderr) = child(Some("0"));
    assert!(passed, "SPAWNED_SIM_TURN_BUDGET=0 must disable the check:\n{stderr}");
    let (passed, stderr) = child(Some("5000"));
    assert!(passed, "a 5 s budget must let a 500 ms turn pass:\n{stderr}");
}
