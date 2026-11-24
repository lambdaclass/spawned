//! Supervised `ignore` (unified start, section 9; OTP's `ignore` from a
//! child's start function): `Init::ignore()` in `started()`, or a factory's
//! `ChildStart::Ignore`. The spec stays with no generation
//! (`ChildState::Ignored`, OTP's `undefined` pid); the ignore completes the
//! child's first start; OneForOne never restarts it; a OneForAll/RestForOne
//! round that covers it starts it again with the cohort; a Temporary child
//! is removed; a replacement that ignores keeps its stamped restart grant;
//! waiters get `ChildUnavailable(Ignored)` at once; shutdown finishes it at
//! once. The ignore is observed only once the instance's owned cleanup has
//! joined: until then the slot is `Stopping`, under V3's stuck rule.
#![cfg(feature = "async")]
#![cfg_attr(spawned_sim, allow(dead_code, unused_imports))]
#[macro_use]
#[path = "support/deadline.rs"]
mod deadline;
use spawned_concurrency::{
    message::Message,
    supervision::{
        Backoff, ChildSpec, ChildStart, ChildState, ChildUnavailable, RestartMode, SupervisedChild,
        SupervisorConfig, SupervisorError, SupervisorStrategy,
    },
    system::ActorSystem,
    tasks::{Actor, Context, Handler, Init},
};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

const LIMIT: Duration = Duration::from_secs(5);

fn config(strategy: SupervisorStrategy, max_restarts: usize) -> SupervisorConfig {
    SupervisorConfig::default()
        .strategy(strategy)
        .max_restarts(max_restarts)
        .backoff(
            Backoff::default()
                .initial(Duration::from_millis(1))
                .maximum(Duration::from_millis(2))
                .jitter(false),
        )
}

struct Crash;
impl Message for Crash {
    type Result = ();
}

/// Ready, or `Init::ignore()` from `started()`.
struct Child {
    ignore: bool,
}
impl Actor for Child {
    async fn started(&mut self, _: &Context<Self>) -> Init<Self> {
        if self.ignore {
            Init::ignore()
        } else {
            Init::ready()
        }
    }
}
impl Handler<Crash> for Child {
    async fn handle(
        &mut self,
        _: Crash,
        _reply: spawned_concurrency::PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
        panic!("injected child failure");
    }
}

/// A child whose factory logs `index:generation` and ignores in the
/// generations `ignore` selects.
fn child(
    index: usize,
    group: &spawned_concurrency::system::ExecutionGroup,
    log: &Arc<Mutex<Vec<String>>>,
    ignore: impl Fn(u64) -> bool + Send + Sync + 'static,
) -> ChildSpec<Child> {
    let log = log.clone();
    let ignore = Arc::new(ignore);
    ChildSpec::new(format!("child-{index}"), group, move |ctx| {
        let log = log.clone();
        let ignore = ignore.clone();
        async move {
            log.lock()
                .unwrap()
                .push(format!("{index}:{}", ctx.generation()));
            Ok::<_, String>(Child {
                ignore: ignore(ctx.generation()),
            })
        }
    })
}

async fn crash(child: &SupervisedChild<Child>) {
    assert!(child.current().unwrap().request(Crash).await.is_err());
}

async fn wait_state<A: Actor>(child: &SupervisedChild<A>, state: ChildState) {
    within!(async {
        while child.snapshot().state != state {
            spawned_rt::tasks::sleep(Duration::from_millis(1)).await;
        }
    })
    .await;
}

fn starts(log: &Arc<Mutex<Vec<String>>>, index: usize) -> usize {
    let prefix = format!("{index}:");
    log.lock()
        .unwrap()
        .iter()
        .filter(|entry| entry.starts_with(&prefix))
        .count()
}

