#![cfg(feature = "async")]
// Tests that need OS threads are compiled out under spawned_sim.
#![cfg_attr(spawned_sim, allow(unused_imports, dead_code))]
use spawned_concurrency::{
    message::Message,
    system::{ActorSystem, SystemError},
    tasks::{Actor, ActorStart, Context, Handler},
    Down, ExitReason,
};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
const LIMIT: Duration = Duration::from_secs(3);

struct Ping;
impl Message for Ping {
    type Result = String;
}
struct Worker;
impl Actor for Worker {}
impl Handler<Ping> for Worker {
    async fn handle(&mut self, _: Ping, _: &Context<Self>) -> String {
        std::thread::current()
            .name()
            .unwrap_or_default()
            .to_string()
    }
}
impl Handler<Down> for Worker {
    async fn handle(&mut self, _: Down, _: &Context<Self>) {}
}

// Not simulated: asserts OS worker thread names.
#[cfg(not(spawned_sim))]
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn owned_groups_communicate_and_close_with_stale_handles() {
    let system = ActorSystem::new().unwrap();
    let a = system.async_group("critical", 1).unwrap();
    let b = system.async_group("general", 2).unwrap();
    let first = Worker.start_on(&a).unwrap();
    let second = Worker.start_on(&b).unwrap();
    first.ready().await.unwrap();
    second.ready().await.unwrap();
    assert!(first
        .request(Ping)
        .await
        .unwrap()
        .starts_with("spawned-critical"));
    assert!(second
        .request(Ping)
        .await
        .unwrap()
        .starts_with("spawned-general"));
    let ctx = first.context();
    let foreign = std::thread::spawn(move || {
        let helper = ctx
            .spawn_helper(async {
                spawned_rt::tasks::sleep(Duration::from_millis(1)).await;
                std::thread::current().name().unwrap().to_string()
            })
            .unwrap();
        let timer =
            spawned_concurrency::tasks::send_after(Duration::from_millis(1), ctx.clone(), Ping);
        let monitor = ctx.monitor(&second.child_handle());
        (helper, timer, monitor)
    })
    .join()
    .unwrap();
    assert!(foreign
        .0
        .join()
        .await
        .unwrap()
        .starts_with("spawned-critical"));
    let report = system.shutdown(LIMIT).await;
    assert!(report.complete, "{:?}", report.unfinished_groups);
    assert_eq!(
        first.child_handle().wait_exit_async().await,
        ExitReason::Shutdown
    );
    assert!(matches!(Worker.start_on(&a), Err(SystemError::Closed)));
    assert!(first.request(Ping).await.is_err());
    assert!(first.context().spawn_helper(async {}).is_err());
    // Legacy infallible helper APIs remain inert from a plain thread after the
    // runtime has actually been destroyed; no ambient Tokio runtime is needed.
    std::thread::spawn(move || {
        drop(spawned_concurrency::tasks::send_message_on(
            first.context(),
            async {},
            Ping,
        ));
    })
    .join()
    .unwrap();
}

struct Starting {
    entered: Option<tokio::sync::oneshot::Sender<()>>,
    release: Option<tokio::sync::oneshot::Receiver<()>>,
    stopped: Arc<AtomicUsize>,
}
impl Actor for Starting {
    async fn started(&mut self, _: &Context<Self>) {
        self.entered.take().unwrap().send(()).unwrap();
        let _ = self.release.take().unwrap().await;
    }
    async fn stopped(&mut self, _: &Context<Self>) {
        self.stopped.fetch_add(1, Ordering::SeqCst);
    }
}
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_shutdown_retains_closed_runtime_until_startup_cleanup_finishes() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("slow-start", 1).unwrap();
    let (entered_tx, entered) = tokio::sync::oneshot::channel();
    let (release, release_rx) = tokio::sync::oneshot::channel();
    let stopped = Arc::new(AtomicUsize::new(0));
    let actor = Starting {
        entered: Some(entered_tx),
        release: Some(release_rx),
        stopped: stopped.clone(),
    }
    .start_on(&group)
    .unwrap();
    spawned_rt::tasks::timeout(LIMIT, entered)
        .await
        .unwrap()
        .unwrap();
    let report = system.shutdown(Duration::from_millis(5)).await;
    assert!(!report.complete);
    assert_eq!(report.unfinished_groups, ["slow-start"]);
    assert!(group.is_closed());
    assert!(Worker.start_on(&group).is_err());
    release.send(()).unwrap();
    assert!(report.wait(LIMIT).await.complete);
    assert!(actor.ready().await.is_err());
    assert_eq!(stopped.load(Ordering::SeqCst), 1);
}

