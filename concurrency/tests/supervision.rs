#![cfg(feature = "async")]
// Tests that need OS threads are compiled out under spawned_sim.
#![cfg_attr(spawned_sim, allow(unused_imports, dead_code))]
use spawned_concurrency::{
    message::Message,
    supervision::{Backoff, ChildSpec, RestartMode, SupervisorConfig},
    system::ActorSystem,
    tasks::{Actor, Context, Handler},
    ExitReason,
};
use std::{
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
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
struct Next;
impl Message for Next {
    type Result = usize;
}
struct Crash;
impl Message for Crash {
    type Result = ();
}
struct Counter(usize);
impl Actor for Counter {}
impl Handler<Next> for Counter {
    async fn handle(
        &mut self,
        _: Next,
        reply: spawned_concurrency::reply::PendingReply<'_, usize>,
        _: &Context<Self>,
    ) {
        let __reply_value: usize = {
            self.0 += 1;
            self.0
        };
        reply.reply(__reply_value);
    }
}
impl Handler<Crash> for Counter {
    async fn handle(
        &mut self,
        _: Crash,
        _reply: spawned_concurrency::reply::PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
        {
            panic!("injected child failure");
        }
    }
}

#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn one_for_one_uses_fresh_state_same_group_and_instance_bound_refs() {
    let system = ActorSystem::new().unwrap();
    let critical = system.async_group("recovery-critical", 1).unwrap();
    let general = system.async_group("recovery-general", 1).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let mut tree = system.supervisor(config(10)).unwrap();
    let child = tree
        .add(
            ChildSpec::new("counter", &critical, move |ctx| {
                let observed = observed.clone();
                async move {
                    assert_eq!(
                        ctx.execution_group().name().as_deref(),
                        Some("recovery-critical")
                    );
                    // One simulated thread under spawned_sim: no group workers.
                    #[cfg(not(spawned_sim))]
                    assert!(std::thread::current()
                        .name()
                        .unwrap()
                        .starts_with("spawned-recovery-critical"));
                    observed.fetch_add(1, Ordering::SeqCst);
                    Ok::<_, String>(Counter(0))
                }
            })
            .restart(RestartMode::Transient),
        )
        .unwrap();
    let sibling = tree
        .add(ChildSpec::new("sibling", &general, |_| async {
            Ok::<_, String>(Counter(0))
        }))
        .unwrap();
    let supervisor = tree.start().await.unwrap();
    let old = child.current().unwrap();
    let sibling_id = sibling.current().unwrap().id();
    assert_eq!(old.request(Next).await.unwrap(), 1);
    assert!(old.request(Crash).await.is_err());
    let new = spawned_rt::tasks::timeout(LIMIT, child.wait_for_generation(1))
        .await
        .unwrap()
        .unwrap();
    assert_ne!(new.id(), old.id());
    assert!(old.request(Next).await.is_err());
    assert_eq!(new.request(Next).await.unwrap(), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(sibling.current().unwrap().id(), sibling_id);
    assert!(supervisor.shutdown(LIMIT).await.complete);
    assert_eq!(
        new.child_handle().wait_exit_async().await,
        ExitReason::Shutdown
    );
    assert!(system.shutdown(LIMIT).await.complete);
}

#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_modes_distinguish_normal_shutdown_and_panic() {
    for mode in [
        RestartMode::Permanent,
        RestartMode::Transient,
        RestartMode::Temporary,
    ] {
        for abnormal in [false, true] {
            let system = ActorSystem::new().unwrap();
            let group = system.async_group("mode", 1).unwrap();
            let mut tree = system.supervisor(config(5)).unwrap();
            let child = tree
                .add(
                    ChildSpec::new("child", &group, |_| async { Ok::<_, String>(Counter(0)) })
                        .restart(mode),
                )
                .unwrap();
            let supervisor = tree.start().await.unwrap();
            let old = child.current().unwrap();
            if abnormal {
                assert!(old.request(Crash).await.is_err());
            } else {
                old.child_handle().shutdown();
            }
            let next = spawned_rt::tasks::timeout(LIMIT, child.wait_for_generation(1))
                .await
                .unwrap();
            assert_eq!(
                next.is_ok(),
                mode == RestartMode::Permanent || (mode == RestartMode::Transient && abnormal)
            );
            assert!(supervisor.shutdown(LIMIT).await.complete);
            assert!(system.shutdown(LIMIT).await.complete);
        }
    }
}

#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_budget_is_shared_between_siblings_and_exhaustion_closes_tree() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("budget", 2).unwrap();
    let mut tree = system.supervisor(config(2)).unwrap();
    let a = tree
        .add(ChildSpec::new("a", &group, |_| async {
            Ok::<_, String>(Counter(0))
        }))
        .unwrap();
    let b = tree
        .add(ChildSpec::new("b", &group, |_| async {
            Ok::<_, String>(Counter(0))
        }))
        .unwrap();
    let supervisor = tree.start().await.unwrap();
    assert!(a.current().unwrap().request(Crash).await.is_err());
    let a2 = spawned_rt::tasks::timeout(LIMIT, a.wait_for_generation(1))
        .await
        .unwrap()
        .unwrap();
    assert!(b.current().unwrap().request(Crash).await.is_err());
    let b2 = spawned_rt::tasks::timeout(LIMIT, b.wait_for_generation(1))
        .await
        .unwrap()
        .unwrap();
    assert!(a2.request(Crash).await.is_err());
    spawned_rt::tasks::timeout(LIMIT, supervisor.join())
        .await
        .unwrap();
    assert!(supervisor.failure().unwrap().contains("budget exhausted"));
    assert_eq!(a.snapshot().generation, 2);
    assert_eq!(
        b2.child_handle().wait_exit_async().await,
        ExitReason::Shutdown
    );
    assert!(system.shutdown(LIMIT).await.complete);
}