/// The initial start: an ignore completes the child's start, so the later
/// sibling starts and `start()` succeeds. The spec stays Ignored with no
/// actor; waiters are answered at once; OneForOne never restarts it, also
/// while a sibling restarts; shutdown finishes it.
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn one_for_one_keeps_an_ignored_child_and_never_restarts_it() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("ignore-one-for-one", 2).unwrap();
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut tree = system
        .supervisor(config(SupervisorStrategy::OneForOne, 10))
        .unwrap();
    let first = tree.add(child(0, &group, &log, |_| false)).unwrap();
    let ignored = tree.add(child(1, &group, &log, |_| true)).unwrap();
    let last = tree.add(child(2, &group, &log, |_| false)).unwrap();
    let supervisor = within!(tree.start()).await.unwrap();
    assert!(last.current().is_ok(), "the later sibling started");
    let snapshot = ignored.snapshot();
    assert_eq!(snapshot.state, ChildState::Ignored);
    assert_eq!(snapshot.generation, 1);
    assert_eq!(snapshot.actor_id, None);
    assert_eq!(snapshot.last_error, None);
    assert!(matches!(
        ignored.current(),
        Err(ChildUnavailable(ChildState::Ignored))
    ));
    assert!(matches!(
        within!(ignored.wait_ready()).await,
        Err(ChildUnavailable(ChildState::Ignored))
    ));
    assert!(matches!(
        within!(ignored.wait_for_generation(1)).await,
        Err(ChildUnavailable(ChildState::Ignored))
    ));
    crash(&first).await;
    within!(first.wait_for_generation(1)).await.unwrap();
    crash(&last).await;
    within!(last.wait_for_generation(1)).await.unwrap();
    assert_eq!(starts(&log, 1), 1, "nothing restarts an ignored child");
    assert_eq!(ignored.snapshot().state, ChildState::Ignored);
    let report = supervisor.shutdown(LIMIT).await;
    assert!(report.complete);
    assert_eq!(report.failure, None);
    assert_eq!(ignored.snapshot().state, ChildState::Stopped);
    assert!(system.shutdown(LIMIT).await.complete);
}

/// A factory's `ChildStart::Ignore` is the same outcome as `Init::ignore()`,
/// with no instance at all.
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn factory_ignore_is_the_same_outcome() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("ignore-factory", 1).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let mut tree = system
        .supervisor(config(SupervisorStrategy::OneForOne, 10))
        .unwrap();
    let factory_calls = calls.clone();
    let ignored = tree
        .add(ChildSpec::new("optional", &group, move |_| {
            factory_calls.fetch_add(1, Ordering::SeqCst);
            async { Ok::<_, String>(ChildStart::<Child>::Ignore) }
        }))
        .unwrap();
    let log = Arc::new(Mutex::new(Vec::new()));
    let last = tree.add(child(1, &group, &log, |_| false)).unwrap();
    let supervisor = within!(tree.start()).await.unwrap();
    assert!(last.current().is_ok());
    let snapshot = ignored.snapshot();
    assert_eq!(snapshot.state, ChildState::Ignored);
    assert_eq!(snapshot.actor_id, None);
    assert_eq!(snapshot.last_exit, None);
    assert!(matches!(
        ignored.current(),
        Err(ChildUnavailable(ChildState::Ignored))
    ));
    crash(&last).await;
    within!(last.wait_for_generation(1)).await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(supervisor.shutdown(LIMIT).await.complete);
    assert!(system.shutdown(LIMIT).await.complete);
}

/// An ignored Temporary child is removed (OTP deletes it): its controller
/// finishes, and a later round does not rebuild it.
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn ignored_temporary_child_is_removed() {
    for strategy in [SupervisorStrategy::OneForOne, SupervisorStrategy::OneForAll] {
        let system = ActorSystem::new().unwrap();
        let group = system.async_group("ignore-temporary", 2).unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut tree = system.supervisor(config(strategy, 10)).unwrap();
        let first = tree.add(child(0, &group, &log, |_| false)).unwrap();
        let temporary = tree
            .add(child(1, &group, &log, |_| true).restart(RestartMode::Temporary))
            .unwrap();
        let supervisor = within!(tree.start()).await.unwrap();
        wait_state(&temporary, ChildState::Stopped).await;
        crash(&first).await;
        within!(first.wait_for_generation(1)).await.unwrap();
        assert_eq!(
            starts(&log, 1),
            1,
            "{strategy:?}: a removed child is not rebuilt"
        );
        assert!(matches!(
            temporary.current(),
            Err(ChildUnavailable(ChildState::Stopped))
        ));
        let report = supervisor.shutdown(LIMIT).await;
        assert!(report.complete);
        assert_eq!(report.failure, None);
        assert!(system.shutdown(LIMIT).await.complete);
    }
}

