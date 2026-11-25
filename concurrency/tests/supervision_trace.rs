//! Records OneForOne supervisor lifecycle traces from real actors for TLA+
//! trace validation (verification/tla/README.md). Each scenario writes
//! `<name>.json` to `$LIFECYCLE_TRACE_DIR` (default `target/lifecycle-traces`);
//! `verification/tla/trace_check.py` checks them against Supervision.tla.
#![cfg(feature = "lifecycle-trace")]

#[macro_use]
#[path = "support/deadline.rs"]
mod deadline;
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
    SupervisorConfig::default()
        .max_restarts(max_restarts)
        .backoff(
            Backoff::default()
                .initial(Duration::from_millis(1))
                .maximum(Duration::from_millis(8))
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
    async fn handle(
        &mut self,
        msg: HoldForDrain,
        reply: spawned_concurrency::PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
        msg.0.send(()).unwrap();
        let _ = msg.1.await;
        reply.reply(());
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
    async fn handle(
        &mut self,
        _: DropBomb,
        _reply: spawned_concurrency::PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
        panic!("discarded message must not be handled");
    }
}

#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn trace_queued_destructor_panic_restarts_transient_child() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("trace-drain-panic", 1).unwrap();
    let mut tree = system.supervisor(config(5)).unwrap();
    let child = tree
        .add(worker("drain", &group).restart(RestartMode::Transient))
        .unwrap();
    let supervisor = within!(tree.start()).await.unwrap();
    let actor = child.current().unwrap();
    let (entered, arrival) = tokio::sync::oneshot::channel();
    let (release, gate) = tokio::sync::oneshot::channel();
    actor.send(HoldForDrain(entered, gate)).unwrap();
    spawned_rt::tasks::timeout(LIMIT, arrival)
        .await
        .unwrap()
        .unwrap();
    actor.send(DropBomb).unwrap();
    actor.context().stop();
    release.send(()).unwrap();
    let replacement = spawned_rt::tasks::timeout(LIMIT, child.wait_for_generation(1))
        .await
        .unwrap()
        .unwrap();
    assert_ne!(replacement.id(), actor.id());
    // The final reason stays Normal; the destructor panic after the death
    // point is a cleanup failure, which restarts a Transient child.
    assert_eq!(
        child.snapshot().last_exit,
        Some(spawned_concurrency::ExitReason::Normal)
    );
    assert_eq!(
        child.snapshot().last_error.as_deref(),
        Some("injected queued destructor panic")
    );
    assert!(supervisor.shutdown(LIMIT).await.complete);
    write_trace(
        "queued_drop_panic",
        supervisor.trace_id(),
        &[RestartMode::Transient],
        5,
    );
    assert!(system.shutdown(LIMIT).await.complete);
}

#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn trace_queued_destructor_failure_after_shutdown_restarts_transient_child() {
    use spawned_concurrency::ExitReason;
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("trace-drain-shutdown", 1).unwrap();
    let mut tree = system.supervisor(config(5)).unwrap();
    let child = tree
        .add(worker("drain", &group).restart(RestartMode::Transient))
        .unwrap();
    let supervisor = within!(tree.start()).await.unwrap();
    let actor = child.current().unwrap();
    let (entered, arrival) = tokio::sync::oneshot::channel();
    let (release, gate) = tokio::sync::oneshot::channel();
    actor.send(HoldForDrain(entered, gate)).unwrap();
    spawned_rt::tasks::timeout(LIMIT, arrival)
        .await
        .unwrap()
        .unwrap();
    actor.send(DropBomb).unwrap();
    actor
        .child_handle()
        .stop_with_reason(spawned_concurrency::ExitReason::Shutdown(None));
    release.send(()).unwrap();
    // v0.8: the reason stays Shutdown, but the cleanup failure restarts a
    // Transient child (the supervisor is not closing).
    let replacement = spawned_rt::tasks::timeout(LIMIT, child.wait_for_generation(1))
        .await
        .unwrap()
        .unwrap();
    assert_ne!(replacement.id(), actor.id());
    assert_eq!(child.snapshot().last_exit, Some(ExitReason::Shutdown(None)));
    assert_eq!(
        actor.child_handle().cleanup_failure().as_deref(),
        Some("injected queued destructor panic")
    );
    assert!(supervisor.shutdown(LIMIT).await.complete);
    write_trace(
        "queued_drop_shutdown_restarts",
        supervisor.trace_id(),
        &[RestartMode::Transient],
        5,
    );
    assert!(system.shutdown(LIMIT).await.complete);
}
impl Handler<Crash> for Worker {
    async fn handle(
        &mut self,
        _: Crash,
        _reply: spawned_concurrency::PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
        panic!("injected child failure");
    }
}
impl Handler<Quit> for Worker {
    async fn handle(
        &mut self,
        _: Quit,
        reply: spawned_concurrency::PendingReply<'_, ()>,
        ctx: &Context<Self>,
    ) {
        ctx.stop();
        reply.reply(());
    }
}
impl Handler<ExitWith> for Worker {
    async fn handle(
        &mut self,
        msg: ExitWith,
        reply: spawned_concurrency::PendingReply<'_, ()>,
        ctx: &Context<Self>,
    ) {
        ctx.stop_with_reason(msg.0);
        reply.reply(());
    }
}