struct DropFlag(Arc<AtomicBool>);
impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_factories_cancel_owned_helpers_before_retry_and_catch_panics() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("factories", 1).unwrap();
    let attempts = Arc::new(AtomicUsize::new(0));
    let live = Arc::new(AtomicBool::new(false));
    let mut tree = system.supervisor(config(4)).unwrap();
    let calls = attempts.clone();
    let busy = live.clone();
    let child = tree
        .add(ChildSpec::new("factory", &group, move |ctx| {
            let attempt = calls.fetch_add(1, Ordering::SeqCst);
            let busy = busy.clone();
            async move {
                assert!(
                    !busy.swap(true, Ordering::SeqCst),
                    "old construction helper overlaps next generation"
                );
                let probe = DropFlag(busy);
                let helper = ctx
                    .spawn_helper(async move {
                        let _probe = probe;
                        std::future::pending::<()>().await;
                    })
                    .unwrap();
                drop(helper);
                match attempt {
                    0 => Err("injected construction error".to_string()),
                    1 => panic!("injected factory panic"),
                    _ => Ok(Counter(0)),
                }
            }
        }))
        .unwrap();
    let supervisor = spawned_rt::tasks::timeout(LIMIT, tree.start())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(attempts.load(Ordering::SeqCst), 3);
    assert_eq!(child.snapshot().generation, 3);
    assert_eq!(child.current().unwrap().request(Next).await.unwrap(), 1);
    assert!(supervisor.shutdown(LIMIT).await.complete);
    assert!(!live.load(Ordering::SeqCst));
    assert!(system.shutdown(LIMIT).await.complete);
}

#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_during_backoff_prevents_new_factory_invocation() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("backoff", 1).unwrap();
    let attempts = Arc::new(AtomicUsize::new(0));
    let calls = attempts.clone();
    let mut policy = config(100);
    policy.backoff.initial = Duration::from_secs(60);
    policy.backoff.maximum = Duration::from_secs(60);
    let mut tree = system.supervisor(policy).unwrap();
    let child = tree
        .add(ChildSpec::new("slow-retry", &group, move |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Ok::<_, String>(Counter(0)) }
        }))
        .unwrap();
    let supervisor = tree.start().await.unwrap();
    assert!(child.current().unwrap().request(Crash).await.is_err());
    assert!(supervisor.shutdown(LIMIT).await.complete);
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert!(system.shutdown(LIMIT).await.complete);
}

