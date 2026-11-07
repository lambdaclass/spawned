//! Default dedicated-thread runtime contracts; run without private runtime overrides.
//! Timer delays below exercise the timer driver, not ordering between test steps.
use spawned_concurrency::{
    message::Message,
    tasks::{Actor, ActorStart, Backend, Context, Handler},
    ChildHandle,
};
use std::{
    net::SocketAddr,
    sync::mpsc,
    thread::{self, ThreadId},
    time::Duration,
};
use tokio::{runtime::RuntimeFlavor, sync::oneshot};

const LIMIT: Duration = Duration::from_secs(5);

struct StopOnDrop(ChildHandle);
impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.stop();
    }
}

async fn timed_helper() -> ThreadId {
    tokio::time::sleep(Duration::from_millis(1)).await;
    thread::current().id()
}

#[derive(Debug)]
struct Observation {
    root: ThreadId,
    worker: ThreadId,
    workers: usize,
    flavor: RuntimeFlavor,
}

struct Probe {
    root: Option<ThreadId>,
    stopped: Option<oneshot::Sender<(ThreadId, ThreadId)>>,
}
impl Probe {
    fn new(stopped: oneshot::Sender<(ThreadId, ThreadId)>) -> Self {
        Self {
            root: None,
            stopped: Some(stopped),
        }
    }
    fn assert_root(&self) -> ThreadId {
        let root = thread::current().id();
        assert_eq!(
            Some(root),
            self.root,
            "actor moved off its dedicated thread"
        );
        root
    }
}
impl Actor for Probe {
    async fn started(&mut self, _: &Context<Self>) {
        let root = thread::current().id();
        self.root = Some(root);
        let worker = tokio::time::timeout(LIMIT, tokio::spawn(timed_helper()))
            .await
            .expect("startup helper stalled")
            .unwrap();
        assert_ne!(root, worker, "helper ran on the dedicated root thread");
        self.assert_root();
    }
    async fn stopped(&mut self, _: &Context<Self>) {
        let root = self.assert_root();
        let worker = tokio::time::timeout(LIMIT, tokio::spawn(timed_helper()))
            .await
            .expect("shutdown helper stalled")
            .unwrap();
        self.assert_root();
        let _ = self.stopped.take().unwrap().send((root, worker));
    }
}

#[derive(Clone, Copy)]
enum Inspect {
    Yield,
    BlockingReceive,
    BlockInPlace,
    SpawnBlocking,
}
impl Message for Inspect {
    type Result = Observation;
}
impl Handler<Inspect> for Probe {
    async fn handle(&mut self, operation: Inspect, _: &Context<Self>) -> Observation {
        let root = self.assert_root();
        let handle = tokio::runtime::Handle::current();
        let workers = handle.metrics().num_workers();
        let flavor = handle.runtime_flavor();
        let worker = match operation {
            Inspect::SpawnBlocking => tokio::time::timeout(
                LIMIT,
                tokio::task::spawn_blocking(|| thread::current().id()),
            )
            .await
            .unwrap()
            .unwrap(),
            Inspect::Yield => {
                tokio::task::yield_now().await;
                tokio::time::timeout(LIMIT, tokio::spawn(timed_helper()))
                    .await
                    .unwrap()
                    .unwrap()
            }
            Inspect::BlockingReceive | Inspect::BlockInPlace => {
                let (tx, rx) = mpsc::channel();
                let helper = tokio::spawn(async move {
                    let worker = timed_helper().await;
                    tx.send(worker).unwrap();
                });
                // The dedicated root synchronously waits for an async helper.
                // A current-thread runtime cannot make progress here, but the
                // bounded receive still lets that regression terminate.
                let worker = if matches!(operation, Inspect::BlockInPlace) {
                    tokio::task::block_in_place(|| rx.recv_timeout(LIMIT).unwrap())
                } else {
                    rx.recv_timeout(LIMIT).unwrap()
                };
                tokio::time::timeout(LIMIT, helper).await.unwrap().unwrap();
                worker
            }
        };
        self.assert_root();
        Observation {
            root,
            worker,
            workers,
            flavor,
        }
    }
}

struct Ping;
impl Message for Ping {
    type Result = u64;
}
struct Child;
impl Actor for Child {}
impl Handler<Ping> for Child {
    async fn handle(&mut self, _: Ping, _: &Context<Self>) -> u64 {
        tokio::time::sleep(Duration::from_millis(1)).await;
        42
    }
}
struct Nested;
impl Message for Nested {
    type Result = u64;
}
impl Handler<Nested> for Probe {
    async fn handle(&mut self, _: Nested, _: &Context<Self>) -> u64 {
        self.assert_root();
        // Default Async startup uses this actor's isolated runtime.
        let child = Child.start();
        let _cleanup = StopOnDrop(child.child_handle());
        let answer = child.request_with_timeout(Ping, LIMIT).await.unwrap();
        child.child_handle().stop();
        tokio::time::timeout(LIMIT, child.join()).await.unwrap();
        self.assert_root();
        answer
    }
}