#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
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
    let supervisor = within!(tree.start()).await.unwrap();
    let actor = clean.current().unwrap();
    actor
        .request(ExitWith(ExitReason::Shutdown(None)))
        .await
        .unwrap();
    wait_state(&clean, ChildState::Stopped).await;
    assert_eq!(
        actor.child_handle().exit_reason(),
        Some(ExitReason::Shutdown(None))
    );
    let actor = failed.current().unwrap();
    actor.request(ExitWith(ExitReason::Kill)).await.unwrap();
    let replacement = spawned_rt::tasks::timeout(LIMIT, failed.wait_for_generation(1))
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

async fn wait_state<A: Actor>(child: &SupervisedChild<A>, state: ChildState) {
    spawned_rt::tasks::timeout(LIMIT, async {
        while child.snapshot().state != state {
            spawned_rt::tasks::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
}

#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
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
    let supervisor = within!(tree.start()).await.unwrap();
    assert!(supervisor.shutdown(LIMIT).await.complete);
    write_trace("normal_start_stop", supervisor.trace_id(), &modes, 5);
    assert!(system.shutdown(LIMIT).await.complete);
}

#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn trace_permanent_panic_restarts() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("trace-panic", 1).unwrap();
    let modes = [RestartMode::Permanent, RestartMode::Transient];
    let mut tree = system.supervisor(config(5)).unwrap();
    let a = tree.add(worker("a", &group).restart(modes[0])).unwrap();
    let b = tree.add(worker("b", &group).restart(modes[1])).unwrap();
    let supervisor = within!(tree.start()).await.unwrap();
    crash(&a).await;
    spawned_rt::tasks::timeout(LIMIT, a.wait_for_generation(1))
        .await
        .unwrap()
        .unwrap();
    crash(&b).await;
    spawned_rt::tasks::timeout(LIMIT, b.wait_for_generation(1))
        .await
        .unwrap()
        .unwrap();
    assert!(supervisor.shutdown(LIMIT).await.complete);
    write_trace("permanent_panic_restart", supervisor.trace_id(), &modes, 5);
    assert!(system.shutdown(LIMIT).await.complete);
}

#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn trace_transient_normal_exit_is_not_restarted() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("trace-transient", 1).unwrap();
    let modes = [RestartMode::Permanent, RestartMode::Transient];
    let mut tree = system.supervisor(config(5)).unwrap();
    tree.add(worker("a", &group).restart(modes[0])).unwrap();
    let b = tree.add(worker("b", &group).restart(modes[1])).unwrap();
    let supervisor = within!(tree.start()).await.unwrap();
    b.current().unwrap().send(Quit).unwrap();
    wait_state(&b, ChildState::Stopped).await;
    assert_eq!(b.snapshot().generation, 1);
    assert!(supervisor.shutdown(LIMIT).await.complete);
    write_trace("transient_normal_exit", supervisor.trace_id(), &modes, 5);
    assert!(system.shutdown(LIMIT).await.complete);
}

#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn trace_temporary_exit_is_not_restarted() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("trace-temporary", 1).unwrap();
    let modes = [RestartMode::Temporary, RestartMode::Permanent];
    let mut tree = system.supervisor(config(5)).unwrap();
    let a = tree.add(worker("a", &group).restart(modes[0])).unwrap();
    tree.add(worker("b", &group).restart(modes[1])).unwrap();
    let supervisor = within!(tree.start()).await.unwrap();
    crash(&a).await;
    wait_state(&a, ChildState::Stopped).await;
    assert!(supervisor.shutdown(LIMIT).await.complete);
    write_trace("temporary_exit", supervisor.trace_id(), &modes, 5);
    assert!(system.shutdown(LIMIT).await.complete);
}