struct SlowHelper {
    entered: Option<tokio::sync::oneshot::Sender<()>>,
    release: Option<std::sync::mpsc::Receiver<()>>,
}
impl Actor for SlowHelper {
    async fn started(&mut self, ctx: &Context<Self>) {
        let entered = self.entered.take().unwrap();
        let release = self.release.take().unwrap();
        ctx.spawn_helper(async move {
            let _ = entered.send(());
            let _ = release.recv();
        })
        .unwrap();
    }
}
impl Handler<Crash> for SlowHelper {
    async fn handle(
        &mut self,
        _: Crash,
        _reply: spawned_concurrency::reply::PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
        {
            panic!("injected failure with active helper");
        }
    }
}
// Not simulated: a helper blocks an OS worker on purpose.
#[cfg(not(spawned_sim))]
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn cleanup_deadline_escalates_without_overlapping_a_live_generation() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("stuck", 2).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let (entered_tx, entered) = tokio::sync::oneshot::channel();
    let (release, release_rx) = std::sync::mpsc::channel();
    let args = Arc::new(std::sync::Mutex::new(Some((entered_tx, release_rx))));
    let mut tree = system.supervisor(config(5)).unwrap();
    let child = tree
        .add(
            ChildSpec::new("stuck", &group, move |_| {
                count.fetch_add(1, Ordering::SeqCst);
                let (entered, release) = args
                    .lock()
                    .unwrap()
                    .take()
                    .expect("replacement must never overlap stuck helper");
                async move {
                    Ok::<_, String>(SlowHelper {
                        entered: Some(entered),
                        release: Some(release),
                    })
                }
            })
            .shutdown_timeout(Duration::from_millis(20)),
        )
        .unwrap();
    let supervisor = tree.start().await.unwrap();
    spawned_rt::tasks::timeout(LIMIT, entered)
        .await
        .unwrap()
        .unwrap();
    assert!(child.current().unwrap().request(Crash).await.is_err());
    spawned_rt::tasks::timeout(LIMIT, async {
        while supervisor.failure().is_none() {
            spawned_rt::tasks::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let report = system.shutdown(Duration::from_millis(5)).await;
    assert!(!report.complete);
    release.send(()).unwrap();
    assert!(report.wait(LIMIT).await.complete);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

struct SlowStop {
    entered: Option<tokio::sync::oneshot::Sender<()>>,
    release: Option<tokio::sync::oneshot::Receiver<()>>,
}
impl Actor for SlowStop {
    async fn started(&mut self, ctx: &Context<Self>) {
        // Traps exits so stopped() also runs on its parent's shutdown (E3).
        ctx.trap_exit(true);
    }
    async fn stopped(&mut self, _: &Context<Self>) {
        let _ = self.entered.take().unwrap().send(());
        let _ = self.release.take().unwrap().await;
    }
}
impl Handler<Crash> for SlowStop {
    async fn handle(
        &mut self,
        _: Crash,
        _reply: spawned_concurrency::reply::PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
        {
            panic!("failure before stuck stopped hook");
        }
    }
}
// Not simulated: a helper blocks an OS worker on purpose.
#[cfg(not(spawned_sim))]
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn stuck_stopped_hook_triggers_cleanup_deadline_before_terminal_publication() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("stopped-hook", 1).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let (entered_tx, entered) = tokio::sync::oneshot::channel();
    let (release, release_rx) = tokio::sync::oneshot::channel();
    let args = Arc::new(std::sync::Mutex::new(Some((entered_tx, release_rx))));
    let mut tree = system.supervisor(config(5)).unwrap();
    let child = tree
        .add(
            ChildSpec::new("stuck", &group, move |_| {
                count.fetch_add(1, Ordering::SeqCst);
                let (entered, release) = args
                    .lock()
                    .unwrap()
                    .take()
                    .expect("no overlapping replacement");
                async move {
                    Ok::<_, String>(SlowStop {
                        entered: Some(entered),
                        release: Some(release),
                    })
                }
            })
            .shutdown_timeout(Duration::from_millis(20)),
        )
        .unwrap();
    let supervisor = tree.start().await.unwrap();
    let old = child.current().unwrap();
    let response = old.request_raw(Crash).unwrap();
    spawned_rt::tasks::timeout(LIMIT, entered)
        .await
        .unwrap()
        .unwrap();
    assert!(response.await.is_err());
    assert!(old.exit_reason().is_none(), "stopped hook has not finished");
    spawned_rt::tasks::timeout(LIMIT, async {
        while supervisor.failure().is_none() {
            spawned_rt::tasks::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(child.current().is_err());
    let report = system.shutdown(Duration::from_millis(5)).await;
    assert!(!report.complete);
    assert!(report.unfinished_actors.contains(&old.id()));
    release.send(()).unwrap();
    let report = report.wait(LIMIT).await;
    assert!(report.complete);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    // The system's kill at grace reached the actor inside its stuck
    // stopped() (before its death point): Killed replaces the panic (E1).
    assert_eq!(old.exit_reason(), Some(ExitReason::Killed));
}

struct Ordered {
    name: &'static str,
    log: Arc<std::sync::Mutex<Vec<String>>>,
}
impl Actor for Ordered {
    async fn started(&mut self, ctx: &Context<Self>) {
        // v0.8: only a trapping child runs stopped() on its parent's
        // shutdown (E3, decision 2); the order is observed through it.
        ctx.trap_exit(true);
        self.log
            .lock()
            .unwrap()
            .push(format!("start:{}", self.name));
    }
    async fn stopped(&mut self, _: &Context<Self>) {
        self.log.lock().unwrap().push(format!("stop:{}", self.name));
    }
}
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn initial_startup_and_shutdown_follow_dependency_order_across_groups() {
    let system = ActorSystem::new().unwrap();
    let a = system.async_group("ordered-a", 1).unwrap();
    let b = system.async_group("ordered-b", 1).unwrap();
    let log = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut tree = system.supervisor(config(5)).unwrap();
    for (name, group) in [("a", &a), ("b", &b), ("c", &a)] {
        let log = log.clone();
        tree.add(ChildSpec::new(name, group, move |_| {
            let log = log.clone();
            async move { Ok::<_, String>(Ordered { name, log }) }
        }))
        .unwrap();
    }
    let supervisor = tree.start().await.unwrap();
    let report = system.shutdown(LIMIT).await;
    assert!(report.complete);
    assert!(report.supervisor_failures.is_empty());
    assert!(supervisor.failure().is_none());
    assert_eq!(
        *log.lock().unwrap(),
        ["start:a", "start:b", "start:c", "stop:c", "stop:b", "stop:a"]
    );
}

#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_partial_startup_rolls_back_without_starting_later_children() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("rollback", 1).unwrap();
    let log = Arc::new(std::sync::Mutex::new(Vec::new()));
    let first_log = log.clone();
    let last_log = log.clone();
    let mut tree = system.supervisor(config(0)).unwrap();
    tree.add(ChildSpec::new("a", &group, move |_| {
        let log = first_log.clone();
        async move { Ok::<_, String>(Ordered { name: "a", log }) }
    }))
    .unwrap();
    tree.add(ChildSpec::new("b", &group, |_| async {
        Err::<Counter, _>("cannot construct b")
    }))
    .unwrap();
    let last = tree
        .add(ChildSpec::new("c", &group, move |_| {
            let log = last_log.clone();
            async move { Ok::<_, String>(Ordered { name: "c", log }) }
        }))
        .unwrap();
    assert!(spawned_rt::tasks::timeout(LIMIT, tree.start())
        .await
        .unwrap()
        .is_err());
    assert!(system.shutdown(LIMIT).await.complete);
    assert_eq!(*log.lock().unwrap(), ["start:a", "stop:a"]);
    assert_eq!(last.snapshot().generation, 0);
}

#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_public_startup_future_cancels_construction_and_owned_helpers() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("cancel-factory", 1).unwrap();
    let (entered_tx, entered) = tokio::sync::oneshot::channel();
    let entered_tx = Arc::new(std::sync::Mutex::new(Some(entered_tx)));
    let live = Arc::new(AtomicBool::new(false));
    let live_factory = live.clone();
    let mut tree = system.supervisor(config(5)).unwrap();
    let child = tree
        .add(ChildSpec::new("pending", &group, move |ctx| {
            let entered = entered_tx.lock().unwrap().take();
            let live = live_factory.clone();
            async move {
                live.store(true, Ordering::SeqCst);
                let drop_flag = DropFlag(live);
                ctx.spawn_helper(async move {
                    let _drop_flag = drop_flag;
                    std::future::pending::<()>().await;
                })
                .unwrap();
                let _ = entered.unwrap().send(());
                std::future::pending::<Result<Counter, String>>().await
            }
        }))
        .unwrap();
    let startup = spawned_rt::tasks::spawn(tree.start());
    spawned_rt::tasks::timeout(LIMIT, entered)
        .await
        .unwrap()
        .unwrap();
    startup.abort();
    assert!(startup.await.is_err());
    assert!(system.shutdown(LIMIT).await.complete);
    assert!(!live.load(Ordering::SeqCst));
    assert!(child.current().is_err());
    assert_eq!(child.snapshot().generation, 1);
}

struct RaceActor {
    live: Arc<AtomicUsize>,
    count: usize,
}
impl Drop for RaceActor {
    fn drop(&mut self) {
        assert_eq!(self.live.fetch_sub(1, Ordering::SeqCst), 1);
    }
}
impl Actor for RaceActor {}
impl Handler<Crash> for RaceActor {
    async fn handle(
        &mut self,
        _: Crash,
        _reply: spawned_concurrency::reply::PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
        {
            panic!("racing child failure");
        }
    }
}
impl Handler<Next> for RaceActor {
    async fn handle(
        &mut self,
        _: Next,
        reply: spawned_concurrency::reply::PendingReply<'_, usize>,
        _: &Context<Self>,
    ) {
        let __reply_value: usize = {
            self.count += 1;
            self.count
        };
        reply.reply(__reply_value);
    }
}
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn repeated_crash_producer_and_shutdown_races_never_overlap_generations() {
    for iteration in 0..32 {
        let system = ActorSystem::new().unwrap();
        let group = system.async_group("racing-lifecycle", 2).unwrap();
        let live = Arc::new(AtomicUsize::new(0));
        let factory_live = live.clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let factory_calls = calls.clone();
        let mut tree = system.supervisor(config(4)).unwrap();
        let child = tree
            .add(ChildSpec::new("racy", &group, move |_| {
                assert_eq!(
                    factory_live.fetch_add(1, Ordering::SeqCst),
                    0,
                    "overlapping generations"
                );
                factory_calls.fetch_add(1, Ordering::SeqCst);
                let live = factory_live.clone();
                async move { Ok::<_, String>(RaceActor { live, count: 0 }) }
            }))
            .unwrap();
        let supervisor = tree.start().await.unwrap();
        let old = child.current().unwrap();
        let mut producers = Vec::new();
        for _ in 0..8 {
            let actor = old.clone();
            producers.push(spawned_rt::tasks::spawn(async move {
                for _ in 0..16 {
                    let _ = actor.request(Next).await;
                    spawned_rt::tasks::yield_now().await;
                }
            }));
        }
        let _ = old.send(Crash);
        if iteration % 2 == 0 {
            spawned_rt::tasks::yield_now().await;
        }
        assert!(supervisor.shutdown(LIMIT).await.complete);
        for producer in producers {
            spawned_rt::tasks::timeout(LIMIT, producer)
                .await
                .unwrap()
                .unwrap();
        }
        assert!(system.shutdown(LIMIT).await.complete);
        assert_eq!(live.load(Ordering::SeqCst), 0);
        let final_calls = calls.load(Ordering::SeqCst);
        assert!(final_calls >= 1);
        assert!(child.current().is_err());
        assert!(old.request(Next).await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), final_calls);
    }
}

#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn configuration_rejects_foreign_groups_duplicate_names_and_capacity() {
    use spawned_concurrency::{supervision::SupervisorError, system::SystemError};
    let first = ActorSystem::new().unwrap();
    let second = ActorSystem::new().unwrap();
    let own = first.async_group("own", 1).unwrap();
    let foreign = second.async_group("foreign", 1).unwrap();
    let mut tree = first.supervisor(config(5)).unwrap();
    assert!(matches!(
        tree.add(ChildSpec::new("foreign", &foreign, |_| async {
            Ok::<_, String>(Counter(0))
        })),
        Err(SupervisorError::System(SystemError::ForeignGroup))
    ));
    tree.add(ChildSpec::new("same", &own, |_| async {
        Ok::<_, String>(Counter(0))
    }))
    .unwrap();
    assert!(matches!(
        tree.add(ChildSpec::new("same", &own, |_| async {
            Ok::<_, String>(Counter(0))
        })),
        Err(SupervisorError::DuplicateChild(_))
    ));
    drop(tree);
    let mut limited = config(5);
    limited.max_children = 1;
    let mut tree = first.supervisor(limited).unwrap();
    tree.add(ChildSpec::new("one", &own, |_| async {
        Ok::<_, String>(Counter(0))
    }))
    .unwrap();
    assert!(matches!(
        tree.add(ChildSpec::new("two", &own, |_| async {
            Ok::<_, String>(Counter(0))
        })),
        Err(SupervisorError::Capacity)
    ));
    drop(tree);
    let mut invalid = config(5);
    invalid.restart_window = Duration::ZERO;
    assert!(first.supervisor(invalid).is_err());
    assert!(first.shutdown(LIMIT).await.complete);
    assert!(second.shutdown(LIMIT).await.complete);
}

#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn system_shutdown_orders_standalone_actors_and_supervisor_trees_together() {
    use spawned_concurrency::tasks::ActorStart;
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("mixed-order", 1).unwrap();
    let log = Arc::new(std::sync::Mutex::new(Vec::new()));
    let a = Ordered {
        name: "a",
        log: log.clone(),
    }
    .start_on(&group)
    .unwrap();
    a.ready().await.unwrap();
    let mut trees = Vec::new();
    for name in ["b", "d"] {
        let mut tree = system.supervisor(config(5)).unwrap();
        let events = log.clone();
        tree.add(ChildSpec::new(name, &group, move |_| {
            let log = events.clone();
            async move { Ok::<_, String>(Ordered { name, log }) }
        }))
        .unwrap();
        trees.push(tree.start().await.unwrap());
        if name == "b" {
            Ordered {
                name: "c",
                log: log.clone(),
            }
            .start_on(&group)
            .unwrap()
            .ready()
            .await
            .unwrap();
        }
    }
    assert!(system.shutdown(LIMIT).await.complete);
    assert_eq!(
        *log.lock().unwrap(),
        ["start:a", "start:b", "start:c", "start:d", "stop:d", "stop:c", "stop:b", "stop:a"]
    );
    assert!(trees.iter().all(|s| s.failure().is_none()));
}

