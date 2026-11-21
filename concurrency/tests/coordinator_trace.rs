//! Records the ActorSystem shutdown coordinator's traces from real runs for
//! TLA+ trace validation against `verification/tla/Coordinator.tla`
//! (verification/tla/README.md). Each scenario writes
//! `coordinator-<name>.json` to `$LIFECYCLE_TRACE_DIR` (default
//! `target/lifecycle-traces`); `verification/tla/trace_check.py` checks them
//! (schema `coordinator-v1`, `TraceCoordinator.tla`).
//!
//! Every recorded event is deterministic: each scenario waits for what it
//! needs (an actor inside `stopped()`, an exit, a completed report) before
//! the next step, so the trace and its coverage do not depend on scheduling.
#![cfg(feature = "lifecycle-trace")]
#[macro_use]
#[path = "support/deadline.rs"]
mod deadline;
use spawned_concurrency::{
    lifecycle_trace,
    supervision::{ChildSpec, SupervisorConfig},
    system::ActorSystem,
    tasks::{Actor, ActorStart, Context},
};
use std::{path::PathBuf, time::Duration};

const LIMIT: Duration = Duration::from_secs(5);

/// Failpoints are process-global: a scenario that panics the coordinator
/// must not run while another system shuts down.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Worker;
impl Actor for Worker {}

/// Take the system's coordinator trace and write it; returns the actions.
fn write_trace(name: &str, system: &ActorSystem, targets: usize) -> Vec<String> {
    let trace = lifecycle_trace::take(system.trace_id());
    assert_eq!(trace.first().map(|e| e.action), Some("Begin"), "{trace:?}");
    assert_eq!(trace[0].count, targets, "{trace:?}");
    assert_eq!(trace.last().map(|e| e.action), Some("Join"), "{trace:?}");
    let events = trace
        .iter()
        .map(|event| event.to_json())
        .collect::<Vec<_>>()
        .join(",\n");
    let directory = std::env::var_os("LIFECYCLE_TRACE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/lifecycle-traces")
        });
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(
        directory.join(format!("coordinator-{name}.json")),
        format!(
            "{{\"schema\":\"coordinator-v1\",\"name\":\"coordinator_{name}\",\"targets\":{targets},\"events\":[\n{events}\n]}}\n"
        ),
    )
    .unwrap();
    trace.iter().map(|e| e.action.to_string()).collect()
}

/// No admitted roots: `Begin`, `Ordered`, `Join`.
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn trace_coordinator_empty_system() {
    let _serial = SERIAL.lock().await;
    let system = ActorSystem::new().unwrap();
    let _group = system.async_group("coordinator-empty", 1).unwrap();
    assert!(system.shutdown(LIMIT).await.complete);
    assert_eq!(
        write_trace("empty", &system, 0),
        ["Begin", "Ordered", "Join"]
    );
}

/// Standalone actors and a supervisor, stopped in reverse admission order,
/// each joined before the next is signalled.
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn trace_coordinator_ordered_shutdown() {
    let _serial = SERIAL.lock().await;
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("coordinator-ordered", 2).unwrap();
    let first = Worker.with_options(&group).spawn().unwrap();
    within!(first.ready())
        .await
        .unwrap()
        .started()
        .expect("started");
    let mut tree = system.supervisor(SupervisorConfig::default()).unwrap();
    tree.add(ChildSpec::new("child", &group, |_| async {
        Ok::<_, String>(Worker)
    }))
    .unwrap();
    let supervisor = within!(tree.start()).await.unwrap();
    let last = Worker.with_options(&group).spawn().unwrap();
    within!(last.ready())
        .await
        .unwrap()
        .started()
        .expect("started");
    assert!(system.shutdown(LIMIT).await.complete);
    assert_eq!(
        write_trace("ordered", &system, 3),
        [
            "Begin", "StopNext", "Finish", "Advance", "StopNext", "Finish", "Advance", "StopNext",
            "Finish", "Advance", "Ordered", "Join"
        ]
    );
    drop(supervisor);
}

/// A trapping root stuck in `stopped()` holds the ordered loop past the
/// grace: the remaining roots get the kill (E3), and runtime ownership is
/// released only once the stuck root finishes.
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn trace_coordinator_grace_expiry_kills_remaining() {
    let _serial = SERIAL.lock().await;
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("coordinator-grace", 1).unwrap();
    let first = Worker.with_options(&group).spawn().unwrap();
    within!(first.ready())
        .await
        .unwrap()
        .started()
        .expect("started");
    let (entered_tx, entered) = tokio::sync::oneshot::channel();
    let (release, release_rx) = tokio::sync::oneshot::channel();
    struct Stuck {
        entered: Option<tokio::sync::oneshot::Sender<()>>,
        release: Option<tokio::sync::oneshot::Receiver<()>>,
    }
    impl Actor for Stuck {
        async fn started(&mut self, ctx: &Context<Self>) {
            ctx.trap_exit(true);
        }
        async fn stopped(&mut self, _: &Context<Self>) {
            let _ = self.entered.take().unwrap().send(());
            let _ = self.release.take().unwrap().await;
        }
    }
    let last = Stuck {
        entered: Some(entered_tx),
        release: Some(release_rx),
    }
    .with_options(&group)
    .spawn()
    .unwrap();
    within!(last.ready())
        .await
        .unwrap()
        .started()
        .expect("started");
    let report = system.shutdown(Duration::from_millis(20)).await;
    assert!(!report.complete);
    within!(entered).await.unwrap();
    within!(first.wait_exit()).await;
    // The kill has been recorded (Emergency) before the stuck root is
    // released: its Finish comes after.
    within!(async {
        while !lifecycle_trace::recorded_with(system.trace_id(), "Emergency", "kill") {
            spawned_rt::tasks::yield_now().await;
        }
    })
    .await;
    release.send(()).unwrap();
    assert!(report.wait(LIMIT).await.complete);
    assert_eq!(
        write_trace("grace", &system, 2),
        [
            "Begin",
            "StopNext",
            "GraceExpired",
            "Emergency",
            "Finish",
            "Finish",
            "Join"
        ]
    );
}

/// The coordinator panics after the first root joined: the failure is
/// reported and the fallback stops every root with Shutdown, then joins.
#[cfg(feature = "failpoints")]
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn trace_coordinator_panic_takes_emergency_fallback() {
    use spawned_concurrency::failpoints::{self, Action, FailPoint, FailScenario};
    let _serial = SERIAL.lock().await;
    let _scenario = FailScenario::setup();
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("coordinator-crash", 1).unwrap();
    let mut actors = Vec::new();
    for _ in 0..3 {
        let actor = Worker.with_options(&group).spawn().unwrap();
        within!(actor.ready())
            .await
            .unwrap()
            .started()
            .expect("started");
        actors.push(actor);
    }
    failpoints::configure(
        "system.shutdown_before_target",
        FailPoint::new(Action::Panic).skip(1).times(1),
    )
    .unwrap();
    let report = system.shutdown(LIMIT).await;
    let report = report.wait(LIMIT).await;
    assert!(report.complete);
    assert!(report
        .coordinator_failures
        .iter()
        .any(|e| e.contains("injected panic")));
    assert_eq!(
        write_trace("crash", &system, 3),
        [
            "Begin",
            "StopNext",
            "Finish",
            "Advance",
            "Crash",
            "Emergency",
            "Finish",
            "Finish",
            "Join"
        ]
    );
}
