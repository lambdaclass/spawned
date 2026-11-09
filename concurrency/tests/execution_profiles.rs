#![cfg(feature = "async")]
use spawned_concurrency::{
    message::Message,
    supervision::{Backoff, ChildSpec, SupervisorConfig, SupervisorStrategy},
    system::{ActorSystem, ExecutionProfile, SystemError},
    tasks::{Actor, ActorStart, Context, Handler},
};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
const LIMIT: Duration = Duration::from_secs(5);
struct Probe;
impl Actor for Probe {}
struct Audit;
impl Message for Audit {
    type Result = (String, Option<usize>);
}
impl Handler<Audit> for Probe {
    async fn handle(&mut self, _: Audit, _: &Context<Self>) -> <Audit as Message>::Result {
        tokio::time::sleep(Duration::from_millis(1)).await;
        (
            std::thread::current().name().unwrap_or_default().into(),
            spawned_rt::tasks::affinity::current_cpu().ok(),
        )
    }
}
struct Block {
    entered: tokio::sync::oneshot::Sender<()>,
    release: std::sync::mpsc::Receiver<()>,
}
impl Message for Block {
    type Result = ();
}
impl Handler<Block> for Probe {
    async fn handle(&mut self, msg: Block, _: &Context<Self>) {
        tokio::task::block_in_place(|| {
            let _ = msg.entered.send(());
            msg.release.recv_timeout(LIMIT).unwrap();
        });
    }
}
struct Crash;
impl Message for Crash {
    type Result = ();
}
impl Handler<Crash> for Probe {
    async fn handle(&mut self, _: Crash, _: &Context<Self>) {
        panic!("profile recovery injection");
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persistent_native_slots_bound_admission_and_helpers_progress_during_blocking_turns() {
    let system = ActorSystem::new().unwrap();
    let group = system.thread_group("native-roots", 2).unwrap();
    assert_eq!(
        group.profile(),
        Some(ExecutionProfile::ThreadPool {
            workers: 2,
            helper_workers: 1
        })
    );
    let a = Probe.start_on(&group.shard(0).unwrap()).unwrap();
    let b = Probe.start_on(&group.shard(1).unwrap()).unwrap();
    a.ready().await.unwrap();
    b.ready().await.unwrap();
    assert!(matches!(Probe.start_on(&group), Err(SystemError::Capacity)));
    assert!(a.request(Audit).await.unwrap().0.ends_with("actor-0"));
    let (entered_tx, entered) = tokio::sync::oneshot::channel();
    let (release, release_rx) = std::sync::mpsc::channel();
    a.send(Block {
        entered: entered_tx,
        release: release_rx,
    })
    .unwrap();
    tokio::time::timeout(LIMIT, entered).await.unwrap().unwrap();
    // A blocked root does not occupy the group's single async helper worker.
    let ctx = a.context();
    let helper = std::thread::spawn(move || {
        ctx.spawn_helper(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let connect = tokio::net::TcpStream::connect(address);
            let (accepted, connected) = tokio::join!(listener.accept(), connect);
            accepted.unwrap();
            connected.unwrap();
            tokio::time::sleep(Duration::from_millis(1)).await;
            std::thread::current().name().unwrap().to_string()
        })
        .unwrap()
    })
    .join()
    .unwrap();
    let helper_thread = tokio::time::timeout(LIMIT, helper.join())
        .await
        .unwrap()
        .unwrap();
    assert!(helper_thread.contains("helper"));
    assert!(tokio::time::timeout(LIMIT, b.request(Audit))
        .await
        .unwrap()
        .is_ok());
    release.send(()).unwrap();
    assert!(system.shutdown(LIMIT).await.complete);
    assert!(matches!(Probe.start_on(&group), Err(SystemError::Closed)));
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_capacity_is_released_only_after_registered_helper_cleanup() {
    let system = ActorSystem::new().unwrap();
    let group = system.thread_group("slot-cleanup", 1).unwrap();
    let actor = Probe.start_on(&group).unwrap();
    actor.ready().await.unwrap();
    let (entered_tx, entered) = tokio::sync::oneshot::channel();
    let (release, release_rx) = std::sync::mpsc::channel();
    let helper = actor
        .context()
        .spawn_blocking_helper(move || {
            entered_tx.send(()).unwrap();
            release_rx.recv_timeout(LIMIT).unwrap();
        })
        .unwrap();
    tokio::time::timeout(LIMIT, entered).await.unwrap().unwrap();
    actor.context().stop();
    actor.child_handle().wait_exit_async().await;
    assert!(matches!(Probe.start_on(&group), Err(SystemError::Capacity)));
    release.send(()).unwrap();
    drop(helper);
    let replacement = tokio::time::timeout(LIMIT, async {
        loop {
            match Probe.start_on(&group) {
                Ok(actor) => break actor,
                Err(SystemError::Capacity) => tokio::task::yield_now().await,
                Err(e) => panic!("{e}"),
            }
        }
    })
    .await
    .unwrap();
    replacement.ready().await.unwrap();
    assert!(system.shutdown(LIMIT).await.complete);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_strategy_recovers_on_a_full_native_pool_without_overlapping_slots() {
    for strategy in [
        SupervisorStrategy::OneForOne,
        SupervisorStrategy::OneForAll,
        SupervisorStrategy::RestForOne,
    ] {
        let system = ActorSystem::new().unwrap();
        let group = system.thread_group("supervised-roots", 3).unwrap();
        let mut tree = system
            .supervisor(SupervisorConfig {
                strategy,
                max_restarts: 20,
                backoff: Backoff {
                    initial: Duration::from_millis(1),
                    maximum: Duration::from_millis(2),
                    jitter: false,
                    ..Backoff::default()
                },
                ..SupervisorConfig::default()
            })
            .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let mut children = Vec::new();
        for i in 0..3 {
            let calls = calls.clone();
            children.push(
                tree.add(ChildSpec::new(
                    format!("root-{i}"),
                    &group.shard(i).unwrap(),
                    move |_| {
                        calls.fetch_add(1, Ordering::SeqCst);
                        async { Ok::<_, String>(Probe) }
                    },
                ))
                .unwrap(),
            );
        }
        let supervisor = tree.start().await.unwrap();
        assert!(children[1].current().unwrap().request(Crash).await.is_err());
        for (i, child) in children.iter().enumerate() {
            if strategy == SupervisorStrategy::OneForAll
                || i == 1
                || (strategy == SupervisorStrategy::RestForOne && i == 2)
            {
                let next = tokio::time::timeout(LIMIT, child.wait_for_generation(1))
                    .await
                    .unwrap()
                    .unwrap();
                assert!(next
                    .request(Audit)
                    .await
                    .unwrap()
                    .0
                    .ends_with(&format!("actor-{i}")));
            }
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            match strategy {
                SupervisorStrategy::OneForOne => 4,
                SupervisorStrategy::OneForAll => 6,
                SupervisorStrategy::RestForOne => 5,
            }
        );
        assert!(supervisor.shutdown(LIMIT).await.complete);
        assert!(system.shutdown(LIMIT).await.complete);
    }
}
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pinned_shards_preserve_explicit_cpu_placement_and_cross_shard_requests() {
    let cpus = spawned_rt::tasks::affinity::available_cpus().unwrap();
    let cpus: Vec<_> = cpus.into_iter().take(2).collect();
    let system = ActorSystem::new().unwrap();
    assert!(system
        .pinned_group("duplicates", vec![cpus[0], cpus[0]])
        .is_err());
    assert!(system.pinned_group("outside", vec![usize::MAX]).is_err());
    let group = system.pinned_group("pipeline", cpus.clone()).unwrap();
    assert!(group.shard(cpus.len()).is_err());
    let actors: Vec<_> = (0..cpus.len())
        .map(|i| Probe.start_on(&group.shard(i).unwrap()).unwrap())
        .collect();
    for (i, actor) in actors.iter().enumerate() {
        actor.ready().await.unwrap();
        for _ in 0..8 {
            assert_eq!(actor.request(Audit).await.unwrap().1, Some(cpus[i]));
        }
        let ctx = actor.context();
        let peer = actors[(i + 1) % actors.len()].clone();
        let expected = cpus[(i + 1) % cpus.len()];
        let helper = std::thread::spawn(move || {
            ctx.spawn_helper(async move {
                assert_eq!(peer.request(Audit).await.unwrap().1, Some(expected));
                spawned_rt::tasks::affinity::current_cpu().unwrap()
            })
            .unwrap()
        })
        .join()
        .unwrap();
        assert_eq!(helper.join().await.unwrap(), cpus[i]);
    }
    assert!(system.async_group("async", 1).unwrap().shard(0).is_err());
    let report = system.shutdown(LIMIT).await;
    assert!(report.complete);
    assert!(report.group_failures.is_empty());
}
#[cfg(target_os = "linux")]
mod tls_join {
    use super::*;
    thread_local! { static PROBE: std::cell::RefCell<Option<DropProbe>> = const { std::cell::RefCell::new(None) }; }
    struct DropProbe {
        entered: tokio::sync::oneshot::Sender<()>,
        release: std::sync::mpsc::Receiver<()>,
    }
    impl Drop for DropProbe {
        fn drop(&mut self) {
            // Sender is consumed through a replacement because Drop cannot move fields.
            let (placeholder, _) = tokio::sync::oneshot::channel();
            let _ = std::mem::replace(&mut self.entered, placeholder).send(());
            self.release.recv_timeout(LIMIT).unwrap();
        }
    }
    struct Install(DropProbe);
    impl Message for Install {
        type Result = ();
    }
    impl Handler<Install> for Probe {
        async fn handle(&mut self, msg: Install, _: &Context<Self>) {
            PROBE.with(|p| *p.borrow_mut() = Some(msg.0));
        }
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_reports_wait_for_actual_pinned_worker_tls_destruction() {
        let system = ActorSystem::new().unwrap();
        let cpu = spawned_rt::tasks::affinity::available_cpus().unwrap()[0];
        let group = system.pinned_group("tls-owner", vec![cpu]).unwrap();
        let actor = Probe.start_on(&group).unwrap();
        actor.ready().await.unwrap();
        let (entered_tx, entered) = tokio::sync::oneshot::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        actor
            .request(Install(DropProbe {
                entered: entered_tx,
                release: release_rx,
            }))
            .await
            .unwrap();
        let report = system.shutdown(Duration::from_millis(20)).await;
        tokio::time::timeout(LIMIT, entered).await.unwrap().unwrap();
        assert!(!report.complete);
        assert_eq!(report.unfinished_groups, ["tls-owner"]);
        assert!(actor.child_handle().exit_reason().is_some());
        release.send(()).unwrap();
        let report = report.wait(LIMIT).await;
        assert!(report.complete);
        assert!(report.group_failures.is_empty());
    }
}

#[cfg(target_os = "linux")]
#[test]
fn partial_pinned_construction_rolls_back_before_returning_error() {
    const CHILD: &str = "SPAWNED_PROFILE_FD_FAULT_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "partial_pinned_construction_rolls_back_before_returning_error",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "isolated FD fault failed: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    // This process has exactly one test. Never change a shared test runner's FD limit.
    let cpus = spawned_rt::tasks::affinity::available_cpus().unwrap();
    if cpus.len() < 2 {
        eprintln!("two allowed CPUs required for partial-construction injection");
        return;
    }
    fn fds() -> u64 {
        std::fs::read_dir("/proc/self/fd").unwrap().count() as u64 - 1
    }
    let system = ActorSystem::new().unwrap();
    let baseline = fds();
    let (ready_tx, ready) = std::sync::mpsc::channel();
    let (release, release_rx) = std::sync::mpsc::channel();
    let probe = std::thread::spawn(move || {
        let runtime = spawned_rt::tasks::control_runtime().unwrap();
        ready_tx.send(()).unwrap();
        release_rx.recv_timeout(LIMIT).unwrap();
        drop(runtime);
    });
    ready.recv_timeout(LIMIT).unwrap();
    let cost = fds() - baseline;
    release.send(()).unwrap();
    probe.join().unwrap();
    assert!(cost >= 2, "unexpected reactor FD cost: {cost}");
    assert_eq!(fds(), baseline);
    use nix::sys::resource::{getrlimit, setrlimit, Resource};
    let (soft, hard) = getrlimit(Resource::RLIMIT_NOFILE).unwrap();
    setrlimit(Resource::RLIMIT_NOFILE, baseline + 2 * cost - 1, hard).unwrap();
    let failure = system.pinned_group("rollback-fd", cpus[..2].to_vec());
    setrlimit(Resource::RLIMIT_NOFILE, soft, hard).unwrap();
    assert!(
        matches!(failure, Err(SystemError::Runtime(_))),
        "second reactor must fail after the first starts"
    );
    assert_eq!(
        fds(),
        baseline,
        "failed construction retained reactor descriptors"
    );
    let group = system.async_group("after-rollback", 1).unwrap();
    let runtime = spawned_rt::tasks::control_runtime().unwrap();
    runtime.block_on(async {
        let actor = Probe.start_on(&group).unwrap();
        actor.ready().await.unwrap();
        assert!(actor.request(Audit).await.is_ok());
        assert!(system.shutdown(LIMIT).await.complete);
    });
}