#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn generation_waiters_wake_at_admission_close_before_their_shutdown_turn() {
    use spawned_concurrency::supervision::{ChildState, ChildUnavailable};
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("lookup-close", 1).unwrap();
    let (entered_tx, entered) = tokio::sync::oneshot::channel();
    let (release, release_rx) = tokio::sync::oneshot::channel();
    let args = Arc::new(std::sync::Mutex::new(Some((entered_tx, release_rx))));
    let mut tree = system.supervisor(config(5)).unwrap();
    let first = tree
        .add(ChildSpec::new("first", &group, |_| async {
            Ok::<_, String>(Counter(0))
        }))
        .unwrap();
    tree.add(ChildSpec::new("last", &group, move |_| {
        let (entered, release) = args.lock().unwrap().take().unwrap();
        async move {
            Ok::<_, String>(SlowStop {
                entered: Some(entered),
                release: Some(release),
            })
        }
    }))
    .unwrap();
    let supervisor = tree.start().await.unwrap();
    let mut waiter = Box::pin(first.wait_for_generation(99));
    assert!(futures::poll!(waiter.as_mut()).is_pending());
    supervisor.stop();
    spawned_rt::tasks::timeout(LIMIT, entered)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        spawned_rt::tasks::timeout(LIMIT, waiter).await.unwrap(),
        Err(ChildUnavailable(ChildState::Stopping))
    ));
    release.send(()).unwrap();
    assert!(supervisor.shutdown(LIMIT).await.complete);
    assert!(system.shutdown(LIMIT).await.complete);
}