struct Probe(Arc<AtomicUsize>);
impl Drop for Probe {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
struct WithHelper {
    dropped: Arc<AtomicUsize>,
    registered: Option<tokio::sync::oneshot::Sender<()>>,
}
impl Actor for WithHelper {
    async fn started(&mut self, ctx: &Context<Self>) {
        let probe = Probe(self.dropped.clone());
        let task = ctx
            .spawn_helper(async move {
                let _probe = probe;
                std::future::pending::<()>().await;
            })
            .unwrap();
        drop(task); // Ownership remains with the scope despite a dropped join handle.
        ctx.register_name("execution-group-owned-helper-test")
            .unwrap();
        self.registered.take().unwrap().send(()).unwrap();
    }
}
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_joins_helper_drops_and_generation_owned_registration() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("helpers", 1).unwrap();
    let dropped = Arc::new(AtomicUsize::new(0));
    let (tx, rx) = tokio::sync::oneshot::channel();
    let actor = WithHelper {
        dropped: dropped.clone(),
        registered: Some(tx),
    }
    .start_on(&group)
    .unwrap();
    rx.await.unwrap();
    actor.ready().await.unwrap();
    assert!(spawned_concurrency::registry::whereis::<
        spawned_concurrency::tasks::ActorRef<WithHelper>,
    >("execution-group-owned-helper-test")
    .is_some());
    assert!(system.shutdown(LIMIT).await.complete);
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    assert!(spawned_concurrency::registry::whereis::<
        spawned_concurrency::tasks::ActorRef<WithHelper>,
    >("execution-group-owned-helper-test")
    .is_none());
}

// Not simulated: drops the system from a foreign OS thread.
#[cfg(not(spawned_sim))]
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_system_requests_shutdown_without_an_ambient_runtime() {
    let system = ActorSystem::new().unwrap();
    assert!(system.async_group("bad", 0).is_err());
    let group = system.async_group("drop", 1).unwrap();
    assert!(system.async_group("drop", 2).is_err());
    let actor = Worker.start_on(&group).unwrap();
    actor.ready().await.unwrap();
    std::thread::spawn(move || drop(system)).join().unwrap();
    assert_eq!(
        spawned_rt::tasks::timeout(LIMIT, actor.child_handle().wait_exit_async())
            .await
            .unwrap(),
        ExitReason::Shutdown
    );
}

// Not simulated: uses spawn_blocking.
#[cfg(not(spawned_sim))]
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn registered_blocking_work_keeps_runtime_owned_until_native_completion() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("blocking-helper", 1).unwrap();
    let actor = Worker.start_on(&group).unwrap();
    actor.ready().await.unwrap();
    let (entered_tx, entered) = tokio::sync::oneshot::channel();
    let (release, release_rx) = std::sync::mpsc::channel();
    let dropped = Arc::new(AtomicUsize::new(0));
    let probe = Probe(dropped.clone());
    let helper = actor
        .context()
        .spawn_blocking_helper(move || {
            let _probe = probe;
            let _ = entered_tx.send(());
            let _ = release_rx.recv();
        })
        .unwrap();
    spawned_rt::tasks::timeout(LIMIT, entered)
        .await
        .unwrap()
        .unwrap();
    helper.abort(); // Active native code cannot be preempted.
    drop(helper);
    let report = system.shutdown(Duration::from_millis(10)).await;
    assert!(!report.complete);
    assert_eq!(dropped.load(Ordering::SeqCst), 0);
    release.send(()).unwrap();
    assert!(report.wait(LIMIT).await.complete);
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
}