/// V5: a OneForOne replacement that ignores keeps the restart grant stamped
/// at its admission and adds nothing: with `max_restarts = 1` the next
/// failure exhausts the budget.
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn replacement_ignore_keeps_its_restart_grant() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("ignore-budget", 2).unwrap();
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut tree = system
        .supervisor(config(SupervisorStrategy::OneForOne, 1))
        .unwrap();
    let optional = tree.add(child(0, &group, &log, |g| g > 1)).unwrap();
    let other = tree.add(child(1, &group, &log, |_| false)).unwrap();
    let supervisor = within!(tree.start()).await.unwrap();
    crash(&optional).await;
    wait_state(&optional, ChildState::Ignored).await;
    assert_eq!(optional.snapshot().generation, 2);
    assert_eq!(supervisor.failure(), None);
    crash(&other).await;
    within!(supervisor.join()).await;
    let failure = supervisor.failure().expect("the budget is exhausted");
    assert!(failure.contains("restart budget exhausted"), "{failure}");
    assert!(system.shutdown(LIMIT).await.complete);
}

/// OneForAll/RestForOne: a round that covers an ignored child starts it
/// again with the cohort, in declaration order; one before the round's
/// range (RestForOne) stays Ignored.
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn round_restarts_an_ignored_child_with_the_cohort() {
    for strategy in [
        SupervisorStrategy::OneForAll,
        SupervisorStrategy::RestForOne,
    ] {
        let system = ActorSystem::new().unwrap();
        let group = system.async_group("ignore-cohort", 2).unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut tree = system.supervisor(config(strategy, 10)).unwrap();
        let before = tree.add(child(0, &group, &log, |g| g == 1)).unwrap();
        let origin = tree.add(child(1, &group, &log, |_| false)).unwrap();
        let after = tree.add(child(2, &group, &log, |g| g == 1)).unwrap();
        let supervisor = within!(tree.start()).await.unwrap();
        assert_eq!(before.snapshot().state, ChildState::Ignored);
        assert_eq!(after.snapshot().state, ChildState::Ignored);
        crash(&origin).await;
        wait_state(&after, ChildState::Running).await;
        assert_eq!(after.snapshot().generation, 2);
        assert!(after.current().is_ok());
        let restarted: Vec<String> = log
            .lock()
            .unwrap()
            .iter()
            .filter(|entry| entry.ends_with(":2"))
            .cloned()
            .collect();
        match strategy {
            SupervisorStrategy::OneForAll => {
                assert_eq!(restarted, ["0:2", "1:2", "2:2"]);
                assert_eq!(before.snapshot().state, ChildState::Running);
            }
            _ => {
                assert_eq!(restarted, ["1:2", "2:2"]);
                assert_eq!(before.snapshot().state, ChildState::Ignored);
                assert_eq!(before.snapshot().generation, 1);
            }
        }
        let report = supervisor.shutdown(LIMIT).await;
        assert!(report.complete);
        assert_eq!(report.failure, None);
        assert!(system.shutdown(LIMIT).await.complete);
    }
}

