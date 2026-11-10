#![cfg(feature = "async")]
//! Abrupt process failure is distinct from a recoverable actor panic.
// Tests that need OS threads are compiled out under spawned_sim.
#![cfg_attr(spawned_sim, allow(unused_imports, dead_code))]
use spawned_concurrency::{
    message::Message,
    tasks::{Actor, ActorStart, Context, Handler},
};
use std::{
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
struct Crash;
impl Message for Crash {
    type Result = ();
}
struct Fixture {
    path: PathBuf,
    abort: bool,
}
impl Actor for Fixture {
    async fn stopped(&mut self, _: &Context<Self>) {
        std::fs::write(self.path.join("stopped"), b"unexpected").unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::write(self.path.join("dropped"), b"unexpected");
    }
}
impl Handler<Crash> for Fixture {
    async fn handle(&mut self, _: Crash, reply: spawned_concurrency::reply::PendingReply<'_, ()>, _: &Context<Self>) {
        {
        std::fs::write(
            self.path.join("effect"),
            b"effect happened before process failure",
        )
        .unwrap();
        if self.abort {
            std::process::abort();
        }
        loop {
            std::thread::park();
        }
    }
        let _ = reply.reply(());
    }
}
#[test]
#[ignore = "only invoked by the isolated parent test"]
fn isolated_process_fixture() {
    let Ok(path) = std::env::var("SPAWNED_PROCESS_FIXTURE") else {
        return;
    };
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        Fixture {
            path: path.into(),
            abort: std::env::var("SPAWNED_PROCESS_MODE").unwrap() == "abort",
        }
        .start()
        .request_with_timeout(Crash, Duration::from_secs(60))
        .await
        .unwrap();
    });
}
struct Cleanup {
    child: Child,
    path: PathBuf,
}
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
fn wait_for_effect(child: &mut Child, path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.join("effect").exists() {
        assert!(
            child.try_wait().unwrap().is_none(),
            "fixture exited before its handler ran"
        );
        assert!(
            Instant::now() < deadline,
            "fixture did not reach its failure window"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}
// Not simulated: kills OS processes and threads.
#[cfg(not(spawned_sim))]
#[test]
#[cfg(unix)]
fn abort_and_external_kill_do_not_promise_stopped_hooks_or_rollback() {
    for mode in ["abort", "kill"] {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "spawned-process-test-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&path).unwrap();
        // Prevent core dumps. Only this owned child is killed; no process-name matching.
        let child = Command::new("sh")
            .args(["-c", "ulimit -c 0; exec \"$@\"", "spawned-fixture"])
            .arg(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "isolated_process_fixture",
                "--nocapture",
            ])
            .env("SPAWNED_PROCESS_FIXTURE", &path)
            .env("SPAWNED_PROCESS_MODE", mode)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut cleanup = Cleanup { child, path };
        if mode == "kill" {
            wait_for_effect(&mut cleanup.child, &cleanup.path);
            cleanup.child.kill().unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(status) = cleanup.child.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "abrupt fixture did not exit");
            std::thread::sleep(Duration::from_millis(5));
        };
        assert!(!status.success());
        assert!(cleanup.path.join("effect").exists());
        assert!(!cleanup.path.join("stopped").exists());
        assert!(!cleanup.path.join("dropped").exists());
    }
}
