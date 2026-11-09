//! Records OneForOne supervisor lifecycle traces from real actors for TLA+
//! trace validation (verification/tla/README.md). Each scenario writes
//! `<name>.json` to `$LIFECYCLE_TRACE_DIR` (default `target/lifecycle-traces`);
//! `verification/tla/trace_check.py` checks them against Supervision.tla.
#![cfg(feature = "lifecycle-trace")]

use spawned_concurrency::{
    lifecycle_trace,
    message::Message,
    supervision::{Backoff, ChildSpec, ChildState, RestartMode, SupervisedChild, SupervisorConfig},
    system::ActorSystem,
    tasks::{Actor, Context, Handler},
};
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

const LIMIT: Duration = Duration::from_secs(4);

fn config(max_restarts: usize) -> SupervisorConfig {
    SupervisorConfig {
        max_restarts,
        backoff: Backoff {
            initial: Duration::from_millis(1),
            maximum: Duration::from_millis(8),
            jitter: false,
            seed: Some(7),
            ..Backoff::default()
        },
        ..SupervisorConfig::default()
    }
}

fn mode_name(mode: RestartMode) -> &'static str {
    match mode {
        RestartMode::Permanent => "Permanent",
        RestartMode::Transient => "Transient",
        RestartMode::Temporary => "Temporary",
    }
}

fn trace_dir() -> PathBuf {
    std::env::var_os("LIFECYCLE_TRACE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/lifecycle-traces")
        })
}

/// Take the finished supervisor's trace and write it for trace_check.py.
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
        "{{\"name\":\"{name}\",\"n\":{},\"modes\":[{}],\"max_restarts\":{max_restarts},\"events\":[\n{}\n]}}\n",
        modes.len(),
        modes.join(","),
        events.join(",\n")
    );
    let dir = trace_dir();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("{name}.json")), json).unwrap();
}