/// ChildHandle::shutdown on a running Permanent child: an exit with reason
/// Shutdown while the supervisor is open, which the design restarts.
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn trace_external_shutdown_of_permanent_child_restarts() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("trace-external", 1).unwrap();
    let modes = [RestartMode::Permanent, RestartMode::Transient];
    let mut tree = system.supervisor(config(5)).unwrap();
    let a = tree.add(worker("a", &group).restart(modes[0])).unwrap();
    let b = tree.add(worker("b", &group).restart(modes[1])).unwrap();
    let supervisor = within!(tree.start()).await.unwrap();
    a.current()
        .unwrap()
        .child_handle()
        .stop_with_reason(spawned_concurrency::ExitReason::Shutdown(None));
    spawned_rt::tasks::timeout(LIMIT, a.wait_for_generation(1))
        .await
        .unwrap()
        .unwrap();
    b.current()
        .unwrap()
        .child_handle()
        .stop_with_reason(spawned_concurrency::ExitReason::Shutdown(None));
    wait_state(&b, ChildState::Stopped).await;
    assert!(supervisor.shutdown(LIMIT).await.complete);
    write_trace("external_shutdown", supervisor.trace_id(), &modes, 5);
    assert!(system.shutdown(LIMIT).await.complete);
}

/// S1: a Permanent child whose first construction fails aborts the start (v0.7
/// retried it); the model's Decide fails because the child never completed
/// its first start.
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn trace_first_start_factory_error_fails_start() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("trace-factory-first", 1).unwrap();
    let modes = [RestartMode::Permanent, RestartMode::Permanent];
    let mut tree = system.supervisor(config(5)).unwrap();
    tree.add(worker("a", &group).restart(modes[0])).unwrap();
    tree.add(
        ChildSpec::new("flaky", &group, |_| async {
            Err::<Worker, _>("first construction fails")
        })
        .restart(modes[1]),
    )
    .unwrap();
    let trace_id = tree.trace_id();
    assert!(spawned_rt::tasks::timeout(LIMIT, tree.start())
        .await
        .unwrap()
        .is_err());
    assert!(system.shutdown(LIMIT).await.complete);
    write_trace("first_start_factory_error", trace_id, &modes, 5);
}

/// S1: a ready child's exit during the initial start is deferred (DeferExit)
/// and processed once the tree commits ready (Undefer, then the restart).
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn trace_ready_exit_deferred_during_startup() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("trace-deferred", 2).unwrap();
    let modes = [RestartMode::Permanent, RestartMode::Permanent];
    let mut tree = system.supervisor(config(5)).unwrap();
    let a = tree.add(worker("a", &group).restart(modes[0])).unwrap();
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let (e, r) = (entered.clone(), release.clone());
    tree.add(
        ChildSpec::new("b", &group, move |ctx| {
            let (e, r) = (e.clone(), r.clone());
            async move {
                if ctx.generation() == 1 {
                    e.notify_one();
                    r.notified().await;
                }
                Ok::<_, String>(Worker)
            }
        })
        .restart(modes[1]),
    )
    .unwrap();
    let trace_id = tree.trace_id();
    let start = spawned_rt::tasks::spawn(tree.start());
    spawned_rt::tasks::timeout(LIMIT, entered.notified())
        .await
        .unwrap();
    crash(&a).await;
    // The exit is recorded before the start completes.
    spawned_rt::tasks::timeout(LIMIT, async {
        while !lifecycle_trace::recorded(trace_id, "DeferExit") {
            spawned_rt::tasks::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    release.notify_one();
    let supervisor = spawned_rt::tasks::timeout(LIMIT, start)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    spawned_rt::tasks::timeout(LIMIT, a.wait_for_generation(1))
        .await
        .unwrap()
        .unwrap();
    assert!(supervisor.shutdown(LIMIT).await.complete);
    write_trace("ready_exit_deferred", trace_id, &modes, 5);
    assert!(system.shutdown(LIMIT).await.complete);
}

/// A replacement whose construction fails is retried within the budget.
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn trace_factory_error_then_success() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("trace-factory", 1).unwrap();
    let modes = [RestartMode::Permanent];
    let calls = Arc::new(AtomicUsize::new(0));
    let mut tree = system.supervisor(config(5)).unwrap();
    let count = calls.clone();
    let flaky = tree
        .add(ChildSpec::new("flaky", &group, move |_| {
            let attempt = count.fetch_add(1, Ordering::SeqCst);
            async move {
                if attempt == 1 {
                    Err("replacement construction fails".to_string())
                } else {
                    Ok(Worker)
                }
            }
        }))
        .unwrap();
    let supervisor = within!(tree.start()).await.unwrap();
    crash(&flaky).await;
    spawned_rt::tasks::timeout(LIMIT, flaky.wait_for_generation(2))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    assert!(supervisor.shutdown(LIMIT).await.complete);
    write_trace(
        "factory_error_then_success",
        supervisor.trace_id(),
        &modes,
        5,
    );
    assert!(system.shutdown(LIMIT).await.complete);
}

#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
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
    assert!(spawned_rt::tasks::timeout(LIMIT, tree.start())
        .await
        .unwrap()
        .is_err());
    assert!(system.shutdown(LIMIT).await.complete);
    write_trace("non_restartable_startup_failure", trace_id, &modes, 5);
}