// Not simulated: uses spawn_blocking.
#[cfg(not(spawned_sim))]
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn scoped_helpers_report_panics_and_framework_abort_drops_payload() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("helper-errors", 1).unwrap();
    let actor = Worker.start_on(&group).unwrap();
    actor.ready().await.unwrap();
    let async_error = actor
        .context()
        .spawn_helper(async { panic!("async helper panic") })
        .unwrap()
        .join()
        .await;
    assert!(matches!(
        async_error,
        Err(spawned_concurrency::system::HelperError::Panicked(_))
    ));
    let native_error = actor
        .context()
        .spawn_blocking_helper(|| panic!("native helper panic"))
        .unwrap()
        .join()
        .await;
    assert!(matches!(
        native_error,
        Err(spawned_concurrency::system::HelperError::Panicked(_))
    ));
    let dropped = Arc::new(AtomicUsize::new(0));
    let probe = Probe(dropped.clone());
    let future = async move {
        let _probe = probe;
        std::future::pending::<()>().await;
    };
    let helper = spawned_concurrency::tasks::send_message_on(actor.context(), future, Ping);
    helper.abort();
    let _ = helper.await;
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    assert!(system.shutdown(LIMIT).await.complete);
}

// Not simulated: asserts OS worker thread names.
#[cfg(not(spawned_sim))]
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn ambient_actor_started_inside_group_is_unmanaged_and_aborted_at_teardown() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("ambient-child", 1).unwrap();
    let managed = Worker.start_on(&group).unwrap();
    managed.ready().await.unwrap();
    let ambient = managed
        .context()
        .spawn_helper(async { Worker.start() })
        .unwrap()
        .join()
        .await
        .unwrap();
    assert!(ambient.context().execution_group().is_none());
    assert!(matches!(
        ambient.ready().await,
        Err(SystemError::UnmanagedActor)
    ));
    assert!(ambient.context().spawn_helper(async {}).is_err());
    assert!(ambient
        .request(Ping)
        .await
        .unwrap()
        .starts_with("spawned-ambient-child"));
    assert!(system.shutdown(LIMIT).await.complete);
    assert_eq!(ambient.wait_exit().await, ExitReason::Kill);
    assert_eq!(managed.wait_exit().await, ExitReason::Shutdown);
}

