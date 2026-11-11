#![cfg(feature = "async")]
//! Deterministic characterization of executor capacity; no machine-wide tuning.
// Tests that need OS threads are compiled out under spawned_sim.
#![cfg_attr(spawned_sim, allow(unused_imports, dead_code))]
#[macro_use]
#[path = "support/deadline.rs"]
mod deadline;
use spawned_concurrency::{
    message::Message,
    tasks::{Actor, ActorStart, Backend, Context, Handler},
    ChildHandle,
};
use std::time::Duration;
const LIMIT: Duration = Duration::from_secs(5);
struct Cleanup(Vec<ChildHandle>);
impl Drop for Cleanup {
    fn drop(&mut self) {
        for actor in &self.0 {
            actor.stop();
        }
    }
}
struct Ping;
impl Message for Ping {
    type Result = ();
}
struct Worker(Option<tokio::sync::oneshot::Sender<()>>);
impl Actor for Worker {
    async fn started(&mut self, _: &Context<Self>) {
        if let Some(tx) = self.0.take() {
            let _ = tx.send(());
        }
    }
}
impl Handler<Ping> for Worker {
    async fn handle(&mut self, _: Ping, _: &Context<Self>) {}
}
// Not simulated: Blocking backend workers are OS threads.
#[cfg(not(spawned_sim))]
#[test]
fn persistent_blocking_actors_queue_until_a_worker_is_released() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .max_blocking_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let mut cleanup = Cleanup(Vec::new());
        let mut actors = Vec::new();
        // Await both started hooks so the finite pool is occupied deterministically.
        for _ in 0..2 {
            let (tx, rx) = tokio::sync::oneshot::channel();
            let actor = Worker(Some(tx)).start_with_backend(Backend::Blocking);
            cleanup.0.push(actor.child_handle());
            tokio::time::timeout(LIMIT, rx).await.unwrap().unwrap();
            actors.push(actor);
        }
        let (tx, mut started) = tokio::sync::oneshot::channel();
        let queued = Worker(Some(tx)).start_with_backend(Backend::Blocking);
        cleanup.0.push(queued.child_handle());
        assert!(matches!(
            started.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(25), &mut started)
                .await
                .is_err(),
            "a third persistent actor unexpectedly obtained a blocking worker"
        );
        // Stopping an occupied actor releases a worker, allowing the queued actor to start.
        actors[0].child_handle().stop();
        tokio::time::timeout(LIMIT, actors[0].join()).await.unwrap();
        tokio::time::timeout(LIMIT, started).await.unwrap().unwrap();
        queued.request(Ping).await.unwrap();
        for actor in &cleanup.0 {
            actor.stop();
        }
        for actor in &cleanup.0 {
            tokio::time::timeout(LIMIT, actor.wait_exit_async())
                .await
                .unwrap();
        }
    });
}

#[cfg(all(unix, feature = "backend-experiments"))]
#[test]
fn descriptor_limit_is_scoped_to_children() {
    use std::process::{Command, Stdio};
    for mode in ["dedicated", "async", "sharded", "sharded-cold"] {
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "ulimit -n 96; exec \"$@\"", "spawned-fd-test"])
            .arg(std::env::current_exe().unwrap())
            .args(["--exact", "descriptor_limit_child", "--nocapture"])
            .env("SPAWNED_FD_CHILD", mode)
            .env("SPAWNED_EXPERIMENT_RUNTIME", "current")
            .env(
                "SPAWNED_EXPERIMENT_SCHEDULER",
                if mode.starts_with("sharded") {
                    "sharded"
                } else {
                    ""
                },
            )
            .env("SPAWNED_EXPERIMENT_WORKERS", "2")
            .env("SPAWNED_EXPERIMENT_BATCH", "16")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while child.try_wait().unwrap().is_none() {
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                let output = child.wait_with_output().unwrap();
                panic!(
                    "{mode} FD child stalled: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{mode}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[cfg(all(unix, feature = "backend-experiments"))]
#[test]
fn descriptor_limit_child() {
    let Ok(mode) = std::env::var("SPAWNED_FD_CHILD") else {
        return;
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let backend = if mode != "dedicated" {
            Backend::Async
        } else {
            Backend::Thread
        };
        let mut cleanup = Cleanup(Vec::new());
        if mode != "sharded-cold" {
            let seed = Worker(None).start_with_backend(backend);
            cleanup.0.push(seed.child_handle());
            seed.request(Ping).await.unwrap();
        }
        let mut descriptors = Vec::new();
        loop {
            match std::fs::File::open("/dev/null") {
                Ok(file) => descriptors.push(file),
                Err(error) => {
                    assert_eq!(error.raw_os_error(), Some(24));
                    break;
                }
            }
        }
        // Existing shards reuse their reactors. Dedicated-current needs another one.
        let count = if mode == "sharded" || mode == "async" {
            128
        } else {
            1
        };
        for _ in 0..count {
            let actor = Worker(None).start_with_backend(backend);
            cleanup.0.push(actor.child_handle());
            let outcome = actor.request_with_timeout(Ping, LIMIT).await;
            if mode == "sharded" || mode == "async" {
                outcome.unwrap();
            } else {
                assert!(outcome.is_err());
                assert_eq!(
                    tokio::time::timeout(LIMIT, actor.wait_exit())
                        .await
                        .unwrap(),
                    spawned_concurrency::ExitReason::Kill
                );
                assert_eq!(
                    actor.exit_reason(),
                    Some(spawned_concurrency::ExitReason::Kill)
                );
                let child = actor.child_handle();
                assert!(!child.is_alive());
                assert_eq!(
                    child.exit_reason(),
                    Some(spawned_concurrency::ExitReason::Kill)
                );
                assert_eq!(
                    child.wait_exit_blocking(),
                    spawned_concurrency::ExitReason::Kill
                );
            }
        }
        for actor in &cleanup.0 {
            actor.stop();
        }
        for actor in &cleanup.0 {
            tokio::time::timeout(LIMIT, actor.wait_exit_async())
                .await
                .unwrap();
        }
        drop(descriptors);
        if mode == "sharded-cold" {
            // An initialization error must not poison the registry or leave workers behind.
            let recovered = Worker(None).start();
            cleanup.0.push(recovered.child_handle());
            recovered.request(Ping).await.unwrap();
            recovered.child_handle().stop();
            within!(recovered.join()).await;
        }
    });
    if mode.starts_with("sharded") {
        spawned_rt::tasks::experimental::shutdown_sharded().unwrap();
    }
}