#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn trace_restart_budget_exhaustion() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("trace-budget", 2).unwrap();
    let modes = [RestartMode::Permanent, RestartMode::Permanent];
    let mut tree = system.supervisor(config(2)).unwrap();
    let a = tree.add(worker("a", &group)).unwrap();
    let b = tree.add(worker("b", &group)).unwrap();
    let supervisor = within!(tree.start()).await.unwrap();
    crash(&a).await;
    spawned_rt::tasks::timeout(LIMIT, a.wait_for_generation(1))
        .await
        .unwrap()
        .unwrap();
    crash(&b).await;
    spawned_rt::tasks::timeout(LIMIT, b.wait_for_generation(1))
        .await
        .unwrap()
        .unwrap();
    crash(&a).await;
    spawned_rt::tasks::timeout(LIMIT, supervisor.join())
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

#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn trace_disabled_retries() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("trace-noretry", 1).unwrap();
    let modes = [RestartMode::Permanent];
    let mut tree = system.supervisor(config(0)).unwrap();
    let a = tree.add(worker("a", &group)).unwrap();
    let supervisor = within!(tree.start()).await.unwrap();
    crash(&a).await;
    spawned_rt::tasks::timeout(LIMIT, supervisor.join())
        .await
        .unwrap();
    assert!(supervisor.failure().is_some());
    write_trace("disabled_retries", supervisor.trace_id(), &modes, 0);
    assert!(system.shutdown(LIMIT).await.complete);
}

#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn trace_shutdown_during_backoff() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("trace-backoff", 1).unwrap();
    let modes = [RestartMode::Permanent];
    let mut policy = config(5);
    policy.backoff.initial = Duration::from_secs(60);
    policy.backoff.maximum = Duration::from_secs(60);
    let mut tree = system.supervisor(policy).unwrap();
    let a = tree.add(worker("a", &group)).unwrap();
    let supervisor = within!(tree.start()).await.unwrap();
    crash(&a).await;
    wait_state(&a, ChildState::Backoff).await;
    assert!(supervisor.shutdown(LIMIT).await.complete);
    assert_eq!(a.snapshot().generation, 1);
    write_trace("shutdown_during_backoff", supervisor.trace_id(), &modes, 5);
    assert!(system.shutdown(LIMIT).await.complete);
}

/// Stops itself with `exit` before readiness on its second generation.
struct ExitOnceDuringRestart {
    exit: Option<spawned_concurrency::ExitReason>,
}
impl Actor for ExitOnceDuringRestart {
    async fn started(&mut self, ctx: &Context<Self>) {
        if let Some(reason) = self.exit.take() {
            ctx.stop_with_reason(reason);
        }
    }
}
impl Handler<Crash> for ExitOnceDuringRestart {
    async fn handle(
        &mut self,
        _: Crash,
        _reply: spawned_concurrency::PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
        panic!("injected child failure");
    }
}