struct Queued(Probe);
impl Message for Queued {
    type Result = ();
}
struct FailedStartup {
    entered: Option<tokio::sync::oneshot::Sender<()>>,
    release: Option<tokio::sync::oneshot::Receiver<()>>,
    handled: Arc<AtomicUsize>,
    stopped: Arc<AtomicUsize>,
}
impl Actor for FailedStartup {
    async fn started(&mut self, _: &Context<Self>) {
        let _ = self.entered.take().unwrap().send(());
        let _ = self.release.take().unwrap().await;
        panic!("injected readiness failure");
    }
    async fn stopped(&mut self, _: &Context<Self>) {
        self.stopped.fetch_add(1, Ordering::SeqCst);
    }
}
impl Handler<Queued> for FailedStartup {
    async fn handle(&mut self, msg: Queued, _: &Context<Self>) {
        self.handled.fetch_add(1, Ordering::SeqCst);
        drop(msg.0);
    }
}
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_startup_discards_queued_payloads_and_reply_channels_without_dispatch() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("failed-startup", 1).unwrap();
    let (entered_tx, entered) = tokio::sync::oneshot::channel();
    let (release, release_rx) = tokio::sync::oneshot::channel();
    let handled = Arc::new(AtomicUsize::new(0));
    let stopped = Arc::new(AtomicUsize::new(0));
    let dropped = Arc::new(AtomicUsize::new(0));
    let actor = FailedStartup {
        entered: Some(entered_tx),
        release: Some(release_rx),
        handled: handled.clone(),
        stopped: stopped.clone(),
    }
    .start_on(&group)
    .unwrap();
    spawned_rt::tasks::timeout(LIMIT, entered)
        .await
        .unwrap()
        .unwrap();
    let reply = actor.request_raw(Queued(Probe(dropped.clone()))).unwrap();
    actor.send(Queued(Probe(dropped.clone()))).unwrap();
    assert_eq!(handled.load(Ordering::SeqCst), 0);
    release.send(()).unwrap();
    assert!(matches!(
        spawned_rt::tasks::timeout(LIMIT, actor.ready())
            .await
            .unwrap(),
        Err(SystemError::StartupFailed(ExitReason::Panic(_)))
    ));
    assert!(matches!(
        spawned_rt::tasks::timeout(LIMIT, actor.wait_exit())
            .await
            .unwrap(),
        ExitReason::Panic(_)
    ));
    assert!(reply.await.is_err());
    assert_eq!(handled.load(Ordering::SeqCst), 0);
    assert_eq!(stopped.load(Ordering::SeqCst), 0); // Legacy started-panic contract.
    assert_eq!(dropped.load(Ordering::SeqCst), 2);
    assert!(system.shutdown(LIMIT).await.complete);
}

#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn old_scope_cleanup_cannot_remove_a_replacement_registration() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("registry-replacement", 1).unwrap();
    let actor = Worker.start_on(&group).unwrap();
    actor.ready().await.unwrap();
    let name = "execution-groups-stale-registration";
    actor.context().register_name(name).unwrap();
    assert!(matches!(
        actor.context().register_name(name),
        Err(
            spawned_concurrency::registry::ManagedRegistrationError::Registry(
                spawned_concurrency::registry::RegistryError::AlreadyRegistered(_)
            )
        )
    ));
    spawned_concurrency::registry::unregister(name);
    let replacement =
        spawned_concurrency::registry::register_owned(name, actor.id(), 42u64).unwrap();
    assert!(system.shutdown(LIMIT).await.complete);
    assert!(matches!(
        actor.context().register_name(name),
        Err(spawned_concurrency::registry::ManagedRegistrationError::OwnerClosed)
    ));
    assert_eq!(
        spawned_concurrency::registry::whereis::<u64>(name),
        Some(42)
    );
    drop(replacement);
    assert!(spawned_concurrency::registry::whereis::<u64>(name).is_none());
}

#[spawned_concurrency::protocol]
trait ManagedProtocol {
    fn value(&self) -> spawned_concurrency::Response<usize>;
}
struct ManagedProtocolActor(usize);
#[spawned_concurrency::actor(protocol = ManagedProtocol)]
impl ManagedProtocolActor {
    #[request_handler]
    async fn handle_value(&mut self, _: managed_protocol::Value, _: &Context<Self>) -> usize {
        self.0
    }
}
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn generated_protocols_work_unchanged_with_explicit_group_placement() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("generated-protocol", 1).unwrap();
    let actor = ManagedProtocolActor(42).start_on(&group).unwrap();
    actor.ready().await.unwrap();
    assert_eq!(actor.value().await.unwrap(), 42);
    assert!(system.shutdown(LIMIT).await.complete);
    assert!(actor.value().await.is_err());
}

