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
    name: Option<&'static str>,
    registration: Arc<Mutex<Option<bool>>>,
}
impl Worker {
    fn new(started: &Started) -> Self {
        Self {
            started: started.clone(),
            name: None,
            registration: Default::default(),
        }
    }
}
impl Actor for Worker {
    async fn started(&mut self, ctx: &Context<Self>) {
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
    async fn handle(&mut self, _: Crash, reply: spawned_concurrency::reply::PendingReply<'_, ()>, _: &Context<Self>) {
        {
        panic!("injected child failure");
    }
        let _ = reply.reply(());
    }
}
impl Handler<Ping> for Worker {
    async fn handle(&mut self, _: Ping, reply: spawned_concurrency::reply::PendingReply<'_, ()>, _: &Context<Self>) {
        let _ = reply.reply(());
    }
}

// ---------------------------------------------------------------- child.factory

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn factory_errors_restart_within_budget_and_count_generations() {
    let _scenario = FailScenario::setup();
    failpoints::configure("child.factory", FailPoint::new(Action::Error).times(3)).unwrap();
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
    let current = child.wait_ready().await.unwrap();
    // Three injected failures, each its own generation, then one success.
    assert_eq!(failpoints::stats("child.factory").fired, 3);
    assert_eq!(child.snapshot().generation, 4);
    assert_eq!(constructed.load(Ordering::SeqCst), 1);
    assert_eq!(started.handles().len(), 1);
    assert_eq!(started.handles()[0].id(), current.id());
    assert!(tree.failure().is_none());
    let report = tree.shutdown(LIMIT).await;
    assert!(report.complete && report.failure.is_none());
    assert!(system.shutdown(LIMIT).await.complete);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persistent_factory_failure_exhausts_budget_and_fails_startup() {
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
        Err(SupervisorError::Failed(error)) => {
            assert!(error.contains("restart budget exhausted"), "{error}")
        }
        other => panic!("expected budget failure, got {:?}", other.err()),
    }
    // One initial attempt plus max_restarts retries.
    assert_eq!(failpoints::stats("child.factory").fired, 3);
    assert!(started.handles().is_empty());
    assert!(child.current().is_err());
    assert!(system.shutdown(LIMIT).await.complete);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn factory_panic_is_a_failed_start_not_a_controller_failure() {
    let _scenario = FailScenario::setup();
    failpoints::configure("child.factory", FailPoint::new(Action::Panic).times(1)).unwrap();
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
    child.wait_ready().await.unwrap();
    let snapshot = child.snapshot();
    assert_eq!(snapshot.generation, 2);
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
    failpoints::configure(
        "child.factory",
        FailPoint::new(Action::Error).probability(0.5),
    )
    .unwrap();
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
    failpoints::configure(
        "child.after_ready_before_publish",
        FailPoint::new(Action::Error).times(1),
    )
    .unwrap();
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
    let current = child.wait_ready().await.unwrap();
    let handles = started.handles();
    assert_eq!(handles.len(), 2);
    assert_ne!(handles[0].id(), current.id());
    assert_eq!(handles[1].id(), current.id());
    // The discarded generation was stopped, not leaked.
    let reason = tokio::time::timeout(LIMIT, handles[0].wait_exit_async())
        .await
        .unwrap();
    assert_eq!(reason, ExitReason::Shutdown);
    assert_eq!(child.snapshot().generation, 2);
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
        Err(SupervisorError::Failed(error)) => {
            assert!(error.contains("placement failed"), "{error}")
        }
        other => panic!("expected placement failure, got {:?}", other.err()),
    }
    // start() returns once the supervisor has failed; the child's controller
    // publishes its terminal state afterwards. Never started: `Failed` is
    // sticky, so the outcome does not depend on whether the unstarted-child
    // sweep or the controller finishes last.
    let began = Instant::now();
    while child.snapshot().state == ChildState::Waiting {
        assert!(began.elapsed() < LIMIT, "child never left Waiting");
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
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
    async fn handle(&mut self, msg: LinkTo, reply: spawned_concurrency::reply::PendingReply<'_, ()>, ctx: &Context<Self>) {
        {
        ctx.link(&msg.0);
    }
        let _ = reply.reply(());
    }
}
impl Handler<Watch> for Linker {
    async fn handle(&mut self, msg: Watch, reply: spawned_concurrency::reply::PendingReply<'_, ()>, ctx: &Context<Self>) {
        {
        ctx.monitor(&msg.0);
    }
        let _ = reply.reply(());
    }
}
impl Handler<Ping> for Linker {
    async fn handle(&mut self, _: Ping, reply: spawned_concurrency::reply::PendingReply<'_, ()>, _: &Context<Self>) {
        let _ = reply.reply(());
    }
}
impl Handler<Down> for Linker {
    async fn handle(&mut self, msg: Down, reply: spawned_concurrency::reply::PendingReply<'_, ()>, _: &Context<Self>) {
        {
        let _ = self.downs.send(msg.reason);
    }
        let _ = reply.reply(());
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
async fn target_exit_during_monitor_install_still_delivers_down() {
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
    let reason = tokio::time::timeout(LIMIT, received.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(reason, ExitReason::Panic(_)));
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
    let reason = tokio::time::timeout(LIMIT, received.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(reason, ExitReason::Panic(_)));
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
    let actor = group.start(Worker::new(&Started::default())).unwrap();
    actor.ready().await.unwrap();
    let report = system.shutdown(Duration::from_millis(100)).await;
    assert!(!report.complete);
    assert_eq!(report.unfinished_actors, [actor.id()]);
    let report = report.wait(LIMIT).await;
    assert!(report.complete);
    assert_eq!(actor.exit_reason(), Some(ExitReason::Shutdown));
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

mod native {
    use super::*;
    use spawned_concurrency::threads::{
        Actor as NativeActor, ActorStart as _, Context as NativeContext, Handler as NativeHandler,
    };

    struct Target;
    impl NativeActor for Target {}
    impl NativeHandler<Crash> for Target {
        fn handle(&mut self, _: Crash, _: &NativeContext<Self>) {
            panic!("injected native failure");
        }
    }
    struct Peer;
    impl NativeActor for Peer {}
    impl NativeHandler<LinkTo> for Peer {
        fn handle(&mut self, msg: LinkTo, ctx: &NativeContext<Self>) {
            ctx.link(&msg.0);
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
}