#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn stuck_supervisor_does_not_prevent_earlier_standalone_shutdown_after_system_deadline() {
    use spawned_concurrency::tasks::ActorStart;
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("stuck-tree-order", 1).unwrap();
    let earlier = Counter(0).start_on(&group).unwrap();
    earlier.ready().await.unwrap();
    let (entered_tx, entered) = tokio::sync::oneshot::channel();
    let (release, release_rx) = tokio::sync::oneshot::channel();
    let args = Arc::new(std::sync::Mutex::new(Some((entered_tx, release_rx))));
    let mut tree = system.supervisor(config(5)).unwrap();
    let inside = tree
        .add(ChildSpec::new("earlier-inside-tree", &group, |_| async {
            Ok::<_, String>(Counter(0))
        }))
        .unwrap();
    tree.add(
        ChildSpec::new("stuck", &group, move |_| {
            let (entered, release) = args.lock().unwrap().take().unwrap();
            async move {
                Ok::<_, String>(SlowStop {
                    entered: Some(entered),
                    release: Some(release),
                })
            }
        })
        .shutdown_timeout(Duration::from_secs(60)),
    )
    .unwrap();
    let _supervisor = tree.start().await.unwrap();
    let inner_actor = inside.current().unwrap();
    let report = system.shutdown(Duration::from_millis(20)).await;
    assert!(!report.complete);
    spawned_rt::tasks::timeout(LIMIT, entered)
        .await
        .unwrap()
        .unwrap();
    // Signalled after the system's grace expired, they also get the kill
    // at grace (E3): Shutdown if they reached their death point first,
    // otherwise Killed.
    for actor in [earlier.child_handle(), inner_actor.child_handle()] {
        let reason = spawned_rt::tasks::timeout(LIMIT, actor.wait_exit_async())
            .await
            .unwrap();
        assert!(
            matches!(reason, ExitReason::Shutdown | ExitReason::Killed),
            "{reason:?}"
        );
    }
    release.send(()).unwrap();
    assert!(report.wait(LIMIT).await.complete);
}

struct StopDuringStart;
impl Actor for StopDuringStart {
    async fn started(&mut self, ctx: &Context<Self>) {
        ctx.stop();
    }
}
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn normal_pre_ready_exit_records_real_cause_and_fails_transient_startup() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("normal-start-exit", 1).unwrap();
    let mut tree = system.supervisor(config(5)).unwrap();
    let child = tree
        .add(
            ChildSpec::new("self-stop", &group, |_| async {
                Ok::<_, String>(StopDuringStart)
            })
            .restart(RestartMode::Transient),
        )
        .unwrap();
    assert!(spawned_rt::tasks::timeout(LIMIT, tree.start())
        .await
        .unwrap()
        .is_err());
    assert!(system.shutdown(LIMIT).await.complete);
    let snapshot = child.snapshot();
    assert_eq!(snapshot.generation, 1);
    assert_eq!(snapshot.last_exit, Some(ExitReason::Normal));
    assert!(snapshot.actor_id.is_some());
}

/// Exits before readiness on its second generation, then starts normally.
struct ExitOnceDuringRestart {
    exit: Option<ExitReason>,
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
        _reply: spawned_concurrency::reply::PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
        {
            panic!("injected child failure");
        }
    }
}

/// A Transient child's *replacement* that stops `Normal` or `Shutdown`
/// before readiness is a failed restart attempt, retried within the restart
/// budget, as OTP retries a failed restart (`try_again_restart`) whatever the
/// child's restart type. Only the initial start fails the tree
/// (`normal_pre_ready_exit_records_real_cause_and_fails_transient_startup`).
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn transient_replacement_exiting_before_readiness_is_retried() {
    for reason in [ExitReason::Normal, ExitReason::Shutdown] {
        let system = ActorSystem::new().unwrap();
        let group = system.async_group("replacement-start-exit", 1).unwrap();
        let mut tree = system.supervisor(config(5)).unwrap();
        let exit = reason.clone();
        let child = tree
            .add(
                ChildSpec::new("replacement", &group, move |ctx| {
                    let exit = (ctx.generation() == 2).then(|| exit.clone());
                    async move { Ok::<_, String>(ExitOnceDuringRestart { exit }) }
                })
                .restart(RestartMode::Transient),
            )
            .unwrap();
        let supervisor = tree.start().await.unwrap();
        assert!(child.current().unwrap().request(Crash).await.is_err());
        let replacement = spawned_rt::tasks::timeout(LIMIT, child.wait_for_generation(2))
            .await
            .unwrap();
        assert!(
            replacement.is_ok(),
            "{reason:?}: replacement not retried; failure: {:?}",
            supervisor.failure()
        );
        assert_eq!(child.snapshot().generation, 3);
        assert!(supervisor.failure().is_none());
        assert!(supervisor.shutdown(LIMIT).await.complete);
        assert!(system.shutdown(LIMIT).await.complete);
    }
}

/// The `Shutdown` twin of
/// `normal_pre_ready_exit_records_real_cause_and_fails_transient_startup`:
/// before its initial readiness a Transient child stopping with `Shutdown`
/// fails the start too.
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_pre_ready_exit_fails_transient_startup() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("shutdown-start-exit", 1).unwrap();
    let mut tree = system.supervisor(config(5)).unwrap();
    let child = tree
        .add(
            ChildSpec::new("self-shutdown", &group, |_| async {
                Ok::<_, String>(ExitOnceDuringRestart {
                    exit: Some(ExitReason::Shutdown),
                })
            })
            .restart(RestartMode::Transient),
        )
        .unwrap();
    assert!(spawned_rt::tasks::timeout(LIMIT, tree.start())
        .await
        .unwrap()
        .is_err());
    assert!(system.shutdown(LIMIT).await.complete);
    let snapshot = child.snapshot();
    assert_eq!(snapshot.generation, 1);
    assert_eq!(snapshot.last_exit, Some(ExitReason::Shutdown));
}

