//! Lifecycle traces for model states the other trace suites never reached.
//!
//! `verification/tla/trace_coverage.py` lists the actions, projected states
//! and transitions of the TLA+ models that no recorded trace reaches. Each
//! test here drives a real supervisor into one of the highest-ranked gaps,
//! records its trace like `supervision_trace.rs` / `tree_trace.rs`, and
//! `trace_check.py` checks the trace against the model in CI. The key each
//! test targets is named in its doc comment.
//!
//! Needs `lifecycle-trace` and `failpoints` (one site is armed); every test
//! holds a `FailScenario`, which serializes them.
#![cfg(all(feature = "lifecycle-trace", feature = "failpoints"))]

use spawned_concurrency::{
    failpoints::{self, Action, FailPoint, FailScenario},
    lifecycle_trace,
    message::Message,
    supervision::{
        Backoff, ChildSpec, ChildState, RestartMode, SupervisedChild, SupervisorConfig,
        SupervisorError, SupervisorSpec,
    },
    system::ActorSystem,
    tasks::{Actor, Context, Handler},
};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[path = "support/tree_record.rs"]
mod tree_record;

const LIMIT: Duration = Duration::from_secs(4);

fn config(max_restarts: usize) -> SupervisorConfig {
    SupervisorConfig::default()
        .max_restarts(max_restarts)
        .backoff(
            Backoff::default()
                .initial(Duration::from_millis(1))
                .maximum(Duration::from_millis(4))
                .jitter(false)
                .seed(7),
        )
}

fn mode_name(mode: RestartMode) -> &'static str {
    match mode {
        RestartMode::Permanent => "Permanent",
        RestartMode::Transient => "Transient",
        RestartMode::Temporary => "Temporary",
    }
}

/// Write a finished supervisor's trace as `coverage-<name>.json` for
/// trace_check.py (same format as supervision_trace.rs).
fn write_trace(name: &str, trace_id: u64, modes: &[RestartMode], max_restarts: usize) {
    let events = lifecycle_trace::take(trace_id);
    assert_eq!(
        events.last().map(|e| e.action),
        Some("SupJoin"),
        "{name}: supervisor did not finish"
    );
    let modes: Vec<String> = modes
        .iter()
        .map(|m| format!("\"{}\"", mode_name(*m)))
        .collect();
    let events: Vec<String> = events.iter().map(|e| e.to_json()).collect();
    let json = format!(
        "{{\"name\":\"coverage-{name}\",\"n\":{},\"modes\":[{}],\"max_restarts\":{max_restarts},\"events\":[\n{}\n]}}\n",
        modes.len(),
        modes.join(","),
        events.join(",\n")
    );
    let dir = std::env::var_os("LIFECYCLE_TRACE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/lifecycle-traces")
        });
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("coverage-{name}.json")), json).unwrap();
}

struct Crash;
impl Message for Crash {
    type Result = ();
}
struct Worker;
impl Actor for Worker {}
impl Handler<Crash> for Worker {
    async fn handle(
        &mut self,
        _: Crash,
        _reply: spawned_concurrency::reply::PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
        panic!("injected child failure");
    }
}

fn worker(name: &str, group: &spawned_concurrency::system::ExecutionGroup) -> ChildSpec<Worker> {
    ChildSpec::new(name, group, |_| async { Ok::<_, String>(Worker) })
}

async fn crash(child: &SupervisedChild<Worker>) {
    let actor = child.current().unwrap();
    assert!(actor.request(Crash).await.is_err());
}

