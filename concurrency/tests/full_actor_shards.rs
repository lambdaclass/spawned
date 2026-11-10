#![cfg(feature = "async")]
#![cfg(feature = "backend-experiments")]
//! Same actor semantics on default Tokio, batched Tokio and the opt-in shard pool.
use spawned_concurrency::{
    message::Message,
    tasks::{Actor, ActorStart, Context, Handler},
    ChildHandle, ExitReason,
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
const LIMIT: Duration = Duration::from_secs(5);
struct Cleanup(Vec<ChildHandle>);
impl Drop for Cleanup {
    fn drop(&mut self) {
        for actor in &self.0 {
            actor.stop();
        }
    }
}
struct Worker(Arc<Mutex<Vec<&'static str>>>);
impl Actor for Worker {
    async fn started(&mut self, _: &Context<Self>) {
        self.0.lock().unwrap().push("started");
    }
    async fn stopped(&mut self, _: &Context<Self>) {
        self.0.lock().unwrap().push("stopped");
    }
}
struct Thread;
impl Message for Thread {
    type Result = String;
}
impl Handler<Thread> for Worker {
    async fn handle(
        &mut self,
        _: Thread,
        reply: spawned_concurrency::reply::PendingReply<'_, String>,
        _: &Context<Self>,
    ) {
        let __reply_value: String = {
            std::thread::current()
                .name()
                .unwrap_or("unnamed")
                .to_owned()
        };
        let _ = reply.reply(__reply_value);
    }
}
struct Hold {
    started: tokio::sync::oneshot::Sender<()>,
    release: tokio::sync::oneshot::Receiver<()>,
}
impl Message for Hold {
    type Result = u64;
}
impl Handler<Hold> for Worker {
    async fn handle(
        &mut self,
        msg: Hold,
        reply: spawned_concurrency::reply::PendingReply<'_, u64>,
        _: &Context<Self>,
    ) {
        let __reply_value: u64 = {
            let _ = msg.started.send(());
            msg.release.await.unwrap();
            42
        };
        let _ = reply.reply(__reply_value);
    }
}
struct Nested(std::net::SocketAddr);
impl Message for Nested {
    type Result = u64;
}
impl Handler<Nested> for Worker {
    async fn handle(
        &mut self,
        msg: Nested,
        reply: spawned_concurrency::reply::PendingReply<'_, u64>,
        _: &Context<Self>,
    ) {
        let __reply_value: u64 = {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let child = Worker(Default::default()).start();
            let _ = child.request(Thread).await.unwrap();
            child.child_handle().stop();
            child.join().await;
            let helper = tokio::spawn(async {
                tokio::time::sleep(Duration::from_millis(1)).await;
                42u64
            });
            let mut socket = tokio::net::TcpStream::connect(msg.0).await.unwrap();
            socket
                .write_all(&helper.await.unwrap().to_le_bytes())
                .await
                .unwrap();
            let mut reply = [0; 8];
            socket.read_exact(&mut reply).await.unwrap();
            u64::from_le_bytes(reply)
        };
        let _ = reply.reply(__reply_value);
    }
}
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
}
#[test]
fn nested_spawn_helpers_timers_and_tcp_use_the_actor_reactor() {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket.set_read_timeout(Some(LIMIT)).unwrap();
        let mut data = [0; 8];
        socket.read_exact(&mut data).unwrap();
        socket.write_all(&data).unwrap();
    });
    runtime().block_on(async {
        let events = Arc::new(Mutex::new(Vec::new()));
        let actor = Worker(events.clone()).start();
        let _cleanup = Cleanup(vec![actor.child_handle()]);
        assert_eq!(
            actor
                .request_with_timeout(Nested(addr), LIMIT)
                .await
                .unwrap(),
            42
        );
        actor.child_handle().stop();
        assert_eq!(
            tokio::time::timeout(LIMIT, actor.wait_exit())
                .await
                .unwrap(),
            ExitReason::Normal
        );
        assert_eq!(*events.lock().unwrap(), ["started", "stopped"]);
    });
    server.join().unwrap();
}
#[test]
fn stop_finishes_in_flight_handler_and_rejects_queued_work() {
    runtime().block_on(async {
        let actor = Worker(Default::default()).start();
        let _cleanup = Cleanup(vec![actor.child_handle()]);
        let (started, rx) = tokio::sync::oneshot::channel();
        let (release, gate) = tokio::sync::oneshot::channel();
        let reply = actor
            .request_raw(Hold {
                started,
                release: gate,
            })
            .unwrap();
        tokio::time::timeout(LIMIT, rx).await.unwrap().unwrap();
        let queued = actor.request_raw(Thread).unwrap();
        actor.child_handle().stop();
        assert!(actor.exit_reason().is_none());
        release.send(()).unwrap();
        assert_eq!(
            tokio::time::timeout(LIMIT, reply).await.unwrap().unwrap(),
            42
        );
        assert_eq!(
            tokio::time::timeout(LIMIT, actor.wait_exit())
                .await
                .unwrap(),
            ExitReason::Normal
        );
        assert!(tokio::time::timeout(LIMIT, queued).await.unwrap().is_err());
    });
}
#[test]
fn suspended_actor_does_not_block_another_actor_on_its_shard() {
    if std::env::var("SPAWNED_EXPERIMENT_SCHEDULER").as_deref() != Ok("sharded") {
        return;
    }
    runtime().block_on(async {
        let count: usize = std::env::var("SPAWNED_EXPERIMENT_WORKERS")
            .unwrap_or_else(|_| "2".into())
            .parse()
            .unwrap();
        let mut cleanup = Cleanup(Vec::new());
        let mut actors = Vec::new();
        let mut names = Vec::new();
        for _ in 0..=count {
            let actor = Worker(Default::default()).start();
            cleanup.0.push(actor.child_handle());
            let name = actor.request_with_timeout(Thread, LIMIT).await.unwrap();
            assert!(name.starts_with("spawned-shard-"));
            names.push(name);
            actors.push(actor);
        }
        let (a, b) = (0..names.len())
            .find_map(|a| {
                ((a + 1)..names.len())
                    .find(|&b| names[a] == names[b])
                    .map(|b| (a, b))
            })
            .unwrap();
        let (started, rx) = tokio::sync::oneshot::channel();
        let (release, gate) = tokio::sync::oneshot::channel();
        let held = actors[a]
            .request_raw(Hold {
                started,
                release: gate,
            })
            .unwrap();
        tokio::time::timeout(LIMIT, rx).await.unwrap().unwrap();
        assert_eq!(
            actors[b].request_with_timeout(Thread, LIMIT).await.unwrap(),
            names[a]
        );
        release.send(()).unwrap();
        assert_eq!(
            tokio::time::timeout(LIMIT, held).await.unwrap().unwrap(),
            42
        );
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
#[test]
fn actors_outlive_their_caller_runtime() {
    let scheduler = std::env::var("SPAWNED_EXPERIMENT_SCHEDULER").unwrap_or_default();
    if scheduler != "sharded"
        && spawned_rt::tasks::experimental::dispatcher::policy_for_scheduler(&scheduler).is_none()
    {
        return;
    }
    let rt = runtime();
    let actor = rt.block_on(async {
        let actor = Worker(Default::default()).start();
        actor.request(Thread).await.unwrap();
        actor
    });
    let _cleanup = Cleanup(vec![actor.child_handle()]);
    drop(rt);
    runtime().block_on(async {
        let name = actor.request_with_timeout(Thread, LIMIT).await.unwrap();
        assert!(name.starts_with("spawned-shard-") || name.starts_with("actor-worker-"));
        actor.child_handle().stop();
        tokio::time::timeout(LIMIT, actor.join()).await.unwrap();
    });
}

struct Freeze {
    started: tokio::sync::oneshot::Sender<()>,
    release: std::sync::mpsc::Receiver<()>,
}
impl Message for Freeze {
    type Result = ();
}
impl Handler<Freeze> for Worker {
    async fn handle(
        &mut self,
        msg: Freeze,
        reply: spawned_concurrency::reply::PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
        {
            let _ = msg.started.send(());
            msg.release.recv_timeout(LIMIT).unwrap();
        }
        let _ = reply.reply(());
    }
}
#[test]
fn blocking_handler_stalls_its_shard_while_another_shard_progresses() {
    if std::env::var("SPAWNED_EXPERIMENT_SCHEDULER").as_deref() != Ok("sharded") {
        return;
    }
    runtime().block_on(async {
        let count: usize = std::env::var("SPAWNED_EXPERIMENT_WORKERS")
            .unwrap_or_else(|_| "2".into())
            .parse()
            .unwrap();
        let mut cleanup = Cleanup(Vec::new());
        let mut actors = Vec::new();
        let mut names = Vec::new();
        for _ in 0..=count {
            let actor = Worker(Default::default()).start();
            cleanup.0.push(actor.child_handle());
            names.push(actor.request_with_timeout(Thread, LIMIT).await.unwrap());
            actors.push(actor);
        }
        let (a, b) = (0..names.len())
            .find_map(|a| {
                ((a + 1)..names.len())
                    .find(|&b| names[a] == names[b])
                    .map(|b| (a, b))
            })
            .unwrap();
        let (started, rx) = tokio::sync::oneshot::channel();
        let (release, gate) = std::sync::mpsc::channel();
        let frozen = actors[a]
            .request_raw(Freeze {
                started,
                release: gate,
            })
            .unwrap();
        tokio::time::timeout(LIMIT, rx).await.unwrap().unwrap();
        let mut queued = actors[b].request_raw(Thread).unwrap();
        assert!(matches!(
            queued.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        if let Some(other) = (0..actors.len()).find(|&id| names[id] != names[a]) {
            assert_eq!(
                actors[other]
                    .request_with_timeout(Thread, LIMIT)
                    .await
                    .unwrap(),
                names[other]
            );
        }
        release.send(()).unwrap();
        tokio::time::timeout(LIMIT, frozen).await.unwrap().unwrap();
        assert_eq!(
            tokio::time::timeout(LIMIT, queued).await.unwrap().unwrap(),
            names[a]
        );
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

struct Tick;
impl Message for Tick {
    type Result = u64;
}
impl Handler<Tick> for Worker {
    async fn handle(
        &mut self,
        _: Tick,
        reply: spawned_concurrency::reply::PendingReply<'_, u64>,
        _: &Context<Self>,
    ) {
        let __reply_value: u64 = {
            tokio::time::sleep(Duration::from_millis(1)).await;
            42
        };
        let _ = reply.reply(__reply_value);
    }
}
struct SpawnBlockingChild;
impl Message for SpawnBlockingChild {
    type Result = spawned_concurrency::tasks::ActorRef<Worker>;
}
impl Handler<SpawnBlockingChild> for Worker {
    async fn handle(
        &mut self,
        _: SpawnBlockingChild,
        reply: spawned_concurrency::reply::PendingReply<
            '_,
            spawned_concurrency::tasks::ActorRef<Worker>,
        >,
        _: &Context<Self>,
    ) {
        let __reply_value: spawned_concurrency::tasks::ActorRef<Worker> = {
            Worker(Default::default())
                .start_with_backend(spawned_concurrency::tasks::Backend::Blocking)
        };
        let _ = reply.reply(__reply_value);
    }
}
struct Helper(tokio::sync::oneshot::Receiver<()>);
impl Message for Helper {
    type Result = tokio::task::JoinHandle<u64>;
}
impl Handler<Helper> for Worker {
    async fn handle(
        &mut self,
        msg: Helper,
        reply: spawned_concurrency::reply::PendingReply<'_, tokio::task::JoinHandle<u64>>,
        _: &Context<Self>,
    ) {
        let __reply_value: tokio::task::JoinHandle<u64> = {
            tokio::spawn(async move {
                msg.0.await.unwrap();
                tokio::time::sleep(Duration::from_millis(1)).await;
                42
            })
        };
        let _ = reply.reply(__reply_value);
    }
}
#[test]
fn detached_helper_and_blocking_child_outlive_parent_stop() {
    runtime().block_on(async {
        let actor = Worker(Default::default()).start();
        let mut cleanup = Cleanup(vec![actor.child_handle()]);
        let child = actor
            .request_with_timeout(SpawnBlockingChild, LIMIT)
            .await
            .unwrap();
        cleanup.0.push(child.child_handle());
        let (release, gate) = tokio::sync::oneshot::channel();
        let helper = actor
            .request_with_timeout(Helper(gate), LIMIT)
            .await
            .unwrap();
        actor.child_handle().stop();
        tokio::time::timeout(LIMIT, actor.join()).await.unwrap();
        release.send(()).unwrap();
        assert_eq!(
            tokio::time::timeout(LIMIT, helper).await.unwrap().unwrap(),
            42
        );
        assert_eq!(child.request_with_timeout(Tick, LIMIT).await.unwrap(), 42);
        child.child_handle().stop();
        tokio::time::timeout(LIMIT, child.join()).await.unwrap();
    });
}

struct InstallMonitor {
    observer: Context<Worker>,
    target: ChildHandle,
}
impl Message for InstallMonitor {
    type Result = ();
}
impl Handler<InstallMonitor> for Worker {
    async fn handle(
        &mut self,
        msg: InstallMonitor,
        reply: spawned_concurrency::reply::PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
        {
            msg.observer.monitor(&msg.target);
        }
        let _ = reply.reply(());
    }
}
impl Handler<spawned_concurrency::Down> for Worker {
    async fn handle(
        &mut self,
        _: spawned_concurrency::Down,
        reply: spawned_concurrency::reply::PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
        {
            self.0.lock().unwrap().push("down");
        }
        let _ = reply.reply(());
    }
}
struct Events;
impl Message for Events {
    type Result = Vec<&'static str>;
}
impl Handler<Events> for Worker {
    async fn handle(
        &mut self,
        _: Events,
        reply: spawned_concurrency::reply::PendingReply<'_, Vec<&'static str>>,
        _: &Context<Self>,
    ) {
        let __reply_value: Vec<&'static str> = { self.0.lock().unwrap().clone() };
        let _ = reply.reply(__reply_value);
    }
}
#[test]
fn monitor_helper_survives_the_last_sharded_actor_stopping() {
    runtime().block_on(async {
        use spawned_concurrency::tasks::Backend;
        let installer = Worker(Default::default()).start();
        let observer = Worker(Default::default()).start_with_backend(Backend::Thread);
        let target = Worker(Default::default()).start_with_backend(Backend::Thread);
        let _cleanup = Cleanup(vec![
            installer.child_handle(),
            observer.child_handle(),
            target.child_handle(),
        ]);
        installer
            .request_with_timeout(
                InstallMonitor {
                    observer: observer.context(),
                    target: target.child_handle(),
                },
                LIMIT,
            )
            .await
            .unwrap();
        installer.child_handle().stop();
        tokio::time::timeout(LIMIT, installer.join()).await.unwrap();
        target.child_handle().stop();
        tokio::time::timeout(LIMIT, target.join()).await.unwrap();
        tokio::time::timeout(LIMIT, async {
            loop {
                let events = observer.request_with_timeout(Events, LIMIT).await.unwrap();
                if events.contains(&"down") {
                    assert_eq!(events.iter().filter(|&&v| v == "down").count(), 1);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        observer.child_handle().stop();
        tokio::time::timeout(LIMIT, observer.join()).await.unwrap();
    });
}
