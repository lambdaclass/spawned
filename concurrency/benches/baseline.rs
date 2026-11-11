//! Process-isolated baseline driver; methodology in benchmarks/README.md.
#[path = "support/args.rs"]
mod cli;
use spawned_concurrency::{message::Message, tasks, threads, ChildHandle, Deadline, ReplyReceiver};
use std::{
    future::IntoFuture,
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
struct Sink;
impl tasks::Actor for Sink {}
impl tasks::Handler<Work> for Sink {
    async fn handle(
        &mut self,
        work: Work,
        reply: spawned_concurrency::reply::PendingReply<'_, Ack>,
        _: &tasks::Context<Self>,
    ) {
        reply.reply(process(work));
    }
}
impl threads::Actor for Sink {}
impl threads::Handler<Work> for Sink {
    fn handle(
        &mut self,
        work: Work,
        reply: spawned_concurrency::reply::PendingReply<'_, Ack>,
        _: &threads::Context<Self>,
    ) {
        reply.reply(process(work));
    }
}
#[derive(Clone)]
enum Target {
    Tasks(tasks::ActorRef<Sink>),
    Threads(threads::ActorRef<Sink>),
    Tokio(tokio::sync::mpsc::UnboundedSender<(Work, tokio::sync::oneshot::Sender<Ack>)>),
}
enum Reply {
    Tasks(ReplyReceiver<Ack>),
    Tokio(tokio::sync::oneshot::Receiver<Ack>),
    Threads(ReplyReceiver<Ack>),
}
impl Target {
    fn request(&self, work: Work) -> Reply {
        match self {
            Self::Tasks(actor) => Reply::Tasks(
                actor
                    .request_raw_until(work, Deadline::after(Duration::from_secs(10)))
                    .expect("actor stopped"),
            ),
            Self::Threads(actor) => Reply::Threads(
                actor
                    .request_raw_until(work, Deadline::after(Duration::from_secs(10)))
                    .expect("actor stopped"),
            ),
            Self::Tokio(tx) => {
                let (reply, rx) = tokio::sync::oneshot::channel();
                tx.send((work, reply))
                    .unwrap_or_else(|_| panic!("baseline stopped"));
                Reply::Tokio(rx)
            }
        }
    }
}
impl Reply {
    fn wait(self, rt: &tokio::runtime::Handle) -> Ack {
        match self {
            Self::Tasks(rx) => rt.block_on(rx.into_future()).expect("reply timeout or dropped"),
            Self::Tokio(rx) => rt.block_on(async {
                tokio::time::timeout(Duration::from_secs(10), rx)
                    .await
                    .expect("reply timeout")
                    .expect("reply dropped")
            }),
            Self::Threads(rx) => rx.recv().expect("reply timeout/disconnected"),
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
}
impl Config {
    fn parse() -> Self {
        let args: Vec<_> = std::env::args().skip(1).collect();
        cli::validate(
            &args,
            &[
                "backend",
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
        let config = Self {
            backend: arg("backend", "async"),
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
        };
        assert!(
            config.actors > 0 && config.producers > 0 && config.messages > 0 && config.workers > 0
        );
        assert!(["rpc", "burst", "paced"].contains(&config.scenario.as_str()));
        assert!(["async", "blocking", "dedicated", "threads", "tokio"]
            .contains(&config.backend.as_str()));
        assert!(config.scenario != "paced" || config.rate > 0);
        assert!(
            config.messages <= 1_000_000,
            "bounded campaign size required"
        );
        assert!(config.actors <= 512 && config.producers <= 256);
        config
    }
}
struct Sample {
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
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(config.workers)
        .enable_all()
        .build()
        .unwrap();
    let _entered = rt.enter();
    let mut targets = Vec::new();
    let mut shutdown = Shutdown(Vec::new());
    let mut tokio_joins = Vec::new();
    for _ in 0..config.actors {
        let target = match config.backend.as_str() {
            "async" | "blocking" | "dedicated" => {
                let backend = match config.backend.as_str() {
                    "async" => tasks::Backend::Async,
                    "blocking" => tasks::Backend::Blocking,
                    _ => tasks::Backend::Thread,
                };
                Target::Tasks(tasks::ActorStart::start_with_backend(Sink, backend))
            }
            "threads" => Target::Threads(threads::ActorStart::start(Sink)),
            "tokio" => {
                let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(
                    Work,
                    tokio::sync::oneshot::Sender<Ack>,
                )>();
                tokio_joins.push(rt.spawn(async move {
                    while let Some((work, reply)) = rx.recv().await {
                        let _ = reply.send(process(work));
                    }
                }));
                Target::Tokio(tx)
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
            .request(Work {
                id: id as u64,
                enqueued: now,
                payload: payload.clone(),
                iterations: config.iterations,
                timing: config.timing,
            })
            .wait(rt.handle());
        assert_eq!(ack.id, id as u64);
    }
    let gate = Arc::new(Barrier::new(config.producers + 1));
    let epoch = Instant::now() + Duration::from_millis(100);
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
                let target = &targets[producer % targets.len()];
                let collect = |id, scheduled: Instant, enqueued: Instant, reply: Reply| {
                    let ack = reply.wait(rt);
                    let observed = Instant::now();
                    assert_eq!(ack.id, id);
                    black_box(ack.checksum);
                    Sample {
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
                        samples.push(collect(id as u64, now, now, target.request(work)));
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
                            tx.send((id as u64, scheduled, enqueued, target.request(work)))
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
    println!("backend,scenario,actors,active_actors,producers,messages,payload_bytes,iterations,rate,timing,allocation_counting,elapsed_s,completed_per_s,completion_p50_ns,completion_p95_ns,completion_p99_ns,observed_p99_ns,queue_p99_ns,handler_p99_ns,enqueue_lag_p99_ns,allocations,allocated_bytes");
    println!(
        "{},{},{},{},{},{},{},{},{},{},{},{:.9},{:.3},{},{},{},{},{},{},{},{},{}",
        config.backend,
        config.scenario,
        config.actors,
        config.actors.min(config.producers),
        config.producers,
        config.messages,
        config.payload,
        config.iterations,
        config.rate,
        config.timing,
        config.allocations,
        elapsed.as_secs_f64(),
        config.messages as f64 / elapsed.as_secs_f64(),
        percentile(&mut completion, 50),
        percentile(&mut completion, 95),
        percentile(&mut completion, 99),
        percentile(&mut observed, 99),
        percentile(&mut queue, 99),
        percentile(&mut service, 99),
        percentile(&mut lag, 99),
        allocations,
        bytes
    );
    for child in &shutdown.0 {
        child.stop();
    }
    for child in &shutdown.0 {
        rt.block_on(child.wait_exit_async());
    }
    drop(targets);
    for join in tokio_joins {
        rt.block_on(join).unwrap();
    }
}