struct Exchange(SocketAddr);
impl Message for Exchange {
    type Result = u64;
}
impl Handler<Exchange> for Probe {
    async fn handle(&mut self, message: Exchange, _: &Context<Self>) -> u64 {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        self.assert_root();
        let answer = tokio::time::timeout(LIMIT, async {
            let mut stream = tokio::net::TcpStream::connect(message.0).await.unwrap();
            stream.write_all(&42u64.to_le_bytes()).await.unwrap();
            let mut bytes = [0; 8];
            stream.read_exact(&mut bytes).await.unwrap();
            u64::from_le_bytes(bytes)
        })
        .await
        .expect("dedicated runtime I/O stalled");
        self.assert_root();
        answer
    }
}

fn observer() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn assert_observation(observation: &Observation) {
    assert_eq!(observation.flavor, RuntimeFlavor::MultiThread);
    assert_eq!(observation.workers, 1);
    assert_ne!(observation.root, observation.worker);
}

async fn finish(
    actor: &spawned_concurrency::tasks::ActorRef<Probe>,
    stopped: oneshot::Receiver<(ThreadId, ThreadId)>,
) {
    actor.child_handle().stop();
    tokio::time::timeout(LIMIT, actor.join())
        .await
        .expect("dedicated actor did not stop");
    let (root, worker) = tokio::time::timeout(LIMIT, stopped)
        .await
        .expect("stopped helper did not complete")
        .expect("stopped callback failed");
    assert_ne!(root, worker);
}

#[test]
fn dedicated_actor_and_helpers_progress_without_an_ambient_runtime() {
    assert!(tokio::runtime::Handle::try_current().is_err());
    let (stopped, stop_rx) = oneshot::channel();
    let actor = Probe::new(stopped).start_with_backend(Backend::Thread);
    let _cleanup = StopOnDrop(actor.child_handle());
    observer().block_on(async {
        let first = actor
            .request_with_timeout(Inspect::Yield, LIMIT)
            .await
            .unwrap();
        assert_observation(&first);
        for operation in [
            Inspect::BlockingReceive,
            Inspect::BlockInPlace,
            Inspect::SpawnBlocking,
            Inspect::Yield,
        ] {
            let next = actor.request_with_timeout(operation, LIMIT).await.unwrap();
            assert_observation(&next);
            assert_eq!(first.root, next.root);
        }
        assert_eq!(actor.request_with_timeout(Nested, LIMIT).await.unwrap(), 42);
        finish(&actor, stop_rx).await;
    });
}

#[test]
fn dedicated_actor_drives_async_tcp() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (stopped, stop_rx) = oneshot::channel();
    let actor = Probe::new(stopped).start_with_backend(Backend::Thread);
    let _cleanup = StopOnDrop(actor.child_handle());
    observer().block_on(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            tokio::time::timeout(LIMIT, async {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut bytes = [0; 8];
                stream.read_exact(&mut bytes).await.unwrap();
                stream.write_all(&bytes).await.unwrap();
            })
            .await
            .expect("loopback peer stalled");
        });
        assert_eq!(
            actor
                .request_with_timeout(Exchange(address), LIMIT)
                .await
                .unwrap(),
            42
        );
        tokio::time::timeout(LIMIT, server).await.unwrap().unwrap();
        finish(&actor, stop_rx).await;
    });
}

#[test]
fn dedicated_actor_outlives_its_callers_runtime() {
    let (stopped, stop_rx) = oneshot::channel();
    let caller = observer();
    let (actor, root, _cleanup) = caller.block_on(async {
        let actor = Probe::new(stopped).start_with_backend(Backend::Thread);
        let cleanup = StopOnDrop(actor.child_handle());
        let first = actor
            .request_with_timeout(Inspect::Yield, LIMIT)
            .await
            .unwrap();
        assert_observation(&first);
        (actor, first.root, cleanup)
    });
    drop(caller);
    observer().block_on(async {
        let next = actor
            .request_with_timeout(Inspect::BlockingReceive, LIMIT)
            .await
            .unwrap();
        assert_observation(&next);
        assert_eq!(root, next.root);
        finish(&actor, stop_rx).await;
    });
}

#[test]
fn thread_block_on_owns_one_worker_without_an_ambient_runtime() {
    assert!(tokio::runtime::Handle::try_current().is_err());
    let root = thread::current().id();
    spawned_rt::threads::block_on(async {
        let handle = tokio::runtime::Handle::current();
        assert_eq!(handle.runtime_flavor(), RuntimeFlavor::MultiThread);
        assert_eq!(handle.metrics().num_workers(), 1);
        assert_eq!(thread::current().id(), root);
        let worker = tokio::time::timeout(LIMIT, tokio::spawn(timed_helper()))
            .await
            .unwrap()
            .unwrap();
        assert_ne!(root, worker);
        assert_eq!(tokio::task::block_in_place(|| 42), 42);
        assert_eq!(thread::current().id(), root);
    });
}
