//! Drives each lifecycle failpoint site (`--features failpoints`) and checks
//! that framework invariants hold: supervisors restart per policy and budget,
//! shutdown completes or reports within its deadline, a generation is never
//! published after stop or twice, and registry entries do not outlive owners.
//!
//! Sites are process-global; every test holds a `FailScenario`, which
//! serializes the tests in this binary and prints the seed on failure.
#![cfg(all(feature = "failpoints", feature = "async"))]

use spawned_concurrency::{
    failpoints::{self, Action, FailPoint, FailScenario},
    message::Message,
    registry,
    supervision::{Backoff, ChildSpec, ChildState, SupervisorConfig, SupervisorError},
    system::{ActorSystem, SystemError},
    tasks::{Actor, ActorRef, ActorStart, Context, Handler},
    ChildHandle, Down, ExitReason,
};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

const LIMIT: Duration = Duration::from_secs(4);

fn config(max_restarts: usize) -> SupervisorConfig {
    SupervisorConfig {
        max_restarts,
        backoff: Backoff {
            initial: Duration::from_millis(1),
            maximum: Duration::from_millis(4),
            jitter: false,
            seed: Some(7),
            ..Backoff::default()
        },
        ..SupervisorConfig::default()
    }
}

/// Wait until a site has been reached `n` times.
async fn reached(site: &str, n: u64) {
    let began = Instant::now();
    while failpoints::stats(site).hits < n {
        assert!(began.elapsed() < LIMIT, "site {site} not reached {n} times");
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

struct Crash;
impl Message for Crash {
    type Result = ();
}
struct Ping;
impl Message for Ping {
    type Result = ();
}

/// Records each started generation; optionally registers a name.
#[derive(Clone, Default)]
struct Started(Arc<Mutex<Vec<ChildHandle>>>);
impl Started {
    fn handles(&self) -> Vec<ChildHandle> {
        self.0.lock().unwrap().clone()
    }
}
struct Worker {
    started: Started,
    /// Trap exits, so stopped() runs on a parent's shutdown (v0.8, E3).
    trap: bool,
    name: Option<&'static str>,
    registration: Arc<Mutex<Option<bool>>>,
}
impl Worker {
    fn new(started: &Started) -> Self {
        Self {
            started: started.clone(),
            trap: false,
            name: None,
            registration: Default::default(),
        }
    }
}
impl Actor for Worker {
    async fn started(&mut self, ctx: &Context<Self>) {
        if self.trap {
            ctx.trap_exit(true);
        }
        self.started
            .0
            .lock()
            .unwrap()
            .push(ctx.actor_ref().child_handle());
        if let Some(name) = self.name {
            *self.registration.lock().unwrap() = Some(ctx.register_name(name).is_ok());
        }
    }
}
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
impl Handler<Ping> for Worker {
    async fn handle(
        &mut self,
        _: Ping,
        reply: spawned_concurrency::reply::PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
        reply.reply(());
    }
}

// ---------------------------------------------------------------- child.factory

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn factory_errors_restart_within_budget_and_count_generations() {
    let _scenario = FailScenario::setup();
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("fp-factory", 1).unwrap();
    let started = Started::default();
    let constructed = Arc::new(AtomicUsize::new(0));
    let mut tree = system.supervisor(config(10)).unwrap();
    let (s, c) = (started.clone(), constructed.clone());
    let child = tree
        .add(ChildSpec::new("worker", &group, move |_| {
            let (s, c) = (s.clone(), c.clone());
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Ok::<_, String>(Worker::new(&s))
            }
        }))
        .unwrap();
    let tree = tree.start().await.unwrap();
    // v0.8 (S1): a first start is never retried, so the failures are
    // injected into the replacements after a crash.
    failpoints::configure("child.factory", FailPoint::new(Action::Error).times(3)).unwrap();
    child.wait_ready().await.unwrap().send(Crash).unwrap();
    let current = child.wait_for_generation(1).await.unwrap();
    // Three injected failures, each its own generation, then one success.
    assert_eq!(failpoints::stats("child.factory").fired, 3);
    assert_eq!(child.snapshot().generation, 5);
    assert_eq!(constructed.load(Ordering::SeqCst), 2);
    assert_eq!(started.handles().len(), 2);
    assert_eq!(started.handles()[1].id(), current.id());
    assert!(tree.failure().is_none());
    let report = tree.shutdown(LIMIT).await;
    assert!(report.complete && report.failure.is_none());
    assert!(system.shutdown(LIMIT).await.complete);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persistent_factory_failure_fails_startup_without_retry() {
    let _scenario = FailScenario::setup();
    failpoints::configure("child.factory", Action::Error).unwrap();
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("fp-exhaust", 1).unwrap();
    let started = Started::default();
    let mut tree = system.supervisor(config(2)).unwrap();
    let s = started.clone();
    let child = tree
        .add(ChildSpec::new("worker", &group, move |_| {
            let s = s.clone();
            async move { Ok::<_, String>(Worker::new(&s)) }
        }))
        .unwrap();
    match tree.start().await {
        Err(SupervisorError::FailedToStartChild { child, cause, .. }) => {
            assert_eq!(child, "worker");
            assert!(cause.contains("failpoint child.factory"), "{cause}")
        }
        other => panic!("expected a first-start failure, got {:?}", other.err()),
    }
    // v0.8 (S1): one attempt; a first start is not retried.
    assert_eq!(failpoints::stats("child.factory").fired, 1);
    assert!(started.handles().is_empty());
    assert!(child.current().is_err());
    assert!(system.shutdown(LIMIT).await.complete);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn factory_panic_is_a_failed_start_not_a_controller_failure() {
    let _scenario = FailScenario::setup();
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("fp-factory-panic", 1).unwrap();
    let started = Started::default();
    let mut tree = system.supervisor(config(5)).unwrap();
    let s = started.clone();
    let child = tree
        .add(ChildSpec::new("worker", &group, move |_| {
            let s = s.clone();
            async move { Ok::<_, String>(Worker::new(&s)) }
        }))
        .unwrap();
    let tree = tree.start().await.unwrap();
    // v0.8 (S1): injected into the replacement; a first start is not retried.
    failpoints::configure("child.factory", FailPoint::new(Action::Panic).times(1)).unwrap();
    child.wait_ready().await.unwrap().send(Crash).unwrap();
    child.wait_for_generation(1).await.unwrap();
    let snapshot = child.snapshot();
    assert_eq!(snapshot.generation, 3);
    assert!(snapshot
        .last_error
        .as_deref()
        .is_some_and(|e| e.contains("factory panicked") && e.contains("child.factory")));
    assert!(tree.failure().is_none());
    assert!(tree.shutdown(LIMIT).await.complete);
    assert!(system.shutdown(LIMIT).await.complete);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn seeded_probabilistic_factory_failures_match_generation_count() {
    let _scenario = FailScenario::setup();
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("fp-probability", 1).unwrap();
    let started = Started::default();
    let mut tree = system.supervisor(config(1000)).unwrap();
    let s = started.clone();
    let child = tree
        .add(ChildSpec::new("worker", &group, move |_| {
            let s = s.clone();
            async move { Ok::<_, String>(Worker::new(&s)) }
        }))
        .unwrap();
    let tree = tree.start().await.unwrap();
    let mut actor = child.wait_ready().await.unwrap();
    // Armed after the initial start: since S1 any failed first start fails
    // `start()`, so only replacements may fail (and are retried).
    failpoints::configure(
        "child.factory",
        FailPoint::new(Action::Error).probability(0.5),
    )
    .unwrap();
    for _ in 0..8 {
        let generation = child.snapshot().generation;
        actor.send(Crash).unwrap();
        actor = child.wait_for_generation(generation).await.unwrap();
    }
    // Every generation is either an injected failure or a started actor.
    let fired = failpoints::stats("child.factory").fired as usize;
    assert_eq!(
        child.snapshot().generation as usize,
        fired + started.handles().len()
    );
    assert_eq!(started.handles().len(), 9);
    assert!(tree.failure().is_none());
    assert!(tree.shutdown(LIMIT).await.complete);
    assert!(system.shutdown(LIMIT).await.complete);
}

// ------------------------------------------------ child.after_ready_before_publish

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ready_generation_discarded_before_publication_is_never_current() {
    let _scenario = FailScenario::setup();
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("fp-discard", 1).unwrap();
    let started = Started::default();
    let mut tree = system.supervisor(config(5)).unwrap();
    let s = started.clone();
    let child = tree
        .add(ChildSpec::new("worker", &group, move |_| {
            let s = s.clone();
            async move { Ok::<_, String>(Worker::new(&s)) }
        }))
        .unwrap();
    let tree = tree.start().await.unwrap();
    // v0.8 (S1): a discarded first start fails start(), so discard a
    // replacement.
    failpoints::configure(
        "child.after_ready_before_publish",
        FailPoint::new(Action::Error).times(1),
    )
    .unwrap();
    child.wait_ready().await.unwrap().send(Crash).unwrap();
    let current = child.wait_for_generation(1).await.unwrap();
    let handles = started.handles();
    assert_eq!(handles.len(), 3);
    assert_ne!(handles[1].id(), current.id());
    assert_eq!(handles[2].id(), current.id());
    // The discarded generation was stopped, not leaked.
    let reason = tokio::time::timeout(LIMIT, handles[1].wait_exit_async())
        .await
        .unwrap();
    assert_eq!(reason, ExitReason::Shutdown);
    assert_eq!(child.snapshot().generation, 3);
    assert!(tree.shutdown(LIMIT).await.complete);
    assert!(system.shutdown(LIMIT).await.complete);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_during_publication_window_never_publishes() {
    let _scenario = FailScenario::setup();
    failpoints::configure(
        "child.after_ready_before_publish",
        FailPoint::new(Action::Delay(Duration::from_millis(300))).times(1),
    )
    .unwrap();
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("fp-publish-race", 1).unwrap();
    let started = Started::default();
    let mut tree = system.supervisor(config(5)).unwrap();
    let s = started.clone();
    let child = tree
        .add(ChildSpec::new("worker", &group, move |_| {
            let s = s.clone();
            async move { Ok::<_, String>(Worker::new(&s)) }
        }))
        .unwrap();
    let startup = tokio::spawn(tree.start());
    reached("child.after_ready_before_publish", 1).await;
    let report = system.shutdown(LIMIT).await;
    assert!(report.complete, "{:?}", report.unfinished_actors);
    assert!(matches!(
        startup.await.unwrap(),
        Err(SupervisorError::Stopped)
    ));
    let snapshot = child.snapshot();
    assert_eq!(snapshot.generation, 1);
    assert_eq!(snapshot.state, ChildState::Stopped);
    assert!(child.current().is_err());
    assert_eq!(started.handles().len(), 1);
    assert!(started.handles()[0].exit_reason().is_some());
}

// ------------------------------------- supervisor.before_restart / during_backoff

async fn stop_while_restart_is_pending(site: &str) {
    let _scenario = FailScenario::setup();
    failpoints::configure(
        site,
        FailPoint::new(Action::Delay(Duration::from_millis(300))).times(1),
    )
    .unwrap();
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("fp-restart-race", 1).unwrap();
    let started = Started::default();
    let constructed = Arc::new(AtomicUsize::new(0));
    let mut tree = system.supervisor(config(5)).unwrap();
    let (s, c) = (started.clone(), constructed.clone());
    let child = tree
        .add(ChildSpec::new("worker", &group, move |_| {
            let (s, c) = (s.clone(), c.clone());
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Ok::<_, String>(Worker::new(&s))
            }
        }))
        .unwrap();
    let tree = tree.start().await.unwrap();
    child.wait_ready().await.unwrap().send(Crash).unwrap();
    reached(site, 1).await;
    let report = tree.shutdown(LIMIT).await;
    assert!(report.complete);
    assert!(report.failure.is_none(), "{:?}", report.failure);
    // The stop won: no generation was started after it.
    assert_eq!(constructed.load(Ordering::SeqCst), 1);
    assert_eq!(child.snapshot().generation, 1);
    assert_eq!(child.snapshot().state, ChildState::Stopped);
    assert!(system.shutdown(LIMIT).await.complete);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_before_restart_prevents_new_generation() {
    stop_while_restart_is_pending("supervisor.before_restart").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_after_backoff_before_retry_prevents_new_generation() {
    stop_while_restart_is_pending("supervisor.during_backoff").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn controller_panic_before_restart_fails_supervisor_and_shuts_down() {
    let _scenario = FailScenario::setup();
    failpoints::configure(
        "supervisor.before_restart",
        FailPoint::new(Action::Panic).times(1),
    )
    .unwrap();
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("fp-controller-panic", 1).unwrap();
    let started = Started::default();
    let mut tree = system.supervisor(config(5)).unwrap();
    let s = started.clone();
    let child = tree
        .add(ChildSpec::new("worker", &group, move |_| {
            let s = s.clone();
            async move { Ok::<_, String>(Worker::new(&s)) }
        }))
        .unwrap();
    let tree = tree.start().await.unwrap();
    child.wait_ready().await.unwrap().send(Crash).unwrap();
    tokio::time::timeout(LIMIT, tree.join()).await.unwrap();
    assert!(tree
        .failure()
        .is_some_and(|f| f.contains("child controller panicked")));
    assert_eq!(child.snapshot().state, ChildState::Failed);
    assert!(system.shutdown(LIMIT).await.complete);
}

// -------------------------------------------------------- child.before_cleanup_join

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slow_controller_after_completed_cleanup_is_not_a_cleanup_failure() {
    let _scenario = FailScenario::setup();
    failpoints::configure(
        "child.before_cleanup_join",
        FailPoint::new(Action::Delay(Duration::from_millis(300))).times(1),
    )
    .unwrap();
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("fp-cleanup", 1).unwrap();
    let started = Started::default();
    let mut tree = system.supervisor(config(5)).unwrap();
    let s = started.clone();
    let child = tree
        .add(
            ChildSpec::new("worker", &group, move |_| {
                let s = s.clone();
                async move { Ok::<_, String>(Worker::new(&s)) }
            })
            .shutdown_timeout(Duration::from_millis(100)),
        )
        .unwrap();
    let tree = tree.start().await.unwrap();
    child.wait_ready().await.unwrap().send(Crash).unwrap();
    child.wait_for_generation(1).await.unwrap();
    assert_eq!(failpoints::stats("child.before_cleanup_join").fired, 1);
    assert!(tree.failure().is_none(), "{:?}", tree.failure());
    assert!(tree.shutdown(LIMIT).await.complete);
    assert!(system.shutdown(LIMIT).await.complete);
}

// -------------------------------------------------------- shutdown.before_stop_child

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slow_stop_turn_reports_incomplete_at_deadline_then_joins() {
    let _scenario = FailScenario::setup();
    failpoints::configure(
        "shutdown.before_stop_child",
        FailPoint::new(Action::Delay(Duration::from_millis(2000))).times(1),
    )
    .unwrap();
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("fp-stop-turn", 1).unwrap();
    let started = Started::default();
    let mut tree = system.supervisor(config(5)).unwrap();
    let s = started.clone();
    let child = tree
        .add(ChildSpec::new("worker", &group, move |_| {
            let s = s.clone();
            async move { Ok::<_, String>(Worker::new(&s)) }
        }))
        .unwrap();
    let tree = tree.start().await.unwrap();
    child.wait_ready().await.unwrap();
    let began = Instant::now();
    let report = tree.shutdown(Duration::from_millis(100)).await;
    assert!(!report.complete);
    // Returns at its deadline, well before the delayed stop turn finishes.
    assert!(began.elapsed() < Duration::from_millis(1500));
    tokio::time::timeout(LIMIT, tree.join()).await.unwrap();
    assert!(tree.failure().is_none(), "{:?}", tree.failure());
    assert_eq!(child.snapshot().state, ChildState::Stopped);
    assert!(system.shutdown(LIMIT).await.complete);
}

// ---------------------------------------------------- system.shutdown_before_target

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slow_system_coordinator_reports_then_completes() {
    let _scenario = FailScenario::setup();
    failpoints::configure(
        "system.shutdown_before_target",
        FailPoint::new(Action::Delay(Duration::from_millis(300))).times(1),
    )
    .unwrap();
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("fp-system", 2).unwrap();
    let started = Started::default();
    let a = group.start(Worker::new(&started)).unwrap();
    let b = group.start(Worker::new(&started)).unwrap();
    a.ready().await.unwrap();
    b.ready().await.unwrap();
    let report = system.shutdown(Duration::from_millis(50)).await;
    assert!(!report.complete);
    let report = report.wait(LIMIT).await;
    assert!(report.complete);
    assert!(a.exit_reason().is_some() && b.exit_reason().is_some());
}

// ------------------------------------------------- system.group_start / reserve

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_group_start_rolls_back_and_frees_the_name() {
    let _scenario = FailScenario::setup();
    failpoints::configure("system.group_start", FailPoint::new(Action::Error).times(2)).unwrap();
    let system = ActorSystem::new().unwrap();
    assert!(matches!(
        system.async_group("fp-group", 2),
        Err(SystemError::Runtime(_))
    ));
    assert!(matches!(
        system.thread_group("fp-threads", 2),
        Err(SystemError::Runtime(_))
    ));
    let group = system.async_group("fp-group", 2).unwrap();
    let threads = system.thread_group("fp-threads", 2).unwrap();
    let started = Started::default();
    group
        .start(Worker::new(&started))
        .unwrap()
        .ready()
        .await
        .unwrap();
    threads
        .start(Worker::new(&started))
        .unwrap()
        .ready()
        .await
        .unwrap();
    assert!(system.shutdown(LIMIT).await.complete);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn placement_failure_is_reported_and_fails_the_supervisor() {
    let _scenario = FailScenario::setup();
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("fp-reserve", 1).unwrap();
    let started = Started::default();
    failpoints::configure("system.reserve", FailPoint::new(Action::Error).times(1)).unwrap();
    assert!(matches!(
        group.start(Worker::new(&started)),
        Err(SystemError::Capacity)
    ));
    failpoints::configure("system.reserve", Action::Error).unwrap();
    let mut tree = system.supervisor(config(5)).unwrap();
    let s = started.clone();
    let child = tree
        .add(ChildSpec::new("worker", &group, move |_| {
            let s = s.clone();
            async move { Ok::<_, String>(Worker::new(&s)) }
        }))
        .unwrap();
    match tree.start().await {
        Err(SupervisorError::FailedToStartChild { child, cause, .. }) => {
            assert_eq!(child, "worker");
            assert!(cause.contains("placement failed"), "{cause}")
        }
        other => panic!("expected placement failure, got {:?}", other.err()),
    }
    // v0.8 (S2): start() returns after the rollback, so the never-started
    // child is already terminal (#223 polled for it).
    assert_eq!(child.snapshot().state, ChildState::Failed);
    assert_eq!(child.snapshot().generation, 0);
    failpoints::remove("system.reserve");
    group
        .start(Worker::new(&started))
        .unwrap()
        .ready()
        .await
        .unwrap();
    assert!(started.handles().len() == 1);
    assert!(system.shutdown(LIMIT).await.complete);
}

// ------------------------------------------- registry.before_register / unregister

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn registration_racing_shutdown_leaves_no_entry() {
    let _scenario = FailScenario::setup();
    failpoints::configure(
        "registry.before_register",
        FailPoint::new(Action::Delay(Duration::from_millis(300))).times(1),
    )
    .unwrap();
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("fp-register", 1).unwrap();
    let started = Started::default();
    let mut worker = Worker::new(&started);
    worker.name = Some("fp-register-race");
    let registration = worker.registration.clone();
    let actor = group.start(worker).unwrap();
    reached("registry.before_register", 1).await;
    assert!(system.shutdown(LIMIT).await.complete);
    assert_eq!(*registration.lock().unwrap(), Some(false));
    assert!(registry::whereis::<ActorRef<Worker>>("fp-register-race").is_none());
    assert!(actor.ready().await.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn injected_registration_error_leaves_no_entry() {
    let _scenario = FailScenario::setup();
    failpoints::configure("registry.before_register", Action::Error).unwrap();
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("fp-register-error", 1).unwrap();
    let started = Started::default();
    let mut worker = Worker::new(&started);
    worker.name = Some("fp-register-error");
    let registration = worker.registration.clone();
    group.start(worker).unwrap().ready().await.unwrap();
    assert_eq!(*registration.lock().unwrap(), Some(false));
    assert!(registry::whereis::<ActorRef<Worker>>("fp-register-error").is_none());
    assert!(system.shutdown(LIMIT).await.complete);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replacement_registers_only_after_old_entry_is_removed() {
    let _scenario = FailScenario::setup();
    failpoints::configure(
        "registry.before_unregister",
        Action::Delay(Duration::from_millis(200)),
    )
    .unwrap();
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("fp-unregister", 1).unwrap();
    let started = Started::default();
    let registrations = Arc::new(Mutex::new(Vec::new()));
    let mut tree = system.supervisor(config(5)).unwrap();
    let (s, r) = (started.clone(), registrations.clone());
    let child = tree
        .add(ChildSpec::new("worker", &group, move |_| {
            let (s, r) = (s.clone(), r.clone());
            async move {
                let mut worker = Worker::new(&s);
                worker.name = Some("fp-replaced");
                r.lock().unwrap().push(worker.registration.clone());
                Ok::<_, String>(worker)
            }
        }))
        .unwrap();
    let tree = tree.start().await.unwrap();
    child.wait_ready().await.unwrap().send(Crash).unwrap();
    let current = child.wait_for_generation(1).await.unwrap();
    assert!(failpoints::stats("registry.before_unregister").fired >= 1);
    let flags: Vec<_> = registrations
        .lock()
        .unwrap()
        .iter()
        .map(|r| *r.lock().unwrap())
        .collect();
    assert_eq!(flags, [Some(true), Some(true)]);
    let registered = registry::whereis::<ActorRef<Worker>>("fp-replaced").unwrap();
    assert_eq!(registered.id(), current.id());
    assert!(tree.shutdown(LIMIT).await.complete);
    assert!(system.shutdown(LIMIT).await.complete);
    assert!(registry::whereis::<ActorRef<Worker>>("fp-replaced").is_none());
}

// ---------------------------------------------- link.before_install / monitor

struct LinkTo(ChildHandle);
impl Message for LinkTo {
    type Result = ();
}
struct Watch(ChildHandle);
impl Message for Watch {
    type Result = ();
}
struct Linker {
    downs: tokio::sync::mpsc::UnboundedSender<ExitReason>,
}
impl Actor for Linker {}
impl Handler<LinkTo> for Linker {
    async fn handle(
        &mut self,
        msg: LinkTo,
        reply: spawned_concurrency::reply::PendingReply<'_, ()>,
        ctx: &Context<Self>,
    ) {
        ctx.link(&msg.0);
        reply.reply(());
    }
}
impl Handler<Watch> for Linker {
    async fn handle(
        &mut self,
        msg: Watch,
        reply: spawned_concurrency::reply::PendingReply<'_, ()>,
        ctx: &Context<Self>,
    ) {
        ctx.monitor(&msg.0);
        reply.reply(());
    }
}
impl Handler<Ping> for Linker {
    async fn handle(
        &mut self,
        _: Ping,
        reply: spawned_concurrency::reply::PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
        reply.reply(());
    }
}
impl Handler<Down> for Linker {
    async fn handle(
        &mut self,
        msg: Down,
        reply: spawned_concurrency::reply::PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
        let _ = self.downs.send(msg.reason);
        reply.reply(());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn peer_exit_during_link_install_still_propagates() {
    let _scenario = FailScenario::setup();
    failpoints::configure(
        "link.before_install",
        FailPoint::new(Action::Delay(Duration::from_millis(200))).times(1),
    )
    .unwrap();
    let (downs, _) = tokio::sync::mpsc::unbounded_channel();
    let linker = Linker { downs }.start();
    let target = Worker::new(&Started::default()).start();
    linker.send(LinkTo(target.child_handle())).unwrap();
    reached("link.before_install", 1).await;
    target.send(Crash).unwrap();
    assert!(matches!(target.wait_exit().await, ExitReason::Panic(_)));
    // The link completes after the peer died; the exit must still arrive.
    tokio::time::timeout(LIMIT, linker.join()).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn target_exit_during_monitor_install_delivers_noproc() {
    let _scenario = FailScenario::setup();
    failpoints::configure(
        "monitor.before_install",
        FailPoint::new(Action::Delay(Duration::from_millis(200))).times(1),
    )
    .unwrap();
    let (downs, mut received) = tokio::sync::mpsc::unbounded_channel();
    let watcher = Linker { downs }.start();
    let target = Worker::new(&Started::default()).start();
    watcher.send(Watch(target.child_handle())).unwrap();
    reached("monitor.before_install", 1).await;
    target.send(Crash).unwrap();
    target.wait_exit().await;
    // The registration completes after the target's death point: exactly
    // one Down, carrying NoProc (E4).
    let reason = tokio::time::timeout(LIMIT, received.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reason, ExitReason::NoProc);
    watcher.context().stop();
    watcher.join().await;
}

// ------------------------------------------------------------- actor.* (tasks)

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn injected_started_panic_is_an_abnormal_exit_seen_by_monitors() {
    let _scenario = FailScenario::setup();
    let (downs, mut received) = tokio::sync::mpsc::unbounded_channel();
    let watcher = Linker { downs }.start();
    // The watcher must be past started() before the site is armed.
    watcher.request(Ping).await.unwrap();
    failpoints::configure(
        "actor.before_started",
        FailPoint::new(Action::Panic).times(1),
    )
    .unwrap();
    let started = Started::default();
    let target = Worker::new(&started).start();
    watcher.send(Watch(target.child_handle())).unwrap();
    match target.wait_exit().await {
        ExitReason::Panic(message) => assert!(message.contains("actor.before_started")),
        other => panic!("expected panic exit, got {other:?}"),
    }
    assert!(started.handles().is_empty());
    // The monitor races the target's death point: before it, the Down
    // carries the panic; after it, NoProc.
    let reason = tokio::time::timeout(LIMIT, received.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(reason, ExitReason::Panic(_) | ExitReason::NoProc),
        "got {reason:?}"
    );
    watcher.context().stop();
    watcher.join().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn readiness_racing_shutdown_is_not_published() {
    let _scenario = FailScenario::setup();
    failpoints::configure(
        "actor.after_started_before_ready",
        FailPoint::new(Action::Delay(Duration::from_millis(300))).times(1),
    )
    .unwrap();
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("fp-ready", 1).unwrap();
    let actor = group.start(Worker::new(&Started::default())).unwrap();
    reached("actor.after_started_before_ready", 1).await;
    assert!(system.shutdown(LIMIT).await.complete);
    assert!(actor.ready().await.is_err());
    assert!(actor.exit_reason().is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slow_stopped_reports_at_deadline_then_completes() {
    let _scenario = FailScenario::setup();
    failpoints::configure(
        "actor.before_stopped",
        FailPoint::new(Action::Delay(Duration::from_millis(400))).times(1),
    )
    .unwrap();
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("fp-stopped", 1).unwrap();
    let mut worker = Worker::new(&Started::default());
    worker.trap = true;
    let actor = group.start(worker).unwrap();
    actor.ready().await.unwrap();
    let report = system.shutdown(Duration::from_millis(100)).await;
    assert!(!report.complete);
    assert_eq!(report.unfinished_actors, [actor.id()]);
    let report = report.wait(LIMIT).await;
    assert!(report.complete);
    // The kill at grace arrived while stopped() ran: it finishes, but the
    // reason becomes Killed (E1).
    assert_eq!(actor.exit_reason(), Some(ExitReason::Killed));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn injected_stopped_panic_restarts_transient_child() {
    let _scenario = FailScenario::setup();
    failpoints::configure(
        "actor.before_stopped",
        FailPoint::new(Action::Panic).times(1),
    )
    .unwrap();
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("fp-stopped-panic", 1).unwrap();
    let started = Started::default();
    let mut tree = system.supervisor(config(5)).unwrap();
    let s = started.clone();
    let child = tree
        .add(
            ChildSpec::new("worker", &group, move |_| {
                let s = s.clone();
                async move { Ok::<_, String>(Worker::new(&s)) }
            })
            .restart(spawned_concurrency::supervision::RestartMode::Transient),
        )
        .unwrap();
    let tree = tree.start().await.unwrap();
    // A normal stop whose stopped() panics is abnormal, so Transient restarts.
    child.wait_ready().await.unwrap().context().stop();
    child.wait_for_generation(1).await.unwrap();
    assert!(matches!(
        child.snapshot().last_exit,
        Some(ExitReason::Panic(_))
    ));
    assert!(tree.shutdown(LIMIT).await.complete);
    assert!(system.shutdown(LIMIT).await.complete);
}

// ------------------------------------------------------ coordinator panics

// `run_supervisor` and the ActorSystem shutdown coordinator are panic-guarded:
// a panic there (only reachable through a framework bug) is reported and
// followed by an emergency stop-and-join, so shutdown still completes. These
// were ignored known-gap reproductions before v0.7.1.

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supervisor_coordinator_panic_is_reported_and_contained() {
    let _scenario = FailScenario::setup();
    failpoints::configure(
        "shutdown.before_stop_child",
        FailPoint::new(Action::Panic).times(1),
    )
    .unwrap();
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("fp-gap-supervisor", 1).unwrap();
    let mut tree = system.supervisor(config(5)).unwrap();
    let started = Started::default();
    let child = tree
        .add(ChildSpec::new("worker", &group, move |_| {
            let s = started.clone();
            async move { Ok::<_, String>(Worker::new(&s)) }
        }))
        .unwrap();
    let tree = tree.start().await.unwrap();
    child.wait_ready().await.unwrap();
    let report = tree.shutdown(LIMIT).await;
    let failure = report.failure.as_ref().expect("no failure reported");
    assert!(failure.contains("coordinator panicked"), "{failure}");
    assert_ne!(child.snapshot().state, ChildState::Running);
    assert!(system.shutdown(LIMIT).await.complete);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn system_shutdown_coordinator_panic_is_reported_and_contained() {
    let _scenario = FailScenario::setup();
    failpoints::configure(
        "system.shutdown_before_target",
        FailPoint::new(Action::Panic).times(1),
    )
    .unwrap();
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("fp-gap-system", 1).unwrap();
    let actor = group.start(Worker::new(&Started::default())).unwrap();
    actor.ready().await.unwrap();
    let report = system.shutdown(LIMIT).await;
    assert!(report.complete);
    assert!(
        report
            .coordinator_failures
            .iter()
            .any(|f| f.contains("system shutdown coordinator panicked")),
        "{:?}",
        report.coordinator_failures
    );
}

// ------------------------------------------------------------ actor.* (threads)

// ------------------------------------------------- actor.at_death (D fence)

/// Holds deferred tokens for an outside replier.
#[derive(Default)]
struct Holder;
struct Hold(std::sync::mpsc::Sender<spawned_concurrency::ReplyTo<u32>>);
impl Message for Hold {
    type Result = u32;
}
impl Actor for Holder {}
impl Handler<Hold> for Holder {
    async fn handle(
        &mut self,
        msg: Hold,
        reply: spawned_concurrency::PendingReply<'_, u32>,
        _: &Context<Self>,
    ) {
        msg.0.send(reply.defer().unwrap()).unwrap();
    }
}
impl Handler<Ping> for Holder {
    async fn handle(
        &mut self,
        _: Ping,
        _: spawned_concurrency::PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
    }
}

/// Review P1 (Codex 4): once `NoProc` is observable (a send to the dead
/// owner fails), an external `ReplyTo::reply` must not succeed: the caller
/// gets the owner's final reason. Replies were admitted until the registry
/// closed the slot, after D: a reply could land after `NoProc`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_reply_succeeds_after_noproc_is_observable() {
    let _scenario = FailScenario::setup();
    let holder = Holder.start();
    let (tokens, token) = std::sync::mpsc::channel();
    let caller = holder
        .send_request(Hold(tokens), spawned_concurrency::DEFAULT_REQUEST_TIMEOUT)
        .unwrap();
    let token = token.recv_timeout(LIMIT).unwrap();
    failpoints::configure(
        "actor.at_death",
        FailPoint::new(Action::Delay(Duration::from_millis(300))).times(1),
    )
    .unwrap();
    holder.context().stop();
    let began = Instant::now();
    while holder.send(Ping).is_ok() {
        assert!(began.elapsed() < LIMIT, "the owner never died");
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    // `NoProc` is observable: the death point passed.
    let delivery = token.reply(7);
    assert_ne!(
        delivery,
        spawned_concurrency::Delivery::Delivered,
        "a reply succeeded after NoProc was observable"
    );
    assert!(
        matches!(
            caller.await,
            Err(spawned_concurrency::ActorError::ActorStopped(
                ExitReason::Normal
            ))
        ),
        "the caller did not get the final reason"
    );
    holder.join().await;
}

// ---------------------------------------- actor.exit_before_deliver (fence)

/// Traps exits and records them in arrival order.
struct Recorder(Arc<Mutex<Vec<(spawned_concurrency::ActorId, ExitReason)>>>);
impl Actor for Recorder {
    async fn started(&mut self, ctx: &Context<Self>) {
        ctx.trap_exit(true);
    }
    async fn exit_received(&mut self, exit: spawned_concurrency::Exit, _: &Context<Self>) {
        self.0.lock().unwrap().push((exit.from, exit.reason));
    }
}
impl Handler<Ping> for Recorder {
    async fn handle(
        &mut self,
        _: Ping,
        reply: spawned_concurrency::PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
        reply.reply(());
    }
}
struct SignalSender;
impl Actor for SignalSender {}
impl Handler<LinkTo> for SignalSender {
    async fn handle(
        &mut self,
        msg: LinkTo,
        reply: spawned_concurrency::PendingReply<'_, ()>,
        ctx: &Context<Self>,
    ) {
        ctx.link(&msg.0);
        reply.reply(());
    }
}

/// Review P1 (Codex 10): a signal a cloned context sends before its
/// sender's death reaches the target before the sender's death notice
/// (`ExitAfterMessages`, OTP's per-pair signal order). A helper passed the
/// sender-lifetime check, paused, and delivered after the death notice.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_exit_signal_is_not_overtaken_by_its_senders_death_notice() {
    let _scenario = FailScenario::setup();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorder = Recorder(seen.clone()).start();
    recorder.request(Ping).await.unwrap(); // trapping now
    let sender = SignalSender.start();
    sender
        .request(LinkTo(recorder.child_handle()))
        .await
        .unwrap();
    failpoints::configure(
        "actor.exit_before_deliver",
        FailPoint::new(Action::Delay(Duration::from_millis(300))).times(1),
    )
    .unwrap();
    let ctx = sender.context();
    let target = recorder.child_handle();
    let helper = std::thread::spawn(move || ctx.exit(&target, ExitReason::Shutdown));
    reached("actor.exit_before_deliver", 1).await;
    // The sender dies while its helper's signal is in flight.
    sender.context().stop();
    let began = Instant::now();
    while seen.lock().unwrap().len() < 2 {
        assert!(
            began.elapsed() < LIMIT,
            "exits seen: {:?}",
            seen.lock().unwrap()
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    helper.join().unwrap();
    let reasons: Vec<_> = seen
        .lock()
        .unwrap()
        .iter()
        .map(|(_, r)| r.clone())
        .collect();
    assert_eq!(
        reasons,
        [ExitReason::Shutdown, ExitReason::Normal],
        "the death notice overtook the sender's own signal"
    );
    recorder.context().stop();
    recorder.join().await;
}

// ---------------------------------- monitor.down_before_dispatch (flush)

struct WatchDown(ChildHandle);
impl Message for WatchDown {
    type Result = spawned_concurrency::MonitorRef;
}

/// Monitors on request and counts the `Down`s its handler runs.
struct Observer(Arc<AtomicUsize>);
impl Actor for Observer {}
impl Handler<WatchDown> for Observer {
    async fn handle(
        &mut self,
        msg: WatchDown,
        reply: spawned_concurrency::PendingReply<'_, spawned_concurrency::MonitorRef>,
        ctx: &Context<Self>,
    ) {
        reply.reply(ctx.monitor(&msg.0));
    }
}
impl Handler<Down> for Observer {
    async fn handle(
        &mut self,
        _: Down,
        _: spawned_concurrency::PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
impl Handler<Ping> for Observer {
    async fn handle(
        &mut self,
        _: Ping,
        reply: spawned_concurrency::PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
        reply.reply(());
    }
}
struct Quiet;
impl Actor for Quiet {}

/// OTP `demonitor(Ref, [flush])`: a `demonitor` on another thread returns
/// while the observer is dispatching the monitor's `Down` (paused before the
/// flush check); the `Down` never reaches `Handler<Down>`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn demonitor_flushes_a_down_in_dispatch() {
    let _scenario = FailScenario::setup();
    let handled = Arc::new(AtomicUsize::new(0));
    let observer = Observer(handled.clone()).start();
    let target = Quiet.start();
    let monitor = observer
        .request(WatchDown(target.child_handle()))
        .await
        .unwrap();
    failpoints::configure(
        "monitor.down_before_dispatch",
        FailPoint::new(Action::Delay(Duration::from_millis(300))).times(1),
    )
    .unwrap();
    target.context().stop();
    target.join().await;
    reached("monitor.down_before_dispatch", 1).await;
    observer.context().demonitor(monitor);
    // FIFO, one turn at a time: the Down's dispatch finished before this.
    observer.request(Ping).await.unwrap();
    assert_eq!(
        handled.load(Ordering::SeqCst),
        0,
        "a Down was handled after demonitor returned"
    );
    observer.context().stop();
    observer.join().await;
}

mod native {
    use super::*;
    use spawned_concurrency::threads::{
        Actor as NativeActor, ActorStart as _, Context as NativeContext, Handler as NativeHandler,
    };

    struct Target;
    impl NativeActor for Target {}
    impl NativeHandler<Crash> for Target {
        fn handle(
            &mut self,
            _: Crash,
            _reply: spawned_concurrency::reply::PendingReply<'_, ()>,
            _: &NativeContext<Self>,
        ) {
            panic!("injected native failure");
        }
    }
    /// Holds deferred tokens for an outside replier (threads mode).
    struct NativeHolder;
    impl NativeActor for NativeHolder {}
    impl NativeHandler<Hold> for NativeHolder {
        fn handle(
            &mut self,
            msg: Hold,
            reply: spawned_concurrency::reply::PendingReply<'_, u32>,
            _: &NativeContext<Self>,
        ) {
            msg.0.send(reply.defer().unwrap()).unwrap();
        }
    }
    impl NativeHandler<Ping> for NativeHolder {
        fn handle(
            &mut self,
            _: Ping,
            _: spawned_concurrency::reply::PendingReply<'_, ()>,
            _: &NativeContext<Self>,
        ) {
        }
    }

    /// Review P1 (Codex 4), threads mode: no external reply succeeds once
    /// `NoProc` is observable.
    #[test]
    fn no_reply_succeeds_after_noproc_is_observable() {
        let _scenario = FailScenario::setup();
        let holder = NativeHolder.start();
        let (tokens, token) = std::sync::mpsc::channel();
        let caller = holder
            .send_request(Hold(tokens), spawned_concurrency::DEFAULT_REQUEST_TIMEOUT)
            .unwrap();
        let token = token.recv_timeout(LIMIT).unwrap();
        failpoints::configure(
            "actor.at_death",
            FailPoint::new(Action::Delay(Duration::from_millis(300))).times(1),
        )
        .unwrap();
        holder.context().stop();
        let began = Instant::now();
        while holder.send(Ping).is_ok() {
            assert!(began.elapsed() < LIMIT, "the owner never died");
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_ne!(
            token.reply(7),
            spawned_concurrency::Delivery::Delivered,
            "a reply succeeded after NoProc was observable"
        );
        assert!(
            matches!(
                caller.recv(),
                Err(spawned_concurrency::ActorError::ActorStopped(
                    ExitReason::Normal
                ))
            ),
            "the caller did not get the final reason"
        );
        holder.join();
    }

    /// Traps exits and records them (threads mode).
    struct NativeRecorder(Arc<Mutex<Vec<ExitReason>>>);
    impl NativeActor for NativeRecorder {
        fn started(&mut self, ctx: &NativeContext<Self>) {
            ctx.trap_exit(true);
        }
        fn exit_received(&mut self, exit: spawned_concurrency::Exit, _: &NativeContext<Self>) {
            self.0.lock().unwrap().push(exit.reason);
        }
    }
    struct NativeSender;
    impl NativeActor for NativeSender {}
    impl NativeHandler<LinkTo> for NativeSender {
        fn handle(
            &mut self,
            msg: LinkTo,
            reply: spawned_concurrency::reply::PendingReply<'_, ()>,
            ctx: &NativeContext<Self>,
        ) {
            ctx.link(&msg.0);
            reply.reply(());
        }
    }

    /// Review P1 (Codex 10), threads mode: a cloned context's signal sent
    /// before its actor's death reaches the target before the death notice.
    #[test]
    fn an_exit_signal_is_not_overtaken_by_its_senders_death_notice() {
        let _scenario = FailScenario::setup();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorder = NativeRecorder(seen.clone()).start();
        let sender = NativeSender.start();
        sender.request(LinkTo(recorder.child_handle())).unwrap();
        failpoints::configure(
            "actor.exit_before_deliver",
            FailPoint::new(Action::Delay(Duration::from_millis(300))).times(1),
        )
        .unwrap();
        let ctx = sender.context();
        let target = recorder.child_handle();
        let helper = std::thread::spawn(move || ctx.exit(&target, ExitReason::Shutdown));
        let began = Instant::now();
        while failpoints::stats("actor.exit_before_deliver").hits < 1 {
            assert!(began.elapsed() < LIMIT);
            std::thread::sleep(Duration::from_millis(1));
        }
        sender.context().stop();
        while seen.lock().unwrap().len() < 2 {
            assert!(
                began.elapsed() < LIMIT,
                "exits seen: {:?}",
                seen.lock().unwrap()
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        helper.join().unwrap();
        assert_eq!(
            *seen.lock().unwrap(),
            [ExitReason::Shutdown, ExitReason::Normal],
            "the death notice overtook the sender's own signal"
        );
        recorder.context().stop();
        recorder.join();
    }

    struct Peer;
    impl NativeActor for Peer {}
    impl NativeHandler<LinkTo> for Peer {
        fn handle(
            &mut self,
            msg: LinkTo,
            reply: spawned_concurrency::reply::PendingReply<'_, ()>,
            ctx: &NativeContext<Self>,
        ) {
            ctx.link(&msg.0);
            reply.reply(());
        }
    }

    #[test]
    fn injected_started_panic_is_an_abnormal_exit() {
        let _scenario = FailScenario::setup();
        failpoints::configure(
            "actor.before_started",
            FailPoint::new(Action::Panic).times(1),
        )
        .unwrap();
        let actor = Target.start();
        match actor.wait_exit() {
            ExitReason::Panic(message) => assert!(message.contains("actor.before_started")),
            other => panic!("expected panic exit, got {other:?}"),
        }
    }

    #[test]
    fn slow_stopped_still_completes_join() {
        let _scenario = FailScenario::setup();
        failpoints::configure(
            "actor.before_stopped",
            FailPoint::new(Action::Delay(Duration::from_millis(100))).times(1),
        )
        .unwrap();
        let actor = Target.start();
        let began = Instant::now();
        actor.context().stop();
        actor.join();
        assert!(began.elapsed() >= Duration::from_millis(100));
        assert_eq!(actor.exit_reason(), Some(ExitReason::Normal));
    }

    #[test]
    fn peer_exit_during_link_install_still_propagates() {
        let _scenario = FailScenario::setup();
        failpoints::configure(
            "link.before_install",
            FailPoint::new(Action::Delay(Duration::from_millis(200))).times(1),
        )
        .unwrap();
        let peer = Peer.start();
        let target = Target.start();
        peer.send(LinkTo(target.child_handle())).unwrap();
        let began = Instant::now();
        while failpoints::stats("link.before_install").hits < 1 {
            assert!(began.elapsed() < LIMIT);
            std::thread::sleep(Duration::from_millis(2));
        }
        target.send(Crash).unwrap();
        assert!(matches!(target.wait_exit(), ExitReason::Panic(_)));
        let joined = std::thread::spawn(move || peer.join());
        let began = Instant::now();
        while !joined.is_finished() {
            assert!(began.elapsed() < LIMIT, "linked peer did not stop");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    struct NativeObserver(Arc<AtomicUsize>);
    impl NativeActor for NativeObserver {}
    impl NativeHandler<WatchDown> for NativeObserver {
        fn handle(
            &mut self,
            msg: WatchDown,
            reply: spawned_concurrency::reply::PendingReply<'_, spawned_concurrency::MonitorRef>,
            ctx: &NativeContext<Self>,
        ) {
            reply.reply(ctx.monitor(&msg.0));
        }
    }
    impl NativeHandler<Down> for NativeObserver {
        fn handle(
            &mut self,
            _: Down,
            _: spawned_concurrency::reply::PendingReply<'_, ()>,
            _: &NativeContext<Self>,
        ) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    impl NativeHandler<Ping> for NativeObserver {
        fn handle(
            &mut self,
            _: Ping,
            reply: spawned_concurrency::reply::PendingReply<'_, ()>,
            _: &NativeContext<Self>,
        ) {
            reply.reply(());
        }
    }
    struct NativeQuiet;
    impl NativeActor for NativeQuiet {}

    /// Threads mode: a `demonitor` on another thread returns while the
    /// observer dispatches the monitor's `Down`; it is never handled.
    #[test]
    fn demonitor_flushes_a_down_in_dispatch() {
        let _scenario = FailScenario::setup();
        let handled = Arc::new(AtomicUsize::new(0));
        let observer = NativeObserver(handled.clone()).start();
        let target = NativeQuiet.start();
        let monitor = observer.request(WatchDown(target.child_handle())).unwrap();
        failpoints::configure(
            "monitor.down_before_dispatch",
            FailPoint::new(Action::Delay(Duration::from_millis(300))).times(1),
        )
        .unwrap();
        target.context().stop();
        target.join();
        let began = Instant::now();
        while failpoints::stats("monitor.down_before_dispatch").hits < 1 {
            assert!(began.elapsed() < LIMIT, "the Down was not dispatched");
            std::thread::sleep(Duration::from_millis(2));
        }
        observer.context().demonitor(monitor);
        observer.request(Ping).unwrap();
        assert_eq!(
            handled.load(Ordering::SeqCst),
            0,
            "a Down was handled after demonitor returned"
        );
        observer.context().stop();
        observer.join();
    }
}