/// A replacement that stops before readiness while the supervisor is open:
/// `StartupFailed` with `Shutdown` (Permanent) or `Normal` (Transient), then a
/// retry. Trace validation first rejected the `Shutdown` case (the model
/// allowed a `Shutdown` startup failure only after closure) and the code
/// failed the supervisor on the Transient one.
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn trace_replacement_exiting_before_readiness_is_retried() {
    use spawned_concurrency::ExitReason;
    for (name, mode, reason) in [
        (
            "replacement_shutdown_before_ready",
            RestartMode::Permanent,
            ExitReason::Shutdown(None),
        ),
        (
            "transient_replacement_normal_before_ready",
            RestartMode::Transient,
            ExitReason::Normal,
        ),
    ] {
        let system = ActorSystem::new().unwrap();
        let group = system.async_group("trace-replacement-exit", 1).unwrap();
        let mut tree = system.supervisor(config(5)).unwrap();
        let exit = reason.clone();
        let child = tree
            .add(
                ChildSpec::new("replacement", &group, move |ctx| {
                    let exit = (ctx.generation() == 2).then(|| exit.clone());
                    async move { Ok::<_, String>(ExitOnceDuringRestart { exit }) }
                })
                .restart(mode),
            )
            .unwrap();
        let supervisor = within!(tree.start()).await.unwrap();
        assert!(child.current().unwrap().request(Crash).await.is_err());
        spawned_rt::tasks::timeout(LIMIT, child.wait_for_generation(2))
            .await
            .unwrap()
            .unwrap();
        assert!(supervisor.shutdown(LIMIT).await.complete);
        write_trace(name, supervisor.trace_id(), &[mode], 5);
        assert!(system.shutdown(LIMIT).await.complete);
    }
}

/// Startup deadline of the timeout scenarios: far above any healthy start,
/// so on a loaded machine only the held start can miss it.
const STARTUP: Duration = Duration::from_secs(2);

