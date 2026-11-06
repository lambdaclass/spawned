//! Experimental design driver; see benchmarks/EXPERIMENTS.md.
#[path = "support/actor_io.rs"]
mod actor_io;
#[path = "support/args.rs"]
mod cli;
#[path = "support/ready_gate.rs"]
mod ready_gate;
use spawned_concurrency::{message::Message, tasks, threads, ChildHandle};
use std::{
    alloc::{GlobalAlloc, Layout, System},
    hint::black_box,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc, Barrier,
    },
    time::{Duration, Instant},
};

struct CountingAllocator;
static COUNT: AtomicBool = AtomicBool::new(false);
static ALLOCS: AtomicU64 = AtomicU64::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if COUNT.load(Ordering::Relaxed) && !ptr.is_null() {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        }
        ptr
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if COUNT.load(Ordering::Relaxed) && !ptr.is_null() {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        }
        ptr
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let new = unsafe { System.realloc(ptr, layout, size) };
        if COUNT.load(Ordering::Relaxed) && !new.is_null() {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(size as u64, Ordering::Relaxed);
        }
        new
    }
}
#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

struct Work {
    id: u64,
    enqueued: Instant,
    payload: Arc<[u8]>,
    iterations: u32,
    timing: bool,
}
struct Ack {
    id: u64,
    completed: Instant,
    queue_ns: u64,
    handler_ns: u64,
    checksum: u64,
}
impl Message for Work {
    type Result = Ack;
}
fn process(work: Work) -> Ack {
    let start = if work.timing {
        Instant::now()
    } else {
        work.enqueued
    };
    process_at(work, start)
}
fn process_at(work: Work, start: Instant) -> Ack {
    let mut value = work.id;
    for byte in work.payload.iter() {
        value = value.wrapping_mul(31).wrapping_add(*byte as u64);
    }
    for _ in 0..work.iterations {
        value = black_box(value.wrapping_mul(6364136223846793005).wrapping_add(1));
    }
    let completed = Instant::now();
    Ack {
        id: work.id,
        completed,
        queue_ns: if work.timing {
            start.duration_since(work.enqueued).as_nanos() as u64
        } else {
            0
        },
        handler_ns: if work.timing {
            completed.duration_since(start).as_nanos() as u64
        } else {
            0
        },
        checksum: value,
    }
}
struct Sink {
    work: String,
    wait: Duration,
    address: Option<std::net::SocketAddr>,
    socket: Option<tokio::net::TcpStream>,
}
impl Sink {
    fn new(config: &Config, address: Option<std::net::SocketAddr>) -> Self {
        Self {
            work: config.work.clone(),
            wait: Duration::from_micros(config.wait_us),
            address,
            socket: None,
        }
    }
}
impl tasks::Actor for Sink {}
impl tasks::Handler<Work> for Sink {
    async fn handle(&mut self, work: Work, _: &tasks::Context<Self>) -> Ack {
        let start = if work.timing {
            Instant::now()
        } else {
            work.enqueued
        };
        match self.work.as_str() {
            "timer" => tokio::time::sleep(self.wait).await,
            "blocking" => std::thread::sleep(self.wait),
            "tcp" => {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                if self.socket.is_none() {
                    let socket = tokio::net::TcpStream::connect(self.address.unwrap())
                        .await
                        .unwrap();
                    socket.set_nodelay(true).unwrap();
                    self.socket = Some(socket);
                }
                let socket = self.socket.as_mut().unwrap();
                let mut frame = [0u8; 64];
                frame[..8].copy_from_slice(&work.id.to_le_bytes());
                let expected = frame;
                socket.write_all(&frame).await.unwrap();
                socket.read_exact(&mut frame).await.unwrap();
                assert_eq!(frame, expected);
            }
            _ => {}
        }
        process_at(work, start)
    }
}
impl threads::Actor for Sink {}
impl threads::Handler<Work> for Sink {
    fn handle(&mut self, work: Work, _: &threads::Context<Self>) -> Ack {
        process(work)
    }
}
enum ReplySender {
    Std(mpsc::Sender<Ack>),
    Bounded(mpsc::SyncSender<Ack>),
    Tokio(tokio::sync::oneshot::Sender<Ack>),
}
impl ReplySender {
    fn send(self, ack: Ack) {
        match self {
            Self::Std(tx) => {
                let _ = tx.send(ack);
            }
            Self::Bounded(tx) => {
                let _ = tx.send(ack);
            }
            Self::Tokio(tx) => {
                let _ = tx.send(ack);
            }
        }
    }
}
struct Probe {
    work: Work,
    reply: ReplySender,
}
impl Message for Probe {
    type Result = ();
}
impl tasks::Handler<Probe> for Sink {
    async fn handle(&mut self, probe: Probe, _: &tasks::Context<Self>) {
        probe.reply.send(process(probe.work));
    }
}
impl threads::Handler<Probe> for Sink {
    fn handle(&mut self, probe: Probe, _: &threads::Context<Self>) {
        probe.reply.send(process(probe.work));
    }
}
enum TokioMessage {
    Request(Work, tokio::sync::oneshot::Sender<Ack>),
    Probe(Probe),
}
fn external_reply(kind: &str) -> (ReplySender, Reply) {
    match kind {
        "std" => {
            let (tx, rx) = mpsc::channel();
            (ReplySender::Std(tx), Reply::Threads(rx))
        }
        "bounded" => {
            let (tx, rx) = mpsc::sync_channel(1);
            (ReplySender::Bounded(tx), Reply::Threads(rx))
        }
        "tokio" => {
            let (tx, rx) = tokio::sync::oneshot::channel();
            (ReplySender::Tokio(tx), Reply::Tasks(rx))
        }
        _ => panic!("unknown external reply"),
    }
}
#[derive(Clone)]
struct ReadySignal {
    id: usize,
    gate: Arc<ready_gate::ReadyGate>,
    ready: mpsc::Sender<Option<usize>>,
}
struct ReadyMailbox {
    rx: mpsc::Receiver<Probe>,
    buffered: Option<Probe>,
    batch_hint: usize,
}
struct ReadyActor {
    mailbox: std::sync::Mutex<ReadyMailbox>,
    gate: Arc<ready_gate::ReadyGate>,
}
#[derive(Clone)]
struct NativeSender {
    tx: mpsc::SyncSender<Probe>,
    wake: Option<mpsc::SyncSender<()>>,
    ready: Option<ReadySignal>,
}
impl NativeSender {
    fn send(&self, probe: Probe) {
        self.tx
            .send(probe)
            .unwrap_or_else(|_| panic!("native worker stopped"));
        if let Some(signal) = &self.ready {
            signal
                .gate
                .notify(|| signal.ready.send(Some(signal.id)).unwrap());
        }
        if let Some(wake) = &self.wake {
            let _ = wake.try_send(());
        }
    }
}
#[derive(Clone)]
enum Target {
    Tasks(tasks::ActorRef<Sink>),
    Threads(threads::ActorRef<Sink>),
    Tokio(tokio::sync::mpsc::UnboundedSender<TokioMessage>),
    Native(NativeSender),
}
enum Reply {
    Tasks(tokio::sync::oneshot::Receiver<Ack>),
    Threads(mpsc::Receiver<Ack>),
}
impl Target {
    fn request(&self, work: Work, kind: &str) -> Reply {
        if kind != "framework" || matches!(self, Self::Native(_)) {
            let kind = if kind == "framework" { "bounded" } else { kind };
            let (tx, rx) = external_reply(kind);
            let probe = Probe { work, reply: tx };
            match self {
                Self::Tasks(actor) => actor.send(probe).unwrap(),
                Self::Threads(actor) => actor.send(probe).unwrap(),
                Self::Tokio(tx) => tx
                    .send(TokioMessage::Probe(probe))
                    .unwrap_or_else(|_| panic!("stopped")),
                Self::Native(tx) => tx.send(probe),
            }
            return rx;
        }
        match self {
            Self::Tasks(actor) => Reply::Tasks(actor.request_raw(work).expect("actor stopped")),
            Self::Threads(actor) => Reply::Threads(actor.request_raw(work).expect("actor stopped")),
            Self::Tokio(tx) => {
                let (reply, rx) = tokio::sync::oneshot::channel();
                tx.send(TokioMessage::Request(work, reply))
                    .unwrap_or_else(|_| panic!("baseline stopped"));
                Reply::Tasks(rx)
            }
            Self::Native(_) => unreachable!(),
        }
    }
}
impl Reply {
    fn wait(self, rt: &tokio::runtime::Handle, direct: bool) -> Ack {
        if direct {
            return match self {
                Self::Tasks(rx) => futures::executor::block_on(rx).expect("reply dropped"),
                Self::Threads(rx) => rx.recv().expect("reply dropped"),
            };
        }
        match self {
            Self::Tasks(rx) => rt.block_on(async {
                tokio::time::timeout(Duration::from_secs(10), rx)
                    .await
                    .expect("reply timeout")
                    .expect("reply dropped")
            }),
            Self::Threads(rx) => rx
                .recv_timeout(Duration::from_secs(10))
                .expect("reply timeout/disconnected"),
        }
    }
}
struct Shutdown(Vec<ChildHandle>);
impl Drop for Shutdown {
    fn drop(&mut self) {
        for actor in &self.0 {
            actor.stop();
        }
    }
}
#[derive(Clone)]
struct Config {
    backend: String,
    work: String,
    wait_us: u64,
    scenario: String,
    actors: usize,
    producers: usize,
    messages: usize,
    warmup: usize,
    workers: usize,
    payload: usize,
    iterations: u32,
    rate: u64,
    timing: bool,
    allocations: bool,
    reply: String,
    direct: bool,
    routing: String,
    capacity: usize,
    batch: usize,
    idle_ms: u64,
    scheduler: String,
}
impl Config {
    fn parse() -> Self {
        let args: Vec<_> = std::env::args().skip(1).collect();
        cli::validate(
            &args,
            &[
                "backend",
                "work",
                "wait-us",
                "scenario",
                "actors",
                "producers",
                "messages",
                "warmup",
                "workers",
                "payload",
                "iterations",
                "rate",
                "timing",
                "allocations",
                "reply",
                "observer",
                "routing",
                "capacity",
                "batch",
                "idle-ms",
                "scheduler",
            ],
        );
        let arg = |key: &str, default: &str| {
            args.iter()
                .find_map(|a| a.strip_prefix(&format!("--{key}=")).map(str::to_owned))
                .unwrap_or_else(|| default.to_owned())
        };
        let flag = |key: &str, default: &str| match arg(key, default).as_str() {
            "on" | "true" => true,
            "off" | "false" => false,
            _ => panic!("{key} must be on/off or true/false"),
        };
        let observer = arg("observer", "timed");
        assert!(["timed", "direct"].contains(&observer.as_str()));
        let config = Self {
            backend: arg("backend", "async"),
            work: arg("work", "cpu"),
            wait_us: arg("wait-us", "1000").parse().unwrap(),
            scenario: arg("scenario", "rpc"),
            actors: arg("actors", "1").parse().unwrap(),
            producers: arg("producers", "1").parse().unwrap(),
            messages: arg("messages", "10000").parse().unwrap(),
            warmup: arg("warmup", "1000").parse().unwrap(),
            workers: arg("workers", "2").parse().unwrap(),
            payload: arg("payload", "64").parse().unwrap(),
            iterations: arg("iterations", "0").parse().unwrap(),
            rate: arg("rate", "0").parse().unwrap(),
            timing: flag("timing", "on"),
            allocations: flag("allocations", "off"),
            reply: arg("reply", "framework"),
            direct: observer == "direct",
            routing: arg("routing", "producer"),
            capacity: arg("capacity", "256").parse().unwrap(),
            batch: arg("batch", "1").parse().unwrap(),
            idle_ms: arg("idle-ms", "100").parse().unwrap(),
            scheduler: arg("scheduler", "global"),
        };
        assert!(
            config.actors > 0 && config.producers > 0 && config.messages > 0 && config.workers > 0
        );
        assert!(["rpc", "burst", "paced"].contains(&config.scenario.as_str()));
        assert!([
            "async",
            "sharded",
            "blocking",
            "dedicated",
            "threads",
            "tokio",
            "native-ready",
            "native-sharded",
            "native-block",
            "native-hybrid",
            "native-spin"
        ]
        .contains(&config.backend.as_str()));
        assert!(["producer", "round-robin", "hotspot"].contains(&config.routing.as_str()));
        assert!(["global", "local"].contains(&config.scheduler.as_str()));
        assert!(config.scenario != "paced" || config.rate > 0);
        assert!(
            config.messages <= 1_000_000,
            "bounded campaign size required"
        );
        assert!(config.actors <= 2048 && config.producers <= 256);
        assert!(["framework", "std", "bounded", "tokio"].contains(&config.reply.as_str()));
        assert!(config.capacity > 0);
        assert!(["cpu", "timer", "blocking", "tcp"].contains(&config.work.as_str()));
        assert!(config.work == "cpu" || ["async", "sharded"].contains(&config.backend.as_str()));
        assert!(
            config.batch > 0
                || ["native-ready", "async", "sharded"].contains(&config.backend.as_str())
        );
        config
    }
}
struct Sample {
    hot: bool,
    handler_completion: u64,
    observed_reply: u64,
    queue: u64,
    service: u64,
    enqueue_lag: u64,
}
fn percentile(values: &mut [u64], fraction: usize) -> u64 {
    values.sort_unstable();
    values[(values.len() * fraction).div_ceil(100).saturating_sub(1)]
}
fn main() {
    let config = Config::parse();
    let mut builder = if config.backend == "sharded" {
        assert_eq!(
            std::env::var("SPAWNED_EXPERIMENT_SCHEDULER").as_deref(),
            Ok("sharded")
        );
        tokio::runtime::Builder::new_current_thread()
    } else {
        let mut builder = tokio::runtime::Builder::new_multi_thread();
        builder.worker_threads(config.workers);
        builder
    };
    let rt = builder.enable_all().build().unwrap();
    let server = (config.work == "tcp").then(actor_io::EchoServer::new);
    let address = server.as_ref().map(|s| s.addr);
    let _entered = rt.enter();
    let mut targets = Vec::new();
    let mut shutdown = Shutdown(Vec::new());
    let mut tokio_joins = Vec::new();
    let mut native_joins: Vec<(mpsc::Receiver<()>, Vec<()>)> = Vec::new();
    let mut native_worker_joins = Vec::new();
    let mut ready_shutdown = None;
    if config.backend == "native-ready" {
        let groups = if config.scheduler == "local" {
            config.workers
        } else {
            1
        };
        let mut ready_txs = Vec::new();
        let mut ready_rxs = Vec::new();
        for _ in 0..groups {
            let (tx, rx) = mpsc::channel::<Option<usize>>();
            ready_txs.push(tx);
            ready_rxs.push(Arc::new(std::sync::Mutex::new(rx)));
        }
        let mut actors = Vec::new();
        for id in 0..config.actors {
            let (tx, rx) = mpsc::sync_channel(config.capacity);
            let gate = Arc::new(ready_gate::ReadyGate::new());
            actors.push(ReadyActor {
                mailbox: std::sync::Mutex::new(ReadyMailbox {
                    rx,
                    buffered: None,
                    batch_hint: 1,
                }),
                gate: gate.clone(),
            });
            targets.push(Target::Native(NativeSender {
                tx,
                wake: None,
                ready: Some(ReadySignal {
                    id,
                    gate,
                    ready: ready_txs[id % groups].clone(),
                }),
            }));
        }

        let actors = Arc::new(actors);
        for worker in 0..config.workers {
            let (actors, rx, tx, batch) = (
                actors.clone(),
                ready_rxs[worker % groups].clone(),
                ready_txs[worker % groups].clone(),
                config.batch,
            );
            native_worker_joins.push(std::thread::spawn(move || loop {
                let id = { rx.lock().unwrap().recv().unwrap() };
                let Some(id) = id else { break };
                let actor = &actors[id];
                let mut mailbox = actor.mailbox.lock().unwrap();
                let budget = if batch == 0 {
                    mailbox.batch_hint
                } else {
                    batch
                };
                let turn = (batch == 0).then(Instant::now);
                let mut processed = 0;
                for _ in 0..budget {
                    let probe = mailbox
                        .buffered
                        .take()
                        .or_else(|| mailbox.rx.try_recv().ok());
                    let Some(probe) = probe else { break };
                    probe.reply.send(process(probe.work));
                    processed += 1;
                    if turn.is_some_and(|t| t.elapsed() >= Duration::from_micros(50)) {
                        break;
                    }
                }
                actor.gate.finish(
                    || {
                        if mailbox.buffered.is_none() {
                            mailbox.buffered = mailbox.rx.try_recv().ok();
                        }
                        mailbox.buffered.is_some()
                    },
                    || tx.send(Some(id)).unwrap(),
                );
                if batch == 0 {
                    mailbox.batch_hint = if mailbox.buffered.is_some() && processed == budget {
                        (budget * 2).min(64)
                    } else if mailbox.buffered.is_none() {
                        1
                    } else {
                        budget
                    };
                }
            }));
        }
        ready_shutdown = Some(ready_txs);
    }
    if config.backend == "native-sharded" {
        let groups = config.workers.min(config.actors);
        let mut receivers: Vec<Vec<mpsc::Receiver<Probe>>> =
            (0..groups).map(|_| Vec::new()).collect();
        let mut wakes = Vec::new();
        for group in receivers.iter_mut().take(groups) {
            let (wake, wake_rx) = mpsc::sync_channel(1);
            wakes.push(wake);
            // receiver ownership is transferred below after wiring actor queues.
            let _ = group;
            native_joins.push((wake_rx, Vec::new()));
        }
        for actor in 0..config.actors {
            let (tx, rx) = mpsc::sync_channel(config.capacity);
            receivers[actor % groups].push(rx);
            targets.push(Target::Native(NativeSender {
                tx,
                wake: Some(wakes[actor % groups].clone()),
                ready: None,
            }));
        }
        let batch = config.batch;
        let mut workers = Vec::new();
        for ((wake_rx, _), queues) in native_joins.drain(..).zip(receivers) {
            workers.push(std::thread::spawn(move || loop {
                let mut worked = false;
                let mut alive = 0;
                for rx in &queues {
                    let mut connected = true;
                    for _ in 0..batch {
                        match rx.try_recv() {
                            Ok(probe) => {
                                worked = true;
                                probe.reply.send(process(probe.work));
                            }
                            Err(mpsc::TryRecvError::Empty) => break,
                            Err(mpsc::TryRecvError::Disconnected) => {
                                connected = false;
                                break;
                            }
                        }
                    }
                    alive += usize::from(connected);
                }
                if alive == 0 {
                    break;
                }
                if !worked && wake_rx.recv().is_err() {
                    break;
                }
            }));
        }
        // Drop setup wake senders, otherwise worker shutdown would retain them.
        drop(wakes);
        native_worker_joins.extend(workers);
    }
    for _ in 0..if ["native-sharded", "native-ready"].contains(&config.backend.as_str()) {
        0
    } else {
        config.actors
    } {
        let target = match config.backend.as_str() {
            "async" | "sharded" | "blocking" | "dedicated" => {
                let backend = match config.backend.as_str() {
                    "async" | "sharded" => tasks::Backend::Async,
                    "blocking" => tasks::Backend::Blocking,
                    _ => tasks::Backend::Thread,
                };
                Target::Tasks(tasks::ActorStart::start_with_backend(
                    Sink::new(&config, address),
                    backend,
                ))
            }
            "threads" => Target::Threads(threads::ActorStart::start(Sink::new(&config, address))),
            "tokio" => {
                let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<TokioMessage>();
                tokio_joins.push(rt.spawn(async move {
                    while let Some(message) = rx.recv().await {
                        match message {
                            TokioMessage::Request(work, reply) => {
                                let _ = reply.send(process(work));
                            }
                            TokioMessage::Probe(probe) => probe.reply.send(process(probe.work)),
                        }
                    }
                }));
                Target::Tokio(tx)
            }
            "native-block" | "native-hybrid" | "native-spin" => {
                let (tx, rx) = mpsc::sync_channel::<Probe>(config.capacity);
                let mode = config.backend.clone();
                native_worker_joins.push(std::thread::spawn(move || loop {
                    let probe = if mode == "native-block" {
                        rx.recv().ok()
                    } else {
                        let mut tries = 0;
                        loop {
                            match rx.try_recv() {
                                Ok(probe) => break Some(probe),
                                Err(mpsc::TryRecvError::Disconnected) => break None,
                                Err(mpsc::TryRecvError::Empty) => {
                                    if mode == "native-hybrid" && tries >= 256 {
                                        break rx.recv().ok();
                                    }
                                    std::hint::spin_loop();
                                    tries += 1;
                                }
                            }
                        }
                    };
                    let Some(probe) = probe else { break };
                    probe.reply.send(process(probe.work));
                }));
                Target::Native(NativeSender {
                    tx,
                    wake: None,
                    ready: None,
                })
            }
            _ => panic!("unknown backend"),
        };
        match &target {
            Target::Tasks(a) => shutdown.0.push(a.child_handle()),
            Target::Threads(a) => shutdown.0.push(a.child_handle()),
            _ => {}
        }
        targets.push(target);
    }
    let payload: Arc<[u8]> = vec![7; config.payload].into();
    // Initialize every actor before measuring; all warmup replies are drained.
    for id in 0..config.warmup.max(config.actors) {
        let now = Instant::now();
        let ack = targets[id % config.actors]
            .request(
                Work {
                    id: id as u64,
                    enqueued: now,
                    payload: payload.clone(),
                    iterations: config.iterations,
                    timing: config.timing,
                },
                &config.reply,
            )
            .wait(rt.handle(), config.direct);
        assert_eq!(ack.id, id as u64);
    }
    let gate = Arc::new(Barrier::new(config.producers + 1));
    let epoch = Instant::now() + Duration::from_millis(config.idle_ms);
    let mut batches = Vec::new();
    let rt_handle = rt.handle().clone();
    std::thread::scope(|scope| {
        let mut joins = Vec::new();
        for producer in 0..config.producers {
            let targets = &targets;
            let gate = gate.clone();
            let payload = payload.clone();
            let config = &config;
            let rt = &rt_handle;
            joins.push(scope.spawn(move || {
                let mut samples = Vec::with_capacity(config.messages.div_ceil(config.producers));
                gate.wait();
                while Instant::now() < epoch {
                    std::thread::sleep(epoch.saturating_duration_since(Instant::now()));
                }
                // A producer uses one actor: collector order matches that sender's FIFO.
                let target_for = |id: usize| {
                    &targets[if config.routing == "round-robin" {
                        id % targets.len()
                    } else if config.routing == "hotspot" {
                        if id.is_multiple_of(2) {
                            0
                        } else {
                            1 + id % (targets.len() - 1)
                        }
                    } else {
                        producer % targets.len()
                    }]
                };
                let collect = |id, scheduled: Instant, enqueued: Instant, reply: Reply| {
                    let ack = reply.wait(rt, config.direct);
                    let observed = Instant::now();
                    assert_eq!(ack.id, id);
                    black_box(ack.checksum);
                    Sample {
                        hot: config.routing == "hotspot" && id.is_multiple_of(2),
                        handler_completion: ack.completed.duration_since(scheduled).as_nanos()
                            as u64,
                        observed_reply: observed.duration_since(scheduled).as_nanos() as u64,
                        queue: ack.queue_ns,
                        service: ack.handler_ns,
                        enqueue_lag: enqueued.duration_since(scheduled).as_nanos() as u64,
                    }
                };
                if config.scenario == "rpc" {
                    for id in (producer..config.messages).step_by(config.producers) {
                        let now = Instant::now();
                        let work = Work {
                            id: id as u64,
                            enqueued: now,
                            payload: payload.clone(),
                            iterations: config.iterations,
                            timing: config.timing,
                        };
                        samples.push(collect(
                            id as u64,
                            now,
                            now,
                            target_for(id).request(work, &config.reply),
                        ));
                    }
                } else {
                    let (tx, rx) = mpsc::channel();
                    std::thread::scope(|scope| {
                        let consumer = scope.spawn(|| {
                            rx.into_iter()
                                .map(|(id, scheduled, enqueued, reply)| {
                                    collect(id, scheduled, enqueued, reply)
                                })
                                .collect::<Vec<_>>()
                        });
                        for id in (producer..config.messages).step_by(config.producers) {
                            let scheduled = if config.scenario == "paced" {
                                epoch + Duration::from_secs_f64(id as f64 / config.rate as f64)
                            } else {
                                Instant::now()
                            };
                            if config.scenario == "paced" {
                                std::thread::sleep(
                                    scheduled.saturating_duration_since(Instant::now()),
                                );
                            }
                            let enqueued = Instant::now();
                            let work = Work {
                                id: id as u64,
                                enqueued,
                                payload: payload.clone(),
                                iterations: config.iterations,
                                timing: config.timing,
                            };
                            tx.send((
                                id as u64,
                                scheduled,
                                enqueued,
                                target_for(id).request(work, &config.reply),
                            ))
                            .unwrap();
                        }
                        drop(tx);
                        samples = consumer.join().unwrap();
                    });
                }
                samples
            }));
        }
        ALLOCS.store(0, Ordering::Relaxed);
        BYTES.store(0, Ordering::Relaxed);
        COUNT.store(config.allocations, Ordering::Relaxed);
        gate.wait();
        for join in joins {
            batches.push(join.join().unwrap());
        }
    });
    let elapsed = epoch.elapsed();
    COUNT.store(false, Ordering::Relaxed);
    let allocations = ALLOCS.load(Ordering::Relaxed);
    let bytes = BYTES.load(Ordering::Relaxed);
    let samples: Vec<_> = batches.into_iter().flatten().collect();
    assert_eq!(samples.len(), config.messages);
    let mut completion: Vec<_> = samples.iter().map(|s| s.handler_completion).collect();
    let mut observed: Vec<_> = samples.iter().map(|s| s.observed_reply).collect();
    let mut queue: Vec<_> = samples.iter().map(|s| s.queue).collect();
    let mut service: Vec<_> = samples.iter().map(|s| s.service).collect();
    let mut lag: Vec<_> = samples.iter().map(|s| s.enqueue_lag).collect();
    let mut hot: Vec<_> = samples
        .iter()
        .filter(|s| s.hot)
        .map(|s| s.handler_completion)
        .collect();
    let mut cold: Vec<_> = samples
        .iter()
        .filter(|s| !s.hot)
        .map(|s| s.handler_completion)
        .collect();
    let hot_p99 = if config.routing == "hotspot" && !hot.is_empty() {
        percentile(&mut hot, 99)
    } else {
        0
    };
    let cold_p99 = if config.routing == "hotspot" && !cold.is_empty() {
        percentile(&mut cold, 99)
    } else {
        0
    };
    let mut fields: Vec<String> = vec![
        config.backend.clone(),
        config.reply.clone(),
        if config.direct {
            "direct".into()
        } else {
            "timed".into()
        },
        config.routing.clone(),
        config.capacity.to_string(),
        config.batch.to_string(),
        config.scenario.clone(),
        config.actors.to_string(),
        if config.routing == "producer" {
            config.actors.min(config.producers).to_string()
        } else {
            config.actors.min(config.messages).to_string()
        },
        config.producers.to_string(),
        config.messages.to_string(),
        config.payload.to_string(),
        config.iterations.to_string(),
        config.rate.to_string(),
        config.timing.to_string(),
        config.allocations.to_string(),
        format!("{:.9}", elapsed.as_secs_f64()),
        format!("{:.3}", config.messages as f64 / elapsed.as_secs_f64()),
    ];
    fields.extend(
        [
            percentile(&mut completion, 50),
            percentile(&mut completion, 95),
            percentile(&mut completion, 99),
            percentile(&mut observed, 99),
            percentile(&mut queue, 99),
            percentile(&mut service, 99),
            percentile(&mut lag, 99),
            allocations,
            bytes,
            hot_p99,
            cold_p99,
        ]
        .map(|n| n.to_string()),
    );
    println!("backend,reply,observer,routing,capacity,batch,scenario,actors,active_actors,producers,messages,payload_bytes,iterations,rate,timing,allocation_counting,elapsed_s,completed_per_s,completion_p50_ns,completion_p95_ns,completion_p99_ns,observed_p99_ns,queue_p99_ns,handler_p99_ns,enqueue_lag_p99_ns,allocations,allocated_bytes,hot_p99_ns,cold_p99_ns");
    println!("{}", fields.join(","));
    for child in &shutdown.0 {
        child.stop();
    }
    for child in &shutdown.0 {
        rt.block_on(child.wait_exit_async());
    }
    drop(targets);
    if let Some(txs) = ready_shutdown {
        for worker in 0..config.workers {
            txs[worker % txs.len()].send(None).unwrap();
        }
    }
    for join in native_worker_joins {
        join.join().unwrap();
    }
    for join in tokio_joins {
        rt.block_on(join).unwrap();
    }
    if config.backend == "sharded" {
        spawned_rt::tasks::experimental::shutdown_sharded().unwrap();
    }
}