#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn stuck_standalone_cleanup_does_not_leave_earlier_actor_running_forever() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("stuck-standalone-order", 1).unwrap();
    let first = Worker.start_on(&group).unwrap();
    first.ready().await.unwrap();
    let (entered_tx, entered) = tokio::sync::oneshot::channel();
    let (release, release_rx) = tokio::sync::oneshot::channel();
    struct Stuck {
        entered: Option<tokio::sync::oneshot::Sender<()>>,
        release: Option<tokio::sync::oneshot::Receiver<()>>,
    }
    impl Actor for Stuck {
        async fn stopped(&mut self, _: &Context<Self>) {
            let _ = self.entered.take().unwrap().send(());
            let _ = self.release.take().unwrap().await;
        }
    }
    let last = Stuck {
        entered: Some(entered_tx),
        release: Some(release_rx),
    }
    .start_on(&group)
    .unwrap();
    last.ready().await.unwrap();
    let report = system.shutdown(Duration::from_millis(20)).await;
    assert!(!report.complete);
    spawned_rt::tasks::timeout(LIMIT, entered)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        spawned_rt::tasks::timeout(LIMIT, first.wait_exit())
            .await
            .unwrap(),
        ExitReason::Shutdown
    );
    release.send(()).unwrap();
    assert!(report.wait(LIMIT).await.complete);
}

struct StartupPanicAfterStop {
    in_stopped: bool,
}
impl Actor for StartupPanicAfterStop {
    async fn started(&mut self, ctx: &Context<Self>) {
        ctx.stop();
        spawned_rt::tasks::yield_now().await;
        if !self.in_stopped {
            panic!("startup panic after stop");
        }
    }
    async fn stopped(&mut self, _: &Context<Self>) {
        panic!("cleanup panic after startup stop");
    }
}
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn readiness_reports_actual_panic_after_startup_stop_instead_of_early_normal() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("startup-panic-cause", 2).unwrap();
    for in_stopped in [false, true] {
        for _ in 0..16 {
            let actor = StartupPanicAfterStop { in_stopped }
                .start_on(&group)
                .unwrap();
            let ready = spawned_rt::tasks::timeout(LIMIT, actor.ready())
                .await
                .unwrap();
            assert!(matches!(
                ready,
                Err(SystemError::StartupFailed(ExitReason::Panic(_)))
            ));
            assert!(matches!(actor.wait_exit().await, ExitReason::Panic(_)));
        }
    }
    assert!(system.shutdown(LIMIT).await.complete);
}

struct SnapshotHeld {
    release: Option<tokio::sync::oneshot::Receiver<()>>,
}
impl Actor for SnapshotHeld {
    async fn stopped(&mut self, _: &Context<Self>) {
        let _ = self.release.take().unwrap().await;
    }
}
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_zero_deadline_reports_include_scopes_during_shutdown_initialization() {
    let system = Arc::new(ActorSystem::new().unwrap());
    let group = system.async_group("concurrent-reports", 1).unwrap();
    let (release, release_rx) = tokio::sync::oneshot::channel();
    let actor = SnapshotHeld {
        release: Some(release_rx),
    }
    .start_on(&group)
    .unwrap();
    actor.ready().await.unwrap();
    let mut reporters = Vec::new();
    for _ in 0..64 {
        let owner = system.clone();
        reporters.push(spawned_rt::tasks::spawn(async move {
            owner.shutdown(Duration::ZERO).await
        }));
    }
    for reporter in reporters {
        let report = reporter.await.unwrap();
        assert!(!report.complete);
        assert!(report.unfinished_actors.contains(&actor.id()));
    }
    release.send(()).unwrap();
    assert!(system.shutdown(LIMIT).await.complete);
}