async fn until(what: &str, mut done: impl FnMut() -> bool) {
    let began = Instant::now();
    while !done() {
        assert!(began.elapsed() < LIMIT, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

/// `A|Attempt/closed`: admission closes between a controller's first
/// `LoopTop` and its admission check. The second child's group reservation
/// is held (failpoint `system.reserve`, a synchronous delay) while the
/// start future is dropped, which closes the supervisor; the reservation
/// then sees the closed gate and the attempt is abandoned.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn first_attempt_abandoned_when_startup_is_dropped() {
    let _scenario = FailScenario::setup();
    failpoints::configure(
        "system.reserve",
        FailPoint::new(Action::Delay(Duration::from_millis(300)))
            .skip(1)
            .times(1),
    )
    .unwrap();
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("cov-attempt-closed", 2).unwrap();
    let modes = [RestartMode::Permanent, RestartMode::Permanent];
    let mut tree = system.supervisor(config(5)).unwrap();
    let first = tree.add(worker("first", &group)).unwrap();
    let second = tree.add(worker("second", &group)).unwrap();
    let trace_id = tree.trace_id();
    let start = tokio::spawn(tree.start());
    until("the second reservation", || {
        failpoints::stats("system.reserve").fired >= 1
    })
    .await;
    start.abort();
    let _ = start.await;
    assert!(system.shutdown(LIMIT).await.complete);
    assert_eq!(first.snapshot().generation, 1);
    assert_eq!(second.snapshot().generation, 0);
    write_trace("first_attempt_closed", trace_id, &modes, 5);
}

/// `T|Close|..|deferred|starting|open`, then `Undefer` and `Decide/closed`:
/// a ready child's exit during the initial start is deferred (S1) and the
/// caller gives up on the startup (drops the start future) while it waits.
/// (v0.7 reached `T|Close|..|backoff|starting|open` here through a retried
/// first start, which v0.8 no longer makes.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_cancelled_while_a_ready_exit_is_deferred() {
    let _scenario = FailScenario::setup();
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("cov-start-deferred", 2).unwrap();
    let modes = [RestartMode::Permanent, RestartMode::Transient];
    let (_open, gate) = tokio::sync::watch::channel(false);
    let mut tree = system.supervisor(config(5)).unwrap();
    let early = tree.add(worker("early", &group)).unwrap();
    let later = tree
        .add(
            ChildSpec::new("later", &group, move |_| {
                let mut gate = gate.clone();
                async move {
                    let _ = gate.wait_for(|open| *open).await;
                    Ok::<_, String>(Worker)
                }
            })
            .restart(modes[1]),
        )
        .unwrap();
    let trace_id = tree.trace_id();
    let start = tokio::spawn(tree.start());
    until("the later child under construction", || {
        later.snapshot().state == ChildState::Starting
    })
    .await;
    crash(&early).await;
    until("the deferred exit", || {
        lifecycle_trace::recorded(trace_id, "DeferExit")
    })
    .await;
    start.abort();
    assert!(start.await.is_err());
    assert!(system.shutdown(LIMIT).await.complete);
    assert_eq!(early.snapshot().generation, 1);
    write_trace("startup_cancelled_while_deferred", trace_id, &modes, 5);
}

/// Shutdown begins while a restart is under construction and its deadline
/// then expires: `T|Close|..|constructing|await|open`,
/// `T|SupStopTurn|..|constructing|shutdown|closed`,
/// `T|StartupFailed/none|..|waitchild|closed` and
/// `T|ShutdownDeadline|..|cleanup|waitchild|closed`. The first child crashes
/// and its replacement is still being constructed when shutdown begins;
/// closing cancels the construction while the supervisor waits on the last
/// child, whose `stopped()` hangs past the caller's 20 ms deadline.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restart_in_construction_when_shutdown_begins_and_deadline_expires() {
    let _scenario = FailScenario::setup();
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("cov-deadline-restart", 2).unwrap();
    let modes = [RestartMode::Permanent, RestartMode::Permanent];
    let (release, release_rx) = tokio::sync::oneshot::channel();
    let slot = Arc::new(Mutex::new(Some(release_rx)));
    let mut tree = system.supervisor(config(5)).unwrap();
    let restarting = tree
        .add(ChildSpec::new("restarting", &group, |ctx| async move {
            if ctx.generation() > 1 {
                tokio::time::sleep(Duration::from_millis(400)).await;
            }
            Ok::<_, String>(Worker)
        }))
        .unwrap();
    tree.add(
        ChildSpec::new("slow-stop", &group, move |_| {
            let release = slot.lock().unwrap_or_else(|p| p.into_inner()).take();
            async move { Ok::<_, String>(SlowStop { release }) }
        })
        .shutdown_timeout(Duration::from_millis(100)),
    )
    .unwrap();
    let supervisor = tree.start().await.unwrap();
    crash(&restarting).await;
    until("the replacement under construction", || {
        let s = restarting.snapshot();
        s.generation == 2 && s.state == ChildState::Starting
    })
    .await;
    let report = supervisor.shutdown(Duration::from_millis(20)).await;
    assert!(!report.complete);
    tokio::time::sleep(Duration::from_millis(300)).await;
    release.send(()).unwrap();
    tokio::time::timeout(LIMIT, supervisor.join())
        .await
        .unwrap();
    assert!(restarting.current().is_err());
    write_trace("restart_racing_deadline", supervisor.trace_id(), &modes, 5);
    assert!(system.shutdown(LIMIT).await.complete);
}

struct SlowStop {
    release: Option<tokio::sync::oneshot::Receiver<()>>,
}
impl Actor for SlowStop {
    async fn started(&mut self, ctx: &Context<Self>) {
        // v0.8: stopped() runs on a parent's shutdown only for a trapping
        // actor (E3, decision 2).
        ctx.trap_exit(true);
    }
    async fn stopped(&mut self, _: &Context<Self>) {
        if let Some(release) = self.release.take() {
            let _ = release.await;
        }
    }
}