/// Failed replacements consume the restart budget like failed factories:
/// with `max_restarts` 3 a Transient child whose replacements always stop
/// before readiness gets exactly three replacements, then the supervisor
/// fails with an exhausted budget.
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_replacements_exhaust_the_restart_budget_exactly() {
    for reason in [ExitReason::Normal, ExitReason::Shutdown] {
        let system = ActorSystem::new().unwrap();
        let group = system.async_group("replacement-budget", 1).unwrap();
        let mut policy = config(3);
        policy.restart_window = Duration::from_secs(60);
        let mut tree = system.supervisor(policy).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let exit = reason.clone();
        let child = tree
            .add(
                ChildSpec::new("always-fails-restart", &group, move |ctx| {
                    count.fetch_add(1, Ordering::SeqCst);
                    let exit = (ctx.generation() > 1).then(|| exit.clone());
                    async move { Ok::<_, String>(ExitOnceDuringRestart { exit }) }
                })
                .restart(RestartMode::Transient),
            )
            .unwrap();
        let supervisor = tree.start().await.unwrap();
        assert!(child.current().unwrap().request(Crash).await.is_err());
        spawned_rt::tasks::timeout(LIMIT, supervisor.join())
            .await
            .unwrap();
        let failure = supervisor.failure().unwrap();
        assert!(
            failure.contains("budget exhausted"),
            "{reason:?}: {failure}"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 4, "{reason:?}");
        assert_eq!(child.snapshot().generation, 4, "{reason:?}");
        assert!(system.shutdown(LIMIT).await.complete);
    }
}

#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn disabled_retries_fail_without_waiting_for_a_long_backoff() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("no-retries", 1).unwrap();
    let mut policy = config(0);
    policy.backoff.initial = Duration::from_secs(60);
    policy.backoff.maximum = Duration::from_secs(60);
    let mut tree = system.supervisor(policy).unwrap();
    let child = tree
        .add(ChildSpec::new("failed", &group, |_| async {
            Err::<Counter, _>("permanent construction failure")
        }))
        .unwrap();
    assert!(
        spawned_rt::tasks::timeout(Duration::from_secs(1), tree.start())
            .await
            .unwrap()
            .is_err()
    );
    assert!(system.shutdown(LIMIT).await.complete);
    assert_eq!(child.snapshot().generation, 1);
}

struct SlowInit {
    first: bool,
}
impl Actor for SlowInit {
    async fn started(&mut self, _: &Context<Self>) {
        if self.first {
            spawned_rt::tasks::sleep(Duration::from_millis(100)).await;
        }
    }
}
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn transient_startup_timeout_retries_after_actual_cleanup() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("startup-timeout-retry", 1).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let attempts = calls.clone();
    let mut tree = system.supervisor(config(3)).unwrap();
    let child = tree
        .add(
            ChildSpec::new("slow-init", &group, move |ctx| {
                attempts.fetch_add(1, Ordering::SeqCst);
                async move {
                    Ok::<_, String>(SlowInit {
                        first: ctx.generation() == 1,
                    })
                }
            })
            .restart(RestartMode::Transient)
            .startup_timeout(Duration::from_millis(20)),
        )
        .unwrap();
    let supervisor = spawned_rt::tasks::timeout(LIMIT, tree.start())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(child.snapshot().generation, 2);
    assert_eq!(child.snapshot().last_exit, Some(ExitReason::Shutdown));
    assert!(child.current().is_ok());
    assert!(supervisor.shutdown(LIMIT).await.complete);
    assert!(system.shutdown(LIMIT).await.complete);
}

#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn supervisor_report_deadline_signals_remaining_siblings() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("supervisor-report-grace", 1).unwrap();
    let (entered_tx, entered) = tokio::sync::oneshot::channel();
    let (release, release_rx) = tokio::sync::oneshot::channel();
    let args = Arc::new(std::sync::Mutex::new(Some((entered_tx, release_rx))));
    let mut tree = system.supervisor(config(5)).unwrap();
    let first = tree
        .add(ChildSpec::new("first", &group, |_| async {
            Ok::<_, String>(Counter(0))
        }))
        .unwrap();
    tree.add(
        ChildSpec::new("last", &group, move |_| {
            let (entered, release) = args.lock().unwrap().take().unwrap();
            async move {
                Ok::<_, String>(SlowStop {
                    entered: Some(entered),
                    release: Some(release),
                })
            }
        })
        .shutdown_timeout(Duration::from_secs(60)),
    )
    .unwrap();
    let supervisor = tree.start().await.unwrap();
    let actor = first.current().unwrap();
    let report = supervisor.shutdown(Duration::from_millis(20)).await;
    assert!(!report.complete);
    spawned_rt::tasks::timeout(LIMIT, entered)
        .await
        .unwrap()
        .unwrap();
    // The first child must stop while the last one is still held, so the
    // bound is a hang detector, not a latency budget.
    assert_eq!(
        spawned_rt::tasks::timeout(LIMIT, actor.wait_exit())
            .await
            .unwrap(),
        ExitReason::Shutdown
    );
    release.send(()).unwrap();
    assert!(supervisor.shutdown(LIMIT).await.complete);
    assert!(system.shutdown(LIMIT).await.complete);
}

// ---------------------------------------------------------------------------
// Gaps found by mutation testing (cargo-mutants on supervision.rs/system.rs).
// Each test below fails when the corresponding mutant is applied.
// ---------------------------------------------------------------------------

async fn restart_latency(
    child: &spawned_concurrency::supervision::SupervisedChild<Counter>,
) -> Duration {
    let old = child.current().unwrap();
    let generation = child.snapshot().generation;
    let began = spawned_rt::time::Instant::now();
    assert!(old.request(Crash).await.is_err());
    spawned_rt::tasks::timeout(LIMIT, child.wait_for_generation(generation))
        .await
        .unwrap()
        .unwrap();
    began.elapsed()
}