struct CheckAdmission;
impl Message for CheckAdmission {
    type Result = bool;
}
struct HelperAware;
impl Actor for HelperAware {}
impl Handler<CheckAdmission> for HelperAware {
    async fn handle(&mut self, _: CheckAdmission, ctx: &Context<Self>) -> bool {
        ctx.spawn_helper(async {}).is_err()
    }
}
struct Dependent {
    earlier: spawned_concurrency::tasks::ActorRef<HelperAware>,
    observed: Arc<AtomicUsize>,
}
impl Actor for Dependent {
    async fn stopped(&mut self, _: &Context<Self>) {
        assert!(self.earlier.request(CheckAdmission).await.unwrap());
        self.observed.fetch_add(1, Ordering::SeqCst);
    }
}
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn ordered_shutdown_preserves_ordinary_calls_but_closes_new_helper_admission() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("shutdown-admission", 1).unwrap();
    let earlier = HelperAware.start_on(&group).unwrap();
    earlier.ready().await.unwrap();
    let observed = Arc::new(AtomicUsize::new(0));
    Dependent {
        earlier,
        observed: observed.clone(),
    }
    .start_on(&group)
    .unwrap()
    .ready()
    .await
    .unwrap();
    let report = system.shutdown(LIMIT).await;
    assert!(report.complete);
    assert_eq!(observed.load(Ordering::SeqCst), 1);
    assert!(report
        .exits
        .iter()
        .all(|e| e.reason == ExitReason::Shutdown));
}

// Gaps found by mutation testing (cargo-mutants on system.rs).

/// Aborting a registered helper makes its join report cancellation
/// (ScopedTask::abort and the cancelled branch of ScopedTask::join).
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn aborted_scoped_helper_joins_as_cancelled() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("helper-abort", 1).unwrap();
    let actor = Worker.start_on(&group).unwrap();
    actor.ready().await.unwrap();
    let helper = actor
        .context()
        .spawn_helper(std::future::pending::<()>())
        .unwrap();
    helper.abort();
    let joined = spawned_rt::tasks::timeout(LIMIT, helper.join())
        .await
        .expect("aborted helper never completed");
    assert_eq!(
        joined,
        Err(spawned_concurrency::system::HelperError::Cancelled)
    );
    assert!(system.shutdown(LIMIT).await.complete);
}

/// Group names and worker budgets are validated and reported back.
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn groups_validate_names_and_report_their_budget() {
    let system = ActorSystem::new().unwrap();
    for name in ["", "nul\0name"] {
        assert!(matches!(
            system.async_group(name, 1),
            Err(spawned_concurrency::system::SystemError::InvalidName)
        ));
    }
    assert!(matches!(
        system.async_group("zero", 0),
        Err(spawned_concurrency::system::SystemError::InvalidWorkerBudget)
    ));
    let group = system.async_group("budgeted", 3).unwrap();
    assert_eq!(group.name().as_deref(), Some("budgeted"));
    assert_eq!(group.worker_budget(), Some(3));
    assert!(system.shutdown(LIMIT).await.complete);
}

#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn closed_system_rejects_new_groups_and_completed_reports_stay_complete() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("closing", 1).unwrap();
    assert_eq!(
        format!("{group:?}"),
        "ExecutionGroup { name: Some(\"closing\") }"
    );
    let report = system.shutdown(LIMIT).await;
    assert!(report.complete);
    // A completed report has nothing left to wait for.
    let again = spawned_rt::tasks::timeout(LIMIT, report.wait(Duration::ZERO))
        .await
        .unwrap();
    assert!(again.complete);
    assert!(matches!(
        system.async_group("late", 1),
        Err(SystemError::Closed)
    ));
    // Native groups need OS threads, which the simulator rejects first.
    #[cfg(not(spawned_sim))]
    assert!(matches!(
        system.thread_group("late-native", 1),
        Err(SystemError::Closed)
    ));
    assert!(group.is_closed());
    assert!(matches!(Worker.start_on(&group), Err(SystemError::Closed)));
}

// Not simulated: shards belong to native thread groups.
#[cfg(not(spawned_sim))]
#[test]
fn shard_selection_accepts_every_slot_and_rejects_the_next() {
    let system = ActorSystem::new().unwrap();
    let group = system.thread_group("two-slots", 2).unwrap();
    for index in 0..2 {
        assert!(group.shard(index).is_ok(), "slot {index} rejected");
    }
    assert!(matches!(group.shard(2), Err(SystemError::InvalidShard)));
    let pool = system.async_group("pool", 2).unwrap();
    assert!(matches!(pool.shard(0), Err(SystemError::InvalidShard)));
}
