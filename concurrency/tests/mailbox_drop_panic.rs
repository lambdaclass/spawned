//! Run abort-sensitive checks in a subprocess so CI survives a regression.
#![cfg(feature = "async")]
use spawned_concurrency::{
    message::Message,
    system::ActorSystem,
    tasks::{Actor, ActorStart, Backend, Context, Handler},
    Down, Exit, ExitReason,
};
use std::{
    io::Read,
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, oneshot};
const LIMIT: Duration = Duration::from_secs(10);

struct Hold(oneshot::Sender<()>, oneshot::Receiver<()>, bool);
impl Message for Hold {
    type Result = ();
}
struct Bomb(Arc<AtomicUsize>);
impl Message for Bomb {
    type Result = ();
}
impl Drop for Bomb {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("queued message destructor");
    }
}
struct Victim(Arc<AtomicUsize>);
impl Actor for Victim {}
impl Handler<Hold> for Victim {
    async fn handle(&mut self, msg: Hold, _: &Context<Self>) {
        msg.0.send(()).unwrap();
        let _ = msg.1.await;
        assert!(!msg.2, "original handler failure");
    }
}
impl Handler<Bomb> for Victim {
    async fn handle(&mut self, _: Bomb, _: &Context<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
struct Ping;
impl Message for Ping {
    type Result = ();
}
struct Observer(mpsc::UnboundedSender<(bool, ExitReason)>);
impl Actor for Observer {
    async fn exit_received(&mut self, exit: Exit, _: &Context<Self>) {
        self.0.send((true, exit.reason)).unwrap();
    }
}
impl Handler<Down> for Observer {
    async fn handle(&mut self, down: Down, _: &Context<Self>) {
        self.0.send((false, down.reason)).unwrap();
    }
}
impl Handler<Ping> for Observer {
    async fn handle(&mut self, _: Ping, _: &Context<Self>) {}
}

#[test]
fn queued_destructor_panics_do_not_abort_or_kill_trapping_observers() {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "mailbox_drop_child", "--ignored", "--nocapture"])
        .env("SPAWNED_MAILBOX_DROP_CHILD", "1")
        .env("RUST_BACKTRACE", "0")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut output = String::new();
        stderr.read_to_string(&mut output).unwrap();
        output
    });
    let until = Instant::now() + Duration::from_secs(120);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= until {
            child.kill().unwrap();
            let _ = child.wait();
            panic!("mailbox panic child timed out: {}", reader.join().unwrap());
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let errors = reader.join().unwrap();
    assert!(
        status.success(),
        "mailbox panic child failed {status}: {errors}"
    );
    let mut output = String::new();
    stdout.read_to_string(&mut output).unwrap();
    assert!(
        output.contains("validated 30 queued panic cases"),
        "child did not exercise the expected matrix: {output}"
    );
}

#[test]
#[ignore = "subprocess helper, exercised by the enabled parent regression"]
fn mailbox_drop_child() {
    if std::env::var_os("SPAWNED_MAILBOX_DROP_CHILD").is_none() {
        return;
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        // Async, blocking, dedicated thread, managed async, managed native roots.
        for placement in 0..5 {
            for bombs in [1, 2, 16] {
                for handler_panics in [false, true] {
                    let system = ActorSystem::new().unwrap();
                    let group = match placement {
                        3 => Some(system.async_group("panic-async", 1).unwrap()),
                        4 => Some(system.thread_group("panic-native", 1).unwrap()),
                        _ => None,
                    };
                    let handled = Arc::new(AtomicUsize::new(0));
                    let dropped = Arc::new(AtomicUsize::new(0));
                    let victim = match &group {
                        Some(group) => Victim(handled.clone()).start_on(group).unwrap(),
                        None => Victim(handled.clone()).start_with_backend(match placement {
                            0 => Backend::Async,
                            1 => Backend::Blocking,
                            _ => Backend::Thread,
                        }),
                    };
                    let (send, mut events) = mpsc::unbounded_channel();
                    let observer = Observer(send).start();
                    let ctx = observer.context();
                    ctx.trap_exit(true);
                    ctx.link(&victim.child_handle());
                    let _monitor = ctx.monitor(&victim.child_handle());
                    let (entered, arrival) = oneshot::channel();
                    let (release, gate) = oneshot::channel();
                    victim.send(Hold(entered, gate, handler_panics)).unwrap();
                    tokio::time::timeout(LIMIT, arrival).await.unwrap().unwrap();
                    let replies: Vec<_> = (0..bombs)
                        .map(|_| victim.request_raw(Bomb(dropped.clone())).unwrap())
                        .collect();
                    victim.context().stop();
                    release.send(()).unwrap();
                    let reason = tokio::time::timeout(LIMIT, victim.wait_exit())
                        .await
                        .unwrap();
                    if handler_panics {
                        assert_eq!(
                            reason,
                            ExitReason::Panic("panic in handler: original handler failure".into())
                        );
                    } else {
                        assert_eq!(
                            reason,
                            ExitReason::Panic("unexpected framework panic".into())
                        );
                    }
                    let first = tokio::time::timeout(LIMIT, events.recv())
                        .await
                        .unwrap()
                        .unwrap();
                    let second = tokio::time::timeout(LIMIT, events.recv())
                        .await
                        .unwrap()
                        .unwrap();
                    assert_ne!(first.0, second.0, "expected one Exit and one Down");
                    assert_eq!(first.1, reason);
                    assert_eq!(second.1, reason);
                    observer.request(Ping).await.unwrap(); // trapping observer survives
                    assert_eq!(dropped.load(Ordering::SeqCst), bombs);
                    assert_eq!(handled.load(Ordering::SeqCst), 0);
                    for reply in replies {
                        assert!(tokio::time::timeout(LIMIT, reply).await.unwrap().is_err());
                    }
                    observer.context().stop();
                    tokio::time::timeout(LIMIT, observer.join()).await.unwrap();
                    assert!(
                        matches!(
                            events.try_recv(),
                            Err(mpsc::error::TryRecvError::Disconnected)
                        ),
                        "extra notification after observer cleanup"
                    );
                    assert!(system.shutdown(LIMIT).await.complete);
                }
            }
        }
        #[cfg(feature = "metrics")]
        {
            let snapshot = spawned_concurrency::metrics::snapshot();
            assert_eq!(snapshot.actors_active, 0);
            assert_eq!(snapshot.handlers_active, 0);
            assert_eq!(snapshot.mailbox_depth, 0);
        }
    });
    runtime.shutdown_timeout(LIMIT);
    println!("validated 30 queued panic cases");
}