/// Backoff grows across consecutive failures and resets only after a
/// generation stayed ready for `stable_after` (Scope::was_stable/end_running).
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn backoff_exponent_resets_only_after_a_stable_generation() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("backoff-reset", 1).unwrap();
    let mut tree = system
        .supervisor(SupervisorConfig {
            max_restarts: 20,
            backoff: Backoff {
                initial: Duration::from_millis(20),
                maximum: Duration::from_secs(2),
                stable_after: Duration::from_millis(300),
                jitter: false,
                seed: Some(7),
            },
            ..SupervisorConfig::default()
        })
        .unwrap();
    let child = tree
        .add(ChildSpec::new("flappy", &group, |_| async {
            Ok::<_, String>(Counter(0))
        }))
        .unwrap();
    let supervisor = tree.start().await.unwrap();
    let snapshots = supervisor.snapshots();
    assert_eq!(snapshots.len(), 1);
    assert_eq!(snapshots[0].name, "flappy");
    // Four quick failures: delays 20, 40, 80, 160 ms; the fifth would be 320 ms.
    for _ in 0..4 {
        restart_latency(&child).await;
    }
    let unstable = restart_latency(&child).await;
    assert!(
        unstable >= Duration::from_millis(250),
        "backoff did not grow without a stable generation: {unstable:?}"
    );
    // Stay ready past stable_after: the next restart uses the initial delay.
    spawned_rt::tasks::sleep(Duration::from_millis(450)).await;
    // Without the reset this delay would be 640 ms; the margin absorbs a
    // loaded scheduler without accepting a missed reset.
    let reset = restart_latency(&child).await;
    assert!(
        reset < Duration::from_millis(500),
        "backoff did not reset after a stable generation: {reset:?}"
    );
    assert!(supervisor.shutdown(LIMIT).await.complete);
    assert!(system.shutdown(LIMIT).await.complete);
}

/// Dropping a builder that was never started finishes its children, so
/// waiters observe Stopped instead of hanging (Drop for SupervisorBuilder).
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_an_unstarted_builder_releases_child_waiters() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("unstarted", 1).unwrap();
    let mut tree = system.supervisor(config(5)).unwrap();
    let child = tree
        .add(ChildSpec::new("never", &group, |_| async {
            Ok::<_, String>(Counter(0))
        }))
        .unwrap();
    drop(tree);
    let waited = spawned_rt::tasks::timeout(LIMIT, child.wait_ready())
        .await
        .expect("waiter hung after the builder was dropped");
    assert!(waited.is_err());
    assert_eq!(
        child.snapshot().state,
        spawned_concurrency::supervision::ChildState::Stopped
    );
    assert!(system.shutdown(LIMIT).await.complete);
}

/// Aborting the public startup future alone (without a system shutdown)
/// cancels construction and its owned helpers (Drop for StartupGuard).
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn aborted_startup_cancels_construction_without_system_shutdown() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("abort-startup", 1).unwrap();
    let (entered_tx, entered) = tokio::sync::oneshot::channel();
    let entered_tx = Arc::new(std::sync::Mutex::new(Some(entered_tx)));
    let live = Arc::new(AtomicBool::new(false));
    let live_factory = live.clone();
    let mut tree = system.supervisor(config(5)).unwrap();
    tree.add(ChildSpec::new("pending", &group, move |ctx| {
        let entered = entered_tx.lock().unwrap().take();
        let live = live_factory.clone();
        async move {
            live.store(true, Ordering::SeqCst);
            let drop_flag = DropFlag(live);
            ctx.spawn_helper(async move {
                let _drop_flag = drop_flag;
                std::future::pending::<()>().await;
            })
            .unwrap();
            let _ = entered.unwrap().send(());
            std::future::pending::<Result<Counter, String>>().await
        }
    }))
    .unwrap();
    let startup = spawned_rt::tasks::spawn(tree.start());
    spawned_rt::tasks::timeout(LIMIT, entered)
        .await
        .unwrap()
        .unwrap();
    startup.abort();
    assert!(startup.await.is_err());
    spawned_rt::tasks::timeout(LIMIT, async {
        while live.load(Ordering::SeqCst) {
            spawned_rt::tasks::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("aborting startup did not cancel construction helpers");
    assert!(system.shutdown(LIMIT).await.complete);
}

/// A failed supervisor's error appears in the system shutdown report
/// (ShutdownUnit::failure for Core).
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn system_shutdown_reports_supervisor_failures() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("report-failure", 1).unwrap();
    let mut tree = system.supervisor(config(0)).unwrap();
    let child = tree
        .add(ChildSpec::new("fragile", &group, |_| async {
            Ok::<_, String>(Counter(0))
        }))
        .unwrap();
    let supervisor = tree.start().await.unwrap();
    assert!(child.current().unwrap().request(Crash).await.is_err());
    spawned_rt::tasks::timeout(LIMIT, supervisor.join())
        .await
        .unwrap();
    assert!(supervisor.failure().is_some());
    let report = system.shutdown(LIMIT).await;
    assert!(report.complete);
    assert_eq!(
        report.supervisor_failures.len(),
        1,
        "{:?}",
        report.supervisor_failures
    );
    assert!(report.supervisor_failures[0].contains("restart budget exhausted"));
}

/// When the system deadline expires on a stuck later child, earlier
/// supervised siblings are still signalled (ShutdownUnit::stop_remaining).
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn system_deadline_signals_remaining_supervised_siblings() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("system-report-grace", 1).unwrap();
    let (entered_tx, entered) = tokio::sync::oneshot::channel();
    let (release, release_rx) = tokio::sync::oneshot::channel();
    let args = Arc::new(std::sync::Mutex::new(Some((entered_tx, release_rx))));
    let mut tree = system.supervisor(config(5)).unwrap();
    let first = tree
        .add(ChildSpec::new("first", &group, |_| async {
            Ok::<_, String>(Counter(0))
        }))
        .unwrap();
    tree.add(
        ChildSpec::new("last", &group, move |_| {
            let (entered, release) = args.lock().unwrap().take().unwrap();
            async move {
                Ok::<_, String>(SlowStop {
                    entered: Some(entered),
                    release: Some(release),
                })
            }
        })
        .shutdown_timeout(Duration::from_secs(60)),
    )
    .unwrap();
    let _supervisor = tree.start().await.unwrap();
    let actor = first.current().unwrap();
    let report = system.shutdown(Duration::from_millis(50)).await;
    assert!(!report.complete);
    spawned_rt::tasks::timeout(LIMIT, entered)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        spawned_rt::tasks::timeout(Duration::from_secs(1), actor.wait_exit())
            .await
            .expect("earlier sibling was not stopped after the system deadline"),
        ExitReason::Killed,
        "signalled at the system's grace expiry: the kill (E3)"
    );
    release.send(()).unwrap();
    assert!(report.wait(LIMIT).await.complete);
}