/// A replacement that ignores inside a round completes the round's
/// readiness barrier: the later sibling still starts, and the round is
/// charged once.
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn round_replacement_ignore_completes_the_barrier() {
    for strategy in [
        SupervisorStrategy::OneForAll,
        SupervisorStrategy::RestForOne,
    ] {
        let system = ActorSystem::new().unwrap();
        let group = system.async_group("ignore-barrier", 2).unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut tree = system.supervisor(config(strategy, 1)).unwrap();
        let origin = tree.add(child(0, &group, &log, |_| false)).unwrap();
        let optional = tree.add(child(1, &group, &log, |g| g == 2)).unwrap();
        let last = tree.add(child(2, &group, &log, |_| false)).unwrap();
        let supervisor = within!(tree.start()).await.unwrap();
        crash(&origin).await;
        within!(last.wait_for_generation(1)).await.unwrap();
        assert_eq!(optional.snapshot().state, ChildState::Ignored);
        assert_eq!(optional.snapshot().generation, 2);
        assert_eq!(supervisor.failure(), None, "{strategy:?}");
        let report = supervisor.shutdown(LIMIT).await;
        assert!(report.complete);
        assert_eq!(report.failure, None);
        assert!(system.shutdown(LIMIT).await.complete);
    }
}

/// A child whose factory leaves a blocking helper in the generation's scope
/// that runs until `release` and then ignores in `started()`.
#[cfg(not(spawned_sim))]
fn held_ignore(
    group: &spawned_concurrency::system::ExecutionGroup,
    release: std::sync::mpsc::Receiver<()>,
) -> ChildSpec<Child> {
    let release = Arc::new(Mutex::new(Some(release)));
    ChildSpec::new("held", group, move |ctx| {
        let release = release.lock().unwrap().take();
        async move {
            if let Some(release) = release {
                // Wait until the helper runs: a helper not yet started when
                // its scope stops is skipped.
                let (entered, running) = tokio::sync::oneshot::channel();
                ctx.spawn_blocking_helper(move || {
                    let _ = entered.send(());
                    let _ = release.recv();
                })
                .map_err(|e| e.to_string())?;
                running.await.map_err(|e| e.to_string())?;
            }
            Ok::<_, String>(Child { ignore: true })
        }
    })
}

/// Codex R4: the ignore is observed only once the instance's owned cleanup
/// joined. Until then the slot is Stopping and the tree's start waits.
#[cfg(not(spawned_sim))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ignore_is_observed_only_after_owned_cleanup() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("ignore-held", 2).unwrap();
    let (release, held) = std::sync::mpsc::channel();
    let mut tree = system
        .supervisor(config(SupervisorStrategy::OneForOne, 10))
        .unwrap();
    let ignored = tree.add(held_ignore(&group, held)).unwrap();
    let start = tokio::spawn(tree.start());
    wait_state(&ignored, ChildState::Stopping).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(ignored.snapshot().state, ChildState::Stopping);
    assert!(!start.is_finished(), "the start waits for the ignore");
    release.send(()).unwrap();
    let supervisor = within!(start).await.unwrap().unwrap();
    assert_eq!(ignored.snapshot().state, ChildState::Ignored);
    assert!(supervisor.shutdown(LIMIT).await.complete);
    assert!(system.shutdown(LIMIT).await.complete);
}

/// A stuck ignore: its owned cleanup misses `shutdown_timeout`, so the slot
/// is stuck under V3 (never Ignored) and the start fails.
#[cfg(not(spawned_sim))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stuck_ignore_fails_the_start() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("ignore-stuck", 2).unwrap();
    let (release, held) = std::sync::mpsc::channel();
    let mut tree = system
        .supervisor(config(SupervisorStrategy::OneForOne, 10))
        .unwrap();
    let ignored = tree
        .add(held_ignore(&group, held).shutdown_timeout(Duration::from_millis(50)))
        .unwrap();
    let error = within!(tree.start()).await.err().expect("a stuck ignore");
    assert!(
        matches!(&error, SupervisorError::StartupCleanupStuck { child, .. } if child == "held"),
        "{error:?}"
    );
    assert_eq!(ignored.snapshot().state, ChildState::Failed);
    release.send(()).unwrap();
    let _ = system.shutdown(LIMIT).await;
}
