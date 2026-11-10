//! Shared test actors. Gates control real handlers; they do not replace the runtime.
#![allow(dead_code)]
use futures::channel::oneshot;
use spawned_concurrency::{
    message::Message, ActorError, ChildHandle, Down, ExitReason, MonitorRef,
};
use std::{
    sync::{mpsc, Arc, Mutex},
    time::Duration,
};
pub const LIMIT: Duration = Duration::from_secs(5);
#[derive(Debug, Clone, Copy)]
pub enum Backend {
    #[cfg(feature = "async")]
    Async,
    #[cfg(feature = "async")]
    Blocking,
    #[cfg(feature = "async")]
    Thread,
    Native,
}
pub const BACKENDS: &[Backend] = &[
    #[cfg(feature = "async")]
    Backend::Async,
    #[cfg(feature = "async")]
    Backend::Blocking,
    #[cfg(feature = "async")]
    Backend::Thread,
    Backend::Native,
];
pub fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
}
pub struct Gate {
    entered: mpsc::Sender<()>,
    release: oneshot::Receiver<()>,
}
pub struct Control {
    entered: mpsc::Receiver<()>,
    release: Option<oneshot::Sender<()>>,
}
pub fn gate() -> (Gate, Control) {
    let (entered, received) = mpsc::channel();
    let (release, wait) = oneshot::channel();
    (
        Gate {
            entered,
            release: wait,
        },
        Control {
            entered: received,
            release: Some(release),
        },
    )
}
impl Control {
    pub fn entered(&self) {
        self.entered
            .recv_timeout(LIMIT)
            .expect("handler never reached controlled window");
    }
    pub fn release(&mut self) {
        if let Some(sender) = self.release.take() {
            let _ = sender.send(());
        }
    }
}
impl Gate {
    async fn wait(self) {
        let _ = self.entered.send(());
        let _ = self.release.await;
    }
}
pub struct Payload {
    pub bytes: Vec<u8>,
    pub dropped: Arc<std::sync::atomic::AtomicUsize>,
}
impl Drop for Payload {
    fn drop(&mut self) {
        self.dropped
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}
pub enum Command {
    Append {
        id: u64,
        before: Option<Gate>,
        after: Option<Gate>,
        panic_after: bool,
    },
    Payload {
        id: u64,
        payload: Payload,
    },
    Read,
    Panic,
}
impl Command {
    pub fn append(id: u64) -> Self {
        Self::Append {
            id,
            before: None,
            after: None,
            panic_after: false,
        }
    }
}
impl Message for Command {
    type Result = Reply;
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reply {
    Appended { id: u64, length: usize },
    Values(Vec<u64>),
}
#[derive(Default)]
pub struct Worker {
    pub values: Vec<u64>,
    pub started: Option<Gate>,
    pub stopped: Option<Gate>,
    pub panic_stopped: bool,
    pub down: Option<mpsc::Sender<Down>>,
    pub effects: Option<Arc<Mutex<Vec<u64>>>>,
}
impl Worker {
    fn execute(&mut self, id: u64) -> Reply {
        self.values.push(id);
        if let Some(effects) = &self.effects {
            effects.lock().unwrap().push(id);
        }
        Reply::Appended {
            id,
            length: self.values.len(),
        }
    }
}
#[cfg(feature = "async")]
pub struct AsyncWorker(pub Worker);
#[cfg(feature = "async")]
impl spawned_concurrency::tasks::Actor for AsyncWorker {
    async fn started(&mut self, _: &spawned_concurrency::tasks::Context<Self>) {
        if let Some(gate) = self.0.started.take() {
            gate.wait().await;
        }
    }
    async fn stopped(&mut self, _: &spawned_concurrency::tasks::Context<Self>) {
        if let Some(gate) = self.0.stopped.take() {
            gate.wait().await;
        }
        assert!(!self.0.panic_stopped, "controlled cleanup panic");
    }
}
#[cfg(feature = "async")]
impl spawned_concurrency::tasks::Handler<Command> for AsyncWorker {
    async fn handle(
        &mut self,
        command: Command,
        _: &spawned_concurrency::tasks::Context<Self>,
    ) -> Reply {
        match command {
            Command::Payload { id, payload } => {
                assert_eq!(payload.bytes[0], (id % 251) as u8);
                self.0.execute(id)
            }
            Command::Read => Reply::Values(self.0.values.clone()),
            Command::Panic => panic!("controlled handler panic"),
            Command::Append {
                id,
                before,
                after,
                panic_after,
            } => {
                if let Some(gate) = before {
                    gate.wait().await;
                }
                let result = self.0.execute(id);
                if let Some(gate) = after {
                    gate.wait().await;
                }
                assert!(!panic_after, "controlled panic after effect");
                result
            }
        }
    }
}
#[cfg(feature = "async")]
impl spawned_concurrency::tasks::Handler<Down> for AsyncWorker {
    async fn handle(&mut self, down: Down, _: &spawned_concurrency::tasks::Context<Self>) {
        if let Some(sender) = &self.0.down {
            let _ = sender.send(down);
        }
    }
}
pub struct NativeWorker(pub Worker);
impl spawned_concurrency::threads::Actor for NativeWorker {
    fn started(&mut self, _: &spawned_concurrency::threads::Context<Self>) {
        if let Some(gate) = self.0.started.take() {
            futures::executor::block_on(gate.wait());
        }
    }
    fn stopped(&mut self, _: &spawned_concurrency::threads::Context<Self>) {
        if let Some(gate) = self.0.stopped.take() {
            futures::executor::block_on(gate.wait());
        }
        assert!(!self.0.panic_stopped, "controlled cleanup panic");
    }
}
impl spawned_concurrency::threads::Handler<Command> for NativeWorker {
    fn handle(
        &mut self,
        command: Command,
        _: &spawned_concurrency::threads::Context<Self>,
    ) -> Reply {
        match command {
            Command::Payload { id, payload } => {
                assert_eq!(payload.bytes[0], (id % 251) as u8);
                self.0.execute(id)
            }
            Command::Read => Reply::Values(self.0.values.clone()),
            Command::Panic => panic!("controlled handler panic"),
            Command::Append {
                id,
                before,
                after,
                panic_after,
            } => {
                if let Some(gate) = before {
                    futures::executor::block_on(gate.wait());
                }
                let result = self.0.execute(id);
                if let Some(gate) = after {
                    futures::executor::block_on(gate.wait());
                }
                assert!(!panic_after, "controlled panic after effect");
                result
            }
        }
    }
}
impl spawned_concurrency::threads::Handler<Down> for NativeWorker {
    fn handle(&mut self, down: Down, _: &spawned_concurrency::threads::Context<Self>) {
        if let Some(sender) = &self.0.down {
            let _ = sender.send(down);
        }
    }
}
#[derive(Clone)]
pub enum Handle {
    #[cfg(feature = "async")]
    Async(spawned_concurrency::tasks::ActorRef<AsyncWorker>),
    Native(spawned_concurrency::threads::ActorRef<NativeWorker>),
}
pub enum Response {
    #[cfg(feature = "async")]
    Async(tokio::sync::oneshot::Receiver<Reply>),
    Native(mpsc::Receiver<Reply>),
}
impl Response {
    pub fn wait(
        self,
        _rt: &tokio::runtime::Runtime,
        duration: Duration,
    ) -> Result<Reply, ActorError> {
        match self {
            #[cfg(feature = "async")]
            Self::Async(receiver) => _rt.block_on(
                spawned_concurrency::Response::from_with_timeout(Ok(receiver), duration),
            ),
            Self::Native(receiver) => receiver.recv_timeout(duration).map_err(|e| match e {
                mpsc::RecvTimeoutError::Timeout => ActorError::RequestTimeout,
                mpsc::RecvTimeoutError::Disconnected => ActorError::ActorStopped,
            }),
        }
    }
}
impl Handle {
    pub fn start(backend: Backend, worker: Worker) -> Self {
        #[cfg(feature = "async")]
        use spawned_concurrency::tasks::ActorStart as _;
        use spawned_concurrency::threads::ActorStart as _;
        match backend {
            Backend::Native => Self::Native(NativeWorker(worker).start()),
            #[cfg(feature = "async")]
            other => Self::Async(AsyncWorker(worker).start_with_backend(match other {
                Backend::Async => spawned_concurrency::tasks::Backend::Async,
                Backend::Blocking => spawned_concurrency::tasks::Backend::Blocking,
                Backend::Thread => spawned_concurrency::tasks::Backend::Thread,
                Backend::Native => unreachable!(),
            })),
        }
    }
    pub fn child(&self) -> ChildHandle {
        match self {
            #[cfg(feature = "async")]
            Self::Async(actor) => actor.child_handle(),
            Self::Native(actor) => actor.child_handle(),
        }
    }
    pub fn raw(&self, command: Command) -> Result<Response, ActorError> {
        match self {
            #[cfg(feature = "async")]
            Self::Async(actor) => actor.request_raw(command).map(Response::Async),
            Self::Native(actor) => actor.request_raw(command).map(Response::Native),
        }
    }
    pub fn request(
        &self,
        _rt: &tokio::runtime::Runtime,
        command: Command,
    ) -> Result<Reply, ActorError> {
        self.raw(command)?.wait(_rt, LIMIT)
    }
    pub fn send(&self, command: Command) -> Result<(), ActorError> {
        match self {
            #[cfg(feature = "async")]
            Self::Async(actor) => actor.send(command),
            Self::Native(actor) => actor.send(command),
        }
    }
    pub fn stop(&self) {
        self.child().stop();
    }
    pub fn wait(&self, _rt: &tokio::runtime::Runtime) -> ExitReason {
        #[cfg(not(feature = "async"))]
        return wait_native(&self.child());
        #[cfg(feature = "async")]
        _rt.block_on(async {
            tokio::time::timeout(LIMIT, self.child().wait_exit_async())
                .await
                .expect("actor did not finish")
        })
    }
    pub fn link(&self, target: &ChildHandle) {
        match self {
            #[cfg(feature = "async")]
            Self::Async(actor) => actor.context().link(target),
            Self::Native(actor) => actor.context().link(target),
        }
    }
    pub fn monitor(&self, target: &ChildHandle) -> MonitorRef {
        match self {
            #[cfg(feature = "async")]
            Self::Async(actor) => actor.context().monitor(target),
            Self::Native(actor) => actor.context().monitor(target),
        }
    }
    pub fn demonitor(&self, id: MonitorRef) {
        match self {
            #[cfg(feature = "async")]
            Self::Async(actor) => actor.context().demonitor(id),
            Self::Native(actor) => actor.context().demonitor(id),
        }
    }
}
#[derive(Default)]
pub struct Cleanup(pub Vec<ChildHandle>);
impl Cleanup {
    pub fn track(&mut self, handle: &Handle) {
        self.0.push(handle.child());
    }
}
impl Drop for Cleanup {
    fn drop(&mut self) {
        for child in &self.0 {
            child.stop();
        }
    }
}

#[cfg(not(feature = "async"))]
pub fn wait_native(child: &ChildHandle) -> ExitReason {
    let until = std::time::Instant::now() + LIMIT;
    loop {
        if let Some(reason) = child.exit_reason() {
            return reason;
        }
        assert!(
            std::time::Instant::now() < until,
            "native actor did not finish"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}