/// Holds `started()` until the test releases it, so the startup deadline is
/// the only way the attempt can end (no wall-clock race with a sleep).
struct HeldInit {
    release: Option<tokio::sync::oneshot::Receiver<()>>,
}
impl Actor for HeldInit {
    async fn started(&mut self, _: &Context<Self>) {
        if let Some(release) = self.release.take() {
            let _ = release.await;
        }
    }
}
type Held = Arc<Mutex<Option<tokio::sync::oneshot::Receiver<()>>>>;
fn held() -> (tokio::sync::oneshot::Sender<()>, Held) {
    let (release, held) = tokio::sync::oneshot::channel();
    (release, Arc::new(Mutex::new(Some(held))))
}
/// Wait until `child`'s held attempt has missed its startup deadline, then
/// let its `started()` return so the attempt's cleanup can finish.
async fn release_after_timeout<A: Actor>(
    child: &SupervisedChild<A>,
    release: tokio::sync::oneshot::Sender<()>,
) {
    spawned_rt::tasks::timeout(STARTUP + LIMIT, async {
        while child.snapshot().last_error.as_deref() != Some("startup deadline exceeded") {
            spawned_rt::tasks::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    release.send(()).unwrap();
}
/// S1: a first start that misses startup_timeout aborts the start, whatever
/// the restart type (v0.7 retried a Transient child).
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn trace_first_start_timeout_fails_start() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("trace-timeout-first", 1).unwrap();
    let modes = [RestartMode::Transient];
    let (release, held) = held();
    let mut tree = system.supervisor(config(3)).unwrap();
    let child = tree
        .add(
            ChildSpec::new("slow-init", &group, move |_| {
                let release = held.lock().unwrap().take();
                async move { Ok::<_, String>(HeldInit { release }) }
            })
            .restart(modes[0])
            .startup_timeout(STARTUP),
        )
        .unwrap();
    let trace_id = tree.trace_id();
    // start() returns only after the timed-out attempt's cleanup, which
    // waits for the held started(): release it from alongside.
    let (started, ()) = tokio::join!(
        spawned_rt::tasks::timeout(STARTUP + LIMIT, tree.start()),
        release_after_timeout(&child, release),
    );
    let failure = started.unwrap().err().expect("the first start must fail");
    assert!(
        format!("{failure:?}").contains("startup deadline exceeded"),
        "{failure:?}"
    );
    assert!(system.shutdown(LIMIT).await.complete);
    write_trace("first_start_timeout", trace_id, &modes, 3);
}

/// A replacement that misses startup_timeout is retried.
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn trace_startup_timeout_retries() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("trace-timeout", 1).unwrap();
    let modes = [RestartMode::Transient];
    let (release, held) = held();
    let mut tree = system.supervisor(config(3)).unwrap();
    let child = tree
        .add(
            ChildSpec::new("slow-init", &group, move |ctx| {
                // The first start is never retried (S1): hold the first
                // replacement's.
                let release = if ctx.generation() == 2 {
                    held.lock().unwrap().take()
                } else {
                    None
                };
                async move { Ok::<_, String>(HeldInit { release }) }
            })
            .restart(modes[0])
            .startup_timeout(STARTUP),
        )
        .unwrap();
    let supervisor = spawned_rt::tasks::timeout(LIMIT, tree.start())
        .await
        .unwrap()
        .unwrap();
    child.current().unwrap().child_handle().kill();
    release_after_timeout(&child, release).await;
    spawned_rt::tasks::timeout(LIMIT, child.wait_for_generation(2))
        .await
        .unwrap()
        .unwrap();
    assert!(supervisor.shutdown(LIMIT).await.complete);
    write_trace("startup_timeout", supervisor.trace_id(), &modes, 3);
    assert!(system.shutdown(LIMIT).await.complete);
}

/// Wait until `event` is in the trace.
async fn recorded(trace_id: u64, event: &str) {
    spawned_rt::tasks::timeout(STARTUP + LIMIT, async {
        while !lifecycle_trace::recorded(trace_id, event) {
            spawned_rt::tasks::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{event} was not recorded"));
}

/// V3 for a generation stopped before it was published: its cleanup is
/// given shutdown_timeout, then it is killed (CleanupKill); released after
/// the kill it ends (StartupFailed, StopExited, CleanupDone), never released
/// it is stuck only after a second interval (CleanupStuck). `stuck`: the
/// supervisor is shut down while the replacement is in started() and never
/// let go; otherwise the replacement misses startup_timeout and is let go
/// after the kill, and is retried.
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn trace_unpublished_generation_killed_or_stuck() {
    // Stuck first: with a regression that skips the kill the trace is still
    // written (CleanupStuck straight after StartupFailed, which the model
    // rejects) before the released run fails waiting for CleanupKill.
    for stuck in [true, false] {
        let system = ActorSystem::new().unwrap();
        let group = system.async_group("trace-unpublished", 1).unwrap();
        let modes = [RestartMode::Transient];
        let (release, held) = held();
        let mut tree = system.supervisor(config(3)).unwrap();
        let child = tree
            .add(
                ChildSpec::new("slow-init", &group, move |ctx| {
                    let release = if ctx.generation() == 2 {
                        held.lock().unwrap().take()
                    } else {
                        None
                    };
                    async move { Ok::<_, String>(HeldInit { release }) }
                })
                .restart(modes[0])
                .startup_timeout(STARTUP)
                .shutdown_timeout(Duration::from_millis(100)),
            )
            .unwrap();
        let supervisor = within!(tree.start()).await.unwrap();
        let trace_id = supervisor.trace_id();
        child.current().unwrap().child_handle().kill();
        let mut release = Some(release);
        if stuck {
            spawned_rt::tasks::timeout(LIMIT, async {
                while child.snapshot().generation < 2 {
                    spawned_rt::tasks::sleep(Duration::from_millis(1)).await;
                }
            })
            .await
            .unwrap();
            let (_, ()) = tokio::join!(supervisor.shutdown(LIMIT), async {
                recorded(trace_id, "CleanupStuck").await;
                let _ = release.take().unwrap().send(());
            });
            spawned_rt::tasks::timeout(LIMIT, supervisor.join())
                .await
                .unwrap();
            assert!(supervisor.failure().is_some());
        } else {
            recorded(trace_id, "CleanupKill").await;
            let _ = release.take().unwrap().send(());
            spawned_rt::tasks::timeout(LIMIT, child.wait_for_generation(2))
                .await
                .unwrap()
                .unwrap();
            assert!(supervisor.shutdown(LIMIT).await.complete);
            assert!(supervisor.failure().is_none(), "{:?}", supervisor.failure());
        }
        let killed = lifecycle_trace::recorded(trace_id, "CleanupKill");
        write_trace(
            if stuck {
                "unpublished_stuck"
            } else {
                "unpublished_kill"
            },
            trace_id,
            &modes,
            3,
        );
        assert!(killed, "the unpublished generation was not killed");
        let _ = system.shutdown(LIMIT).await;
    }
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
/// Supervisor::shutdown with a short deadline while the last child's
/// stopped() hangs past its cleanup deadline: ShutdownDeadline,
/// SupChildTimeout and CleanupStuck.
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
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
    let supervisor = within!(tree.start()).await.unwrap();
    let report = supervisor.shutdown(Duration::from_millis(20)).await;
    assert!(!report.complete);
    // Past the supervisor's backstop (3 x shutdown_timeout).
    spawned_rt::tasks::sleep(Duration::from_millis(500)).await;
    release.send(()).unwrap();
    spawned_rt::tasks::timeout(LIMIT, supervisor.join())
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

/// V3 under OneForOne: a stopped child that misses its shutdown_timeout is
/// killed (CleanupKill) and, dying within the second interval, is not stuck
/// (StopExited, CleanupDone). trace_shutdown_deadline_with_slow_stop records
/// the other side: a kill that cannot end a running stopped(), then
/// CleanupStuck after the second interval.
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn trace_shutdown_kills_child_at_shutdown_timeout() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("trace-shutdown-kill", 2).unwrap();
    let modes = [RestartMode::Permanent];
    let mut tree = system.supervisor(config(5)).unwrap();
    let child = tree
        .add(worker("held", &group).shutdown_timeout(Duration::from_millis(100)))
        .unwrap();
    let supervisor = within!(tree.start()).await.unwrap();
    let trace_id = supervisor.trace_id();
    let actor = child.current().unwrap();
    let (began, began_rx) = tokio::sync::oneshot::channel();
    let (release, release_rx) = tokio::sync::oneshot::channel();
    actor.send(HoldForDrain(began, release_rx)).unwrap();
    began_rx.await.unwrap();
    let (report, ()) = tokio::join!(supervisor.shutdown(LIMIT), async {
        // After the kill, before the second interval ends.
        spawned_rt::tasks::timeout(LIMIT, async {
            while !lifecycle_trace::recorded(trace_id, "CleanupKill") {
                spawned_rt::tasks::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the stopped child was not killed");
        release.send(()).unwrap();
    });
    assert!(report.complete);
    assert!(supervisor.failure().is_none(), "{:?}", supervisor.failure());
    assert_eq!(
        actor.wait_exit().await,
        spawned_concurrency::ExitReason::Killed
    );
    assert!(!lifecycle_trace::recorded(trace_id, "CleanupStuck"));
    write_trace("shutdown_kill", trace_id, &modes, 5);
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
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 4)]
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
        let supervisor = within!(tree.start()).await.unwrap();
        for _ in 0..rng.next(6) {
            let child = &children[rng.next(3) as usize];
            let op = rng.next(3);
            if let Ok(actor) = child.current() {
                match op {
                    0 => {
                        let _ = spawned_rt::tasks::timeout(LIMIT, actor.request(Crash)).await;
                    }
                    1 => {
                        let _ = actor.send(Quit);
                    }
                    _ => actor
                        .child_handle()
                        .stop_with_reason(spawned_concurrency::ExitReason::Shutdown(None)),
                }
            }
            spawned_rt::tasks::sleep(Duration::from_millis(rng.next(4))).await;
        }
        if rng.next(2) == 0 {
            supervisor.stop();
        } else {
            let _ = supervisor.shutdown(LIMIT).await;
        }
        spawned_rt::tasks::timeout(LIMIT, supervisor.join())
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

/// Returns `Init::ignore()` from `started()` when built with `true`.
struct Optional(bool);
impl Actor for Optional {
    async fn started(&mut self, _: &Context<Self>) -> spawned_concurrency::tasks::Init<Self> {
        if self.0 {
            spawned_concurrency::tasks::Init::ignore()
        } else {
            spawned_concurrency::tasks::Init::ready()
        }
    }
}
impl Handler<Crash> for Optional {
    async fn handle(
        &mut self,
        _: Crash,
        _reply: spawned_concurrency::PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
        panic!("injected child failure");
    }
}

/// Unified start section 9 (OTP `ignore`): `Init::ignore()` and a factory's
/// `ChildStart::Ignore` complete the first start (`Ignore`), the spec stays
/// with no generation, OneForOne never restarts it (a sibling's restart
/// leaves it alone), a Temporary one is removed, and shutdown finishes the
/// parked controllers at once.
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn trace_ignored_children_are_kept_and_never_restarted() {
    use spawned_concurrency::supervision::ChildStart;
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("trace-ignore", 2).unwrap();
    let modes = [
        RestartMode::Permanent,
        RestartMode::Transient,
        RestartMode::Temporary,
        RestartMode::Permanent,
    ];
    let mut tree = system.supervisor(config(5)).unwrap();
    let a = tree
        .add(
            ChildSpec::new("a", &group, |_| async { Ok::<_, String>(Optional(true)) })
                .restart(modes[0]),
        )
        .unwrap();
    let b = tree
        .add(
            ChildSpec::new("b", &group, |_| async {
                Ok::<_, String>(ChildStart::<Worker>::Ignore)
            })
            .restart(modes[1]),
        )
        .unwrap();
    let c = tree
        .add(
            ChildSpec::new("c", &group, |_| async { Ok::<_, String>(Optional(true)) })
                .restart(modes[2]),
        )
        .unwrap();
    let d = tree.add(worker("d", &group).restart(modes[3])).unwrap();
    let supervisor = within!(tree.start()).await.unwrap();
    assert_eq!(a.snapshot().state, ChildState::Ignored);
    assert_eq!(b.snapshot().state, ChildState::Ignored);
    wait_state(&c, ChildState::Stopped).await;
    crash(&d).await;
    spawned_rt::tasks::timeout(LIMIT, d.wait_for_generation(1))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(a.snapshot().generation, 1);
    assert_eq!(b.snapshot().generation, 1);
    assert!(supervisor.shutdown(LIMIT).await.complete);
    assert_eq!(supervisor.failure(), None);
    write_trace("ignored_children", supervisor.trace_id(), &modes, 5);
    assert!(system.shutdown(LIMIT).await.complete);
}

/// A OneForOne replacement that ignores keeps the restart grant stamped at
/// its admission (V5) and parks the spec as Ignored.
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn trace_replacement_ignore_keeps_its_grant() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("trace-replacement-ignore", 1).unwrap();
    let modes = [RestartMode::Permanent];
    let mut tree = system.supervisor(config(5)).unwrap();
    let a = tree
        .add(
            ChildSpec::new("a", &group, |ctx| async move {
                Ok::<_, String>(Optional(ctx.generation() > 1))
            })
            .restart(modes[0]),
        )
        .unwrap();
    let supervisor = within!(tree.start()).await.unwrap();
    assert!(a.current().unwrap().request(Crash).await.is_err());
    wait_state(&a, ChildState::Ignored).await;
    assert_eq!(a.snapshot().generation, 2);
    assert!(supervisor.shutdown(LIMIT).await.complete);
    write_trace("replacement_ignore", supervisor.trace_id(), &modes, 5);
    assert!(system.shutdown(LIMIT).await.complete);
}

/// V3 composed with an ignore: a replacement that ignores while a blocking
/// helper holds its owned cleanup past shutdown_timeout is killed first
/// (CleanupKill, from "constructing" to "startkilled"; the kill cannot end
/// the helper), and the ignore is observed once the helper is let go within
/// the second interval (Ignore from "startkilled"). Not under the simulator:
/// the helper blocks an OS thread on a std channel.
#[cfg(not(spawned_sim))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trace_ignore_observed_after_its_cleanup_kill() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("trace-ignore-killed", 2).unwrap();
    let modes = [RestartMode::Permanent];
    let (release, held) = std::sync::mpsc::channel::<()>();
    let held = Arc::new(Mutex::new(Some(held)));
    let mut tree = system.supervisor(config(5)).unwrap();
    let a = tree
        .add(
            ChildSpec::new("a", &group, move |ctx| {
                let held = if ctx.generation() > 1 {
                    held.lock().unwrap().take()
                } else {
                    None
                };
                async move {
                    let ignore = held.is_some();
                    if let Some(held) = held {
                        // Wait until the helper runs: a helper not yet
                        // started when its scope stops is skipped.
                        let (entered, running) = tokio::sync::oneshot::channel();
                        ctx.spawn_blocking_helper(move || {
                            let _ = entered.send(());
                            let _ = held.recv();
                        })
                        .map_err(|e| e.to_string())?;
                        running.await.map_err(|e| e.to_string())?;
                    }
                    Ok::<_, String>(Optional(ignore))
                }
            })
            .restart(modes[0])
            .shutdown_timeout(Duration::from_millis(100)),
        )
        .unwrap();
    let supervisor = within!(tree.start()).await.unwrap();
    let trace_id = supervisor.trace_id();
    assert!(a.current().unwrap().request(Crash).await.is_err());
    recorded(trace_id, "CleanupKill").await;
    release.send(()).unwrap();
    wait_state(&a, ChildState::Ignored).await;
    assert_eq!(a.snapshot().generation, 2);
    assert!(supervisor.shutdown(LIMIT).await.complete);
    assert_eq!(supervisor.failure(), None);
    write_trace("ignore_after_kill", supervisor.trace_id(), &modes, 5);
    assert!(system.shutdown(LIMIT).await.complete);
}