/// Invalid child names and zero timeouts are rejected
/// (SupervisorBuilder::add validation).
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn child_specs_reject_invalid_names_and_zero_timeouts() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("validation", 1).unwrap();
    let mut tree = system.supervisor(config(5)).unwrap();
    let factory = |_| async { Ok::<_, String>(Counter(0)) };
    for spec in [
        ChildSpec::new("", &group, factory),
        ChildSpec::new("nul\0name", &group, factory),
        ChildSpec::new("zero-startup", &group, factory).startup_timeout(Duration::ZERO),
        ChildSpec::new("zero-shutdown", &group, factory).shutdown_timeout(Duration::ZERO),
    ] {
        assert!(matches!(
            tree.add(spec),
            Err(spawned_concurrency::supervision::SupervisorError::Configuration(_))
        ));
    }
    drop(tree);
    assert!(system.shutdown(LIMIT).await.complete);
}

struct Named(&'static str);
impl Actor for Named {
    async fn started(&mut self, ctx: &Context<Self>) {
        ctx.register_name(self.0)
            .expect("previous generation's registration was not released");
    }
}
impl Handler<Crash> for Named {
    async fn handle(
        &mut self,
        _: Crash,
        _reply: spawned_concurrency::reply::PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
        {
            panic!("crash a registered child");
        }
    }
}

/// A generation's owned name registration is released before its
/// replacement starts, so the replacement can register the same name
/// (Drop for Lease accounting).
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn owned_registration_is_released_before_the_replacement_registers() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("registered", 1).unwrap();
    let mut tree = system.supervisor(config(5)).unwrap();
    let child = tree
        .add(ChildSpec::new("named", &group, |_| async {
            Ok::<_, String>(Named("supervision-gap-registered"))
        }))
        .unwrap();
    let supervisor = tree.start().await.unwrap();
    for generation in 1..=3 {
        let actor = child.current().unwrap();
        assert!(actor.request(Crash).await.is_err());
        let replacement = spawned_rt::tasks::timeout(LIMIT, child.wait_for_generation(generation))
            .await
            .unwrap()
            .expect("replacement failed to start");
        assert_ne!(replacement.id(), actor.id());
    }
    assert!(supervisor.shutdown(LIMIT).await.complete);
    assert!(system.shutdown(LIMIT).await.complete);
    assert!(
        spawned_concurrency::registry::whereis::<spawned_concurrency::tasks::ActorRef<Named>>(
            "supervision-gap-registered"
        )
        .is_none()
    );
}

/// `FactoryContext::cancelled` stays pending while construction is live: a
/// factory that waits on it never produces an actor, so its startup limit
/// fails the tree instead of the factory finishing early.
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn factory_cancelled_waits_for_construction_cancellation() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("factory-cancelled", 1).unwrap();
    let mut tree = system.supervisor(config(0)).unwrap();
    tree.add(
        ChildSpec::new("waits", &group, |ctx| async move {
            ctx.cancelled().await;
            Ok::<_, String>(Counter(0))
        })
        .startup_timeout(Duration::from_millis(50)),
    )
    .unwrap();
    let started = spawned_rt::tasks::timeout(LIMIT, tree.start())
        .await
        .unwrap();
    assert!(started.is_err(), "factory finished before cancellation");
    assert!(system.shutdown(LIMIT).await.complete);
}

/// In a cohort strategy the failed child's backoff races only a superseding
/// recovery round; with none pending it must sleep the full delay.
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn one_for_all_restart_waits_for_its_backoff() {
    use spawned_concurrency::supervision::SupervisorStrategy;
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("ofa-backoff", 1).unwrap();
    let mut tree = system
        .supervisor(SupervisorConfig {
            strategy: SupervisorStrategy::OneForAll,
            max_restarts: 5,
            backoff: Backoff {
                initial: Duration::from_millis(300),
                maximum: Duration::from_secs(2),
                jitter: false,
                seed: Some(1),
                ..Backoff::default()
            },
            ..SupervisorConfig::default()
        })
        .unwrap();
    let child = tree
        .add(ChildSpec::new("a", &group, |_| async {
            Ok::<_, String>(Counter(0))
        }))
        .unwrap();
    tree.add(ChildSpec::new("b", &group, |_| async {
        Ok::<_, String>(Counter(0))
    }))
    .unwrap();
    let supervisor = tree.start().await.unwrap();
    // A lower bound only: a slow machine makes the restart later, not earlier.
    let latency = restart_latency(&child).await;
    assert!(
        latency >= Duration::from_millis(250),
        "restart skipped its backoff: {latency:?}"
    );
    assert!(supervisor.shutdown(LIMIT).await.complete);
    assert!(system.shutdown(LIMIT).await.complete);
}

#[test]
fn supervisor_config_limits_are_validated_at_their_boundaries() {
    use spawned_concurrency::supervision::SupervisorError;
    let system = ActorSystem::new().unwrap();
    let accepts = |config: SupervisorConfig| system.supervisor(config).is_ok();
    let rejects = |config: SupervisorConfig| {
        matches!(
            system.supervisor(config),
            Err(SupervisorError::Configuration(_))
        )
    };
    let base = SupervisorConfig::default;
    assert!(accepts(base()));
    assert!(rejects(SupervisorConfig {
        restart_window: Duration::ZERO,
        ..base()
    }));
    assert!(rejects(SupervisorConfig {
        max_children: 0,
        ..base()
    }));
    assert!(accepts(SupervisorConfig {
        max_children: 65536,
        ..base()
    }));
    assert!(rejects(SupervisorConfig {
        max_children: 65537,
        ..base()
    }));
    assert!(accepts(SupervisorConfig {
        max_restarts: 65536,
        ..base()
    }));
    assert!(rejects(SupervisorConfig {
        max_restarts: 65537,
        ..base()
    }));
    let backoff = |initial, maximum| Backoff {
        initial: Duration::from_millis(initial),
        maximum: Duration::from_millis(maximum),
        ..Backoff::default()
    };
    assert!(accepts(SupervisorConfig {
        backoff: backoff(10, 10),
        ..base()
    }));
    assert!(rejects(SupervisorConfig {
        backoff: backoff(11, 10),
        ..base()
    }));
}