/// A Transient child that stops `Normal` in its first `started()`, after an
/// earlier sibling is ready: `T|StartupFailed/Normal|Transient|starting|open`
/// then `T|Decide/fail|Transient|starting|open`. Before initial readiness the
/// restart table applies, so the start fails and the ready sibling is rolled
/// back (semantics V7; OTP fails a supervisor's start when a child's `init`
/// stops).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transient_child_stopping_in_first_started_fails_the_start() {
    let _scenario = FailScenario::setup();
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("cov-startup-normal", 1).unwrap();
    let modes = [RestartMode::Permanent, RestartMode::Transient];
    let mut tree = system.supervisor(config(5)).unwrap();
    let first = tree.add(worker("first", &group)).unwrap();
    let quitter = tree
        .add(
            ChildSpec::new("quitter", &group, |_| async move {
                Ok::<_, String>(QuitInStarted { quit: true })
            })
            .restart(modes[1]),
        )
        .unwrap();
    let trace_id = tree.trace_id();
    assert!(tokio::time::timeout(LIMIT, tree.start())
        .await
        .unwrap()
        .is_err());
    assert!(system.shutdown(LIMIT).await.complete);
    assert_eq!(first.snapshot().generation, 1);
    assert_eq!(quitter.snapshot().generation, 1);
    write_trace("transient_first_start_exits_normally", trace_id, &modes, 5);
}

struct QuitInStarted {
    quit: bool,
}
impl Actor for QuitInStarted {
    async fn started(&mut self, ctx: &Context<Self>) {
        if self.quit {
            ctx.stop();
        }
    }
}
impl Handler<Crash> for QuitInStarted {
    async fn handle(
        &mut self,
        _: Crash,
        _reply: spawned_concurrency::reply::PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
        panic!("injected child failure");
    }
}

/// A ready child crashes while a later sibling is still being constructed
/// during initial startup (`T|SelfExit/Panic|..|starting|open`); its exit is
/// deferred (S1) and it restarts once the tree commits ready.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ready_child_exit_deferred_while_a_sibling_is_still_starting() {
    let _scenario = FailScenario::setup();
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("cov-start-sibling", 2).unwrap();
    let modes = [RestartMode::Permanent, RestartMode::Transient];
    let (open, gate) = tokio::sync::watch::channel(false);
    let mut tree = system.supervisor(config(5)).unwrap();
    let early = tree.add(worker("early", &group)).unwrap();
    let late = tree
        .add(
            ChildSpec::new("late", &group, move |_| {
                let mut gate = gate.clone();
                async move {
                    let _ = gate.wait_for(|open| *open).await;
                    Ok::<_, String>(Worker)
                }
            })
            .restart(modes[1]),
        )
        .unwrap();
    let trace_id = tree.trace_id();
    let start = tokio::spawn(tree.start());
    until("the late child under construction", || {
        late.snapshot().state == ChildState::Starting
    })
    .await;
    crash(&early).await;
    until("the deferred exit", || {
        lifecycle_trace::recorded(trace_id, "DeferExit")
    })
    .await;
    assert_eq!(early.snapshot().generation, 1);
    open.send_replace(true);
    let supervisor = tokio::time::timeout(LIMIT, start)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    tokio::time::timeout(LIMIT, early.wait_for_generation(1))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(early.snapshot().generation, 2);
    assert!(supervisor.shutdown(LIMIT).await.complete);
    write_trace("restart_during_startup", supervisor.trace_id(), &modes, 5);
    assert!(system.shutdown(LIMIT).await.complete);
}

/// A leaf two levels down restarts in place under its own supervisor
/// (`T|TBegin/restart|child|started|parent:started`): the existing nested
/// trace escalates every leaf failure to a subtree replacement.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nested_leaf_restarts_inside_its_subtree() {
    let _scenario = FailScenario::setup();
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("cov-nested-restart", 2).unwrap();
    let leaf: Arc<Mutex<Option<SupervisedChild<Worker>>>> = Arc::new(Mutex::new(None));
    let expose = leaf.clone();
    let placement = group.clone();
    let mut root = system.supervisor(config(5)).unwrap();
    root.add_supervisor(SupervisorSpec::new(
        "branch",
        &group,
        config(5),
        move |branch| {
            *expose.lock().unwrap() = Some(branch.add(worker("leaf", &placement))?);
            Ok::<_, SupervisorError>(())
        },
    ))
    .unwrap();
    let supervisor = root.start().await.unwrap();
    let leaf = leaf.lock().unwrap().take().unwrap();
    crash(&leaf).await;
    tokio::time::timeout(LIMIT, leaf.wait_for_generation(1))
        .await
        .unwrap()
        .unwrap();
    assert!(supervisor.shutdown(LIMIT).await.complete);
    assert!(system.shutdown(LIMIT).await.complete);
    tree_record::record("coverage-nested-leaf-restart", &supervisor);
}