struct Crash;
impl Message for Crash {
    type Result = ();
}
struct Quit;
impl Message for Quit {
    type Result = ();
}
struct ExitWith(spawned_concurrency::ExitReason);
impl Message for ExitWith {
    type Result = ();
}
struct Worker;
impl Actor for Worker {}
struct HoldForDrain(
    tokio::sync::oneshot::Sender<()>,
    tokio::sync::oneshot::Receiver<()>,
);
impl Message for HoldForDrain {
    type Result = ();
}
impl Handler<HoldForDrain> for Worker {
    async fn handle(&mut self, msg: HoldForDrain, _: &Context<Self>) {
        msg.0.send(()).unwrap();
        let _ = msg.1.await;
    }
}
struct DropBomb;
impl Message for DropBomb {
    type Result = ();
}
impl Drop for DropBomb {
    fn drop(&mut self) {
        panic!("injected queued destructor panic");
    }
}
impl Handler<DropBomb> for Worker {
    async fn handle(&mut self, _: DropBomb, _: &Context<Self>) {
        panic!("discarded message must not be handled");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trace_queued_destructor_panic_restarts_transient_child() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("trace-drain-panic", 1).unwrap();
    let modes = [RestartMode::Transient];
    let mut tree = system.supervisor(config(5)).unwrap();
    let child = tree.add(worker("drain", &group).restart(modes[0])).unwrap();
    let supervisor = tree.start().await.unwrap();
    let actor = child.current().unwrap();
    let (entered, arrival) = tokio::sync::oneshot::channel();
    let (release, gate) = tokio::sync::oneshot::channel();
    actor.send(HoldForDrain(entered, gate)).unwrap();
    tokio::time::timeout(LIMIT, arrival).await.unwrap().unwrap();
    actor.send(DropBomb).unwrap();
    actor.context().stop();
    release.send(()).unwrap();
    let replacement = tokio::time::timeout(LIMIT, child.wait_for_generation(1))
        .await
        .unwrap()
        .unwrap();
    assert_ne!(replacement.id(), actor.id());
    assert_eq!(
        child.snapshot().last_exit,
        Some(spawned_concurrency::ExitReason::Panic(
            "unexpected framework panic".into()
        ))
    );
    assert!(supervisor.shutdown(LIMIT).await.complete);
    write_trace("queued_drop_panic", supervisor.trace_id(), &modes, 5);
    assert!(system.shutdown(LIMIT).await.complete);
}
impl Handler<Crash> for Worker {
    async fn handle(&mut self, _: Crash, _: &Context<Self>) {
        panic!("injected child failure");
    }
}
impl Handler<Quit> for Worker {
    async fn handle(&mut self, _: Quit, ctx: &Context<Self>) {
        ctx.stop();
    }
}
impl Handler<ExitWith> for Worker {
    async fn handle(&mut self, msg: ExitWith, ctx: &Context<Self>) {
        ctx.stop_with_reason(msg.0);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trace_context_shutdown_and_abnormal_exit() {
    use spawned_concurrency::ExitReason;
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("trace-context-reasons", 1).unwrap();
    let modes = [RestartMode::Transient, RestartMode::Transient];
    let mut tree = system.supervisor(config(5)).unwrap();
    let clean = tree.add(worker("clean", &group).restart(modes[0])).unwrap();
    let failed = tree
        .add(worker("failed", &group).restart(modes[1]))
        .unwrap();
    let supervisor = tree.start().await.unwrap();
    let actor = clean.current().unwrap();
    actor.request(ExitWith(ExitReason::Shutdown)).await.unwrap();
    wait_state(&clean, ChildState::Stopped).await;
    assert_eq!(
        actor.child_handle().exit_reason(),
        Some(ExitReason::Shutdown)
    );
    let actor = failed.current().unwrap();
    actor.request(ExitWith(ExitReason::Kill)).await.unwrap();
    let replacement = tokio::time::timeout(LIMIT, failed.wait_for_generation(1))
        .await
        .unwrap()
        .unwrap();
    assert_ne!(replacement.id(), actor.id());
    assert_eq!(actor.child_handle().exit_reason(), Some(ExitReason::Kill));
    assert!(supervisor.shutdown(LIMIT).await.complete);
    write_trace("context_exit_reasons", supervisor.trace_id(), &modes, 5);
    assert!(system.shutdown(LIMIT).await.complete);
}

fn worker(name: &str, group: &spawned_concurrency::system::ExecutionGroup) -> ChildSpec<Worker> {
    ChildSpec::new(name, group, |_| async { Ok::<_, String>(Worker) })
}

async fn crash(child: &SupervisedChild<Worker>) {
    let actor = child.current().unwrap();
    assert!(actor.request(Crash).await.is_err());
}

async fn wait_state(child: &SupervisedChild<Worker>, state: ChildState) {
    tokio::time::timeout(LIMIT, async {
        while child.snapshot().state != state {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trace_normal_start_and_stop() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("trace-normal", 2).unwrap();
    let modes = [
        RestartMode::Permanent,
        RestartMode::Transient,
        RestartMode::Temporary,
    ];
    let mut tree = system.supervisor(config(5)).unwrap();
    for (k, mode) in modes.iter().enumerate() {
        tree.add(worker(&format!("c{k}"), &group).restart(*mode))
            .unwrap();
    }
    let supervisor = tree.start().await.unwrap();
    assert!(supervisor.shutdown(LIMIT).await.complete);
    write_trace("normal_start_stop", supervisor.trace_id(), &modes, 5);
    assert!(system.shutdown(LIMIT).await.complete);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trace_permanent_panic_restarts() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("trace-panic", 1).unwrap();
    let modes = [RestartMode::Permanent, RestartMode::Transient];
    let mut tree = system.supervisor(config(5)).unwrap();
    let a = tree.add(worker("a", &group).restart(modes[0])).unwrap();
    let b = tree.add(worker("b", &group).restart(modes[1])).unwrap();
    let supervisor = tree.start().await.unwrap();
    crash(&a).await;
    tokio::time::timeout(LIMIT, a.wait_for_generation(1))
        .await
        .unwrap()
        .unwrap();
    crash(&b).await;
    tokio::time::timeout(LIMIT, b.wait_for_generation(1))
        .await
        .unwrap()
        .unwrap();
    assert!(supervisor.shutdown(LIMIT).await.complete);
    write_trace("permanent_panic_restart", supervisor.trace_id(), &modes, 5);
    assert!(system.shutdown(LIMIT).await.complete);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trace_transient_normal_exit_is_not_restarted() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("trace-transient", 1).unwrap();
    let modes = [RestartMode::Permanent, RestartMode::Transient];
    let mut tree = system.supervisor(config(5)).unwrap();
    tree.add(worker("a", &group).restart(modes[0])).unwrap();
    let b = tree.add(worker("b", &group).restart(modes[1])).unwrap();
    let supervisor = tree.start().await.unwrap();
    b.current().unwrap().send(Quit).unwrap();
    wait_state(&b, ChildState::Stopped).await;
    assert_eq!(b.snapshot().generation, 1);
    assert!(supervisor.shutdown(LIMIT).await.complete);
    write_trace("transient_normal_exit", supervisor.trace_id(), &modes, 5);
    assert!(system.shutdown(LIMIT).await.complete);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trace_temporary_exit_is_not_restarted() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("trace-temporary", 1).unwrap();
    let modes = [RestartMode::Temporary, RestartMode::Permanent];
    let mut tree = system.supervisor(config(5)).unwrap();
    let a = tree.add(worker("a", &group).restart(modes[0])).unwrap();
    tree.add(worker("b", &group).restart(modes[1])).unwrap();
    let supervisor = tree.start().await.unwrap();
    crash(&a).await;
    wait_state(&a, ChildState::Stopped).await;
    assert!(supervisor.shutdown(LIMIT).await.complete);
    write_trace("temporary_exit", supervisor.trace_id(), &modes, 5);
    assert!(system.shutdown(LIMIT).await.complete);
}

/// ChildHandle::shutdown on a running Permanent child: an exit with reason
/// Shutdown while the supervisor is open, which the design restarts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trace_external_shutdown_of_permanent_child_restarts() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("trace-external", 1).unwrap();
    let modes = [RestartMode::Permanent, RestartMode::Transient];
    let mut tree = system.supervisor(config(5)).unwrap();
    let a = tree.add(worker("a", &group).restart(modes[0])).unwrap();
    let b = tree.add(worker("b", &group).restart(modes[1])).unwrap();
    let supervisor = tree.start().await.unwrap();
    a.current().unwrap().child_handle().shutdown();
    tokio::time::timeout(LIMIT, a.wait_for_generation(1))
        .await
        .unwrap()
        .unwrap();
    b.current().unwrap().child_handle().shutdown();
    wait_state(&b, ChildState::Stopped).await;
    assert!(supervisor.shutdown(LIMIT).await.complete);
    write_trace("external_shutdown", supervisor.trace_id(), &modes, 5);
    assert!(system.shutdown(LIMIT).await.complete);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trace_factory_error_then_success() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("trace-factory", 1).unwrap();
    let modes = [RestartMode::Permanent];
    let calls = Arc::new(AtomicUsize::new(0));
    let mut tree = system.supervisor(config(5)).unwrap();
    let count = calls.clone();
    tree.add(ChildSpec::new("flaky", &group, move |_| {
        let attempt = count.fetch_add(1, Ordering::SeqCst);
        async move {
            if attempt == 0 {
                Err("first construction fails".to_string())
            } else {
                Ok(Worker)
            }
        }
    }))
    .unwrap();
    let supervisor = tree.start().await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(supervisor.shutdown(LIMIT).await.complete);
    write_trace(
        "factory_error_then_success",
        supervisor.trace_id(),
        &modes,
        5,
    );
    assert!(system.shutdown(LIMIT).await.complete);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trace_non_restartable_startup_failure_fails_start() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("trace-startfail", 1).unwrap();
    let modes = [RestartMode::Permanent, RestartMode::Temporary];
    let mut tree = system.supervisor(config(5)).unwrap();
    tree.add(worker("a", &group).restart(modes[0])).unwrap();
    tree.add(
        ChildSpec::new("b", &group, |_| async {
            Err::<Worker, _>("cannot construct")
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
    write_trace("non_restartable_startup_failure", trace_id, &modes, 5);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trace_restart_budget_exhaustion() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("trace-budget", 2).unwrap();
    let modes = [RestartMode::Permanent, RestartMode::Permanent];
    let mut tree = system.supervisor(config(2)).unwrap();
    let a = tree.add(worker("a", &group)).unwrap();
    let b = tree.add(worker("b", &group)).unwrap();
    let supervisor = tree.start().await.unwrap();
    crash(&a).await;
    tokio::time::timeout(LIMIT, a.wait_for_generation(1))
        .await
        .unwrap()
        .unwrap();
    crash(&b).await;
    tokio::time::timeout(LIMIT, b.wait_for_generation(1))
        .await
        .unwrap()
        .unwrap();
    crash(&a).await;
    tokio::time::timeout(LIMIT, supervisor.join())
        .await
        .unwrap();
    assert!(supervisor.failure().unwrap().contains("budget exhausted"));
    write_trace(
        "restart_budget_exhaustion",
        supervisor.trace_id(),
        &modes,
        2,
    );
    assert!(system.shutdown(LIMIT).await.complete);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trace_disabled_retries() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("trace-noretry", 1).unwrap();
    let modes = [RestartMode::Permanent];
    let mut tree = system.supervisor(config(0)).unwrap();
    let a = tree.add(worker("a", &group)).unwrap();
    let supervisor = tree.start().await.unwrap();
    crash(&a).await;
    tokio::time::timeout(LIMIT, supervisor.join())
        .await
        .unwrap();
    assert!(supervisor.failure().is_some());
    write_trace("disabled_retries", supervisor.trace_id(), &modes, 0);
    assert!(system.shutdown(LIMIT).await.complete);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trace_shutdown_during_backoff() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("trace-backoff", 1).unwrap();
    let modes = [RestartMode::Permanent];
    let mut policy = config(5);
    policy.backoff.initial = Duration::from_secs(60);
    policy.backoff.maximum = Duration::from_secs(60);
    let mut tree = system.supervisor(policy).unwrap();
    let a = tree.add(worker("a", &group)).unwrap();
    let supervisor = tree.start().await.unwrap();
    crash(&a).await;
    wait_state(&a, ChildState::Backoff).await;
    assert!(supervisor.shutdown(LIMIT).await.complete);
    assert_eq!(a.snapshot().generation, 1);
    write_trace("shutdown_during_backoff", supervisor.trace_id(), &modes, 5);
    assert!(system.shutdown(LIMIT).await.complete);
}

struct SlowInit {
    first: bool,
}
impl Actor for SlowInit {
    async fn started(&mut self, _: &Context<Self>) {
        if self.first {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trace_startup_timeout_retries() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("trace-timeout", 1).unwrap();
    let modes = [RestartMode::Transient];
    let mut tree = system.supervisor(config(3)).unwrap();
    tree.add(
        ChildSpec::new("slow-init", &group, |ctx| async move {
            Ok::<_, String>(SlowInit {
                first: ctx.generation() == 1,
            })
        })
        .restart(modes[0])
        .startup_timeout(Duration::from_millis(20)),
    )
    .unwrap();
    let supervisor = tokio::time::timeout(LIMIT, tree.start())
        .await
        .unwrap()
        .unwrap();
    assert!(supervisor.shutdown(LIMIT).await.complete);
    write_trace("startup_timeout", supervisor.trace_id(), &modes, 3);
    assert!(system.shutdown(LIMIT).await.complete);
}

struct SlowStop {
    release: Option<tokio::sync::oneshot::Receiver<()>>,
}
impl Actor for SlowStop {
    async fn stopped(&mut self, _: &Context<Self>) {
        if let Some(release) = self.release.take() {
            let _ = release.await;
        }
    }
}
/// Supervisor::shutdown with a short deadline while the last child's
/// stopped() hangs past its cleanup deadline: ShutdownDeadline,
/// SupChildTimeout and CleanupStuck.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trace_shutdown_deadline_with_slow_stop() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("trace-deadline", 2).unwrap();
    let modes = [RestartMode::Permanent, RestartMode::Permanent];
    let (release, release_rx) = tokio::sync::oneshot::channel();
    let slot = Arc::new(Mutex::new(Some(release_rx)));
    let mut tree = system.supervisor(config(5)).unwrap();
    tree.add(worker("first", &group)).unwrap();
    tree.add(
        ChildSpec::new("slow-stop", &group, move |_| {
            let release = slot.lock().unwrap_or_else(|p| p.into_inner()).take();
            async move { Ok::<_, String>(SlowStop { release }) }
        })
        .shutdown_timeout(Duration::from_millis(100)),
    )
    .unwrap();
    let supervisor = tree.start().await.unwrap();
    let report = supervisor.shutdown(Duration::from_millis(20)).await;
    assert!(!report.complete);
    tokio::time::sleep(Duration::from_millis(300)).await;
    release.send(()).unwrap();
    tokio::time::timeout(LIMIT, supervisor.join())
        .await
        .unwrap();
    assert!(supervisor.failure().is_some());
    write_trace(
        "shutdown_deadline_slow_stop",
        supervisor.trace_id(),
        &modes,
        5,
    );
    assert!(system.shutdown(LIMIT).await.complete);
}

/// Small deterministic generator so a failing seed can be replayed.
struct Lcg(u64);
impl Lcg {
    fn next(&mut self, bound: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 33) % bound
    }
}

/// Random crashes, normal exits and external shutdowns on three children
/// with random modes and budgets, racing a final stop or shutdown.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn trace_randomized_lifecycles() {
    let all = [
        RestartMode::Permanent,
        RestartMode::Transient,
        RestartMode::Temporary,
    ];
    for seed in 1..=8u64 {
        let mut rng = Lcg(seed);
        let system = ActorSystem::new().unwrap();
        let group = system.async_group("trace-random", 2).unwrap();
        let max_restarts = 1 + rng.next(4) as usize;
        let modes: Vec<RestartMode> = (0..3).map(|_| all[rng.next(3) as usize]).collect();
        let mut tree = system.supervisor(config(max_restarts)).unwrap();
        let children: Vec<_> = modes
            .iter()
            .enumerate()
            .map(|(k, mode)| {
                tree.add(worker(&format!("c{k}"), &group).restart(*mode))
                    .unwrap()
            })
            .collect();
        let supervisor = tree.start().await.unwrap();
        for _ in 0..rng.next(6) {
            let child = &children[rng.next(3) as usize];
            let op = rng.next(3);
            if let Ok(actor) = child.current() {
                match op {
                    0 => {
                        let _ = tokio::time::timeout(LIMIT, actor.request(Crash)).await;
                    }
                    1 => {
                        let _ = actor.send(Quit);
                    }
                    _ => actor.child_handle().shutdown(),
                }
            }
            tokio::time::sleep(Duration::from_millis(rng.next(4))).await;
        }
        if rng.next(2) == 0 {
            supervisor.stop();
        } else {
            let _ = supervisor.shutdown(LIMIT).await;
        }
        tokio::time::timeout(LIMIT, supervisor.join())
            .await
            .unwrap();
        write_trace(
            &format!("random_{seed}"),
            supervisor.trace_id(),
            &modes,
            max_restarts,
        );
        assert!(system.shutdown(LIMIT).await.complete);
    }
}
