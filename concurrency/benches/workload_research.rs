//! Full actors, explicit DAG placement and independently scheduled offered load.
#[path = "support/args.rs"]
mod cli;
#[path = "support/profiling.rs"]
mod profiling;
use futures::{stream::FuturesUnordered, StreamExt};
use spawned_concurrency::{
    message::Message,
    tasks::{Actor, ActorRef, ActorStart, Context, Handler},
};
use std::{
    collections::HashMap,
    fs::{self, File},
    hint::black_box,
    io::Write,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Barrier,
    },
    time::{Duration, Instant},
};

#[derive(Clone)]
struct Config {
    mode: String,
    topology: String,
    layout: String,
    workers: usize,
    caller: String,
    caller_threads: usize,
    observation_counter: bool,
    actors: usize,
    width: usize,
    producers: usize,
    messages: usize,
    rate: u64,
    iterations: u32,
    hot_iterations: u32,
    hot_actors: usize,
    burst: usize,
    deadline_ms: u64,
    driver_cpus: String,
    collector_cpus: String,
    series: String,
    threads: String,
}
impl Config {
    fn parse() -> Self {
        let args: Vec<String> = std::env::args().skip(1).collect();
        cli::validate(
            &args,
            &[
                "mode",
                "topology",
                "layout",
                "workers",
                "caller",
                "caller-threads",
                "observation-counter",
                "actors",
                "width",
                "producers",
                "messages",
                "rate",
                "iterations",
                "hot-iterations",
                "hot-actors",
                "burst",
                "deadline-ms",
                "driver-cpus",
                "collector-cpus",
                "series",
                "threads",
            ],
        );
        let arg = |key: &str, default: &str| {
            args.iter()
                .find_map(|a| a.strip_prefix(&format!("--{key}=")).map(str::to_owned))
                .unwrap_or_else(|| default.into())
        };
        let c = Self {
            mode: arg("mode", "graph"),
            topology: arg("topology", "chain"),
            layout: arg("layout", "same"),
            workers: arg("workers", "4").parse().unwrap(),
            caller: arg("caller", "blocking"),
            caller_threads: arg("caller-threads", "4").parse().unwrap(),
            observation_counter: match arg("observation-counter", "on").as_str() {
                "on" => true,
                "off" => false,
                _ => panic!("observation-counter is on/off"),
            },
            actors: arg("actors", "1024").parse().unwrap(),
            width: arg("width", "8").parse().unwrap(),
            producers: arg("producers", "128").parse().unwrap(),
            messages: arg("messages", "262144").parse().unwrap(),
            rate: arg("rate", "0").parse().unwrap(),
            iterations: arg("iterations", "100").parse().unwrap(),
            hot_iterations: arg("hot-iterations", "5000").parse().unwrap(),
            hot_actors: arg("hot-actors", "4").parse().unwrap(),
            burst: arg("burst", "1").parse().unwrap(),
            deadline_ms: arg("deadline-ms", "2000").parse().unwrap(),
            driver_cpus: arg("driver-cpus", "6,7,8,9"),
            collector_cpus: arg("collector-cpus", "10,11,12,13"),
            series: arg("series", "/tmp/spawned-research-series.csv"),
            threads: arg("threads", "/tmp/spawned-research-threads.csv"),
        };
        assert!(["graph", "closed", "offered"].contains(&c.mode.as_str()));
        assert!(["chain", "fanout"].contains(&c.topology.as_str()));
        assert!(["same", "cross", "spread", "concentrated"].contains(&c.layout.as_str()));
        assert!((2..=16).contains(&c.workers) && c.actors <= 4096 && c.actors >= c.workers * 2);
        assert!(c.actors.is_multiple_of(c.workers) && (1..=256).contains(&c.producers));
        assert!((1..=4_194_304).contains(&c.messages) && c.burst > 0 && c.deadline_ms > 0);
        assert!(["blocking", "async"].contains(&c.caller.as_str()));
        assert!((1..=256).contains(&c.caller_threads));
        assert!(c.caller == "blocking" || c.caller_threads <= c.producers);
        assert!(
            c.caller == "blocking" || c.mode == "graph",
            "async caller control is graph-only"
        );
        assert!(
            c.observation_counter || c.mode == "graph",
            "counter control is graph-only"
        );
        if c.mode == "graph" {
            assert!((2..=16).contains(&c.width) && c.actors.is_multiple_of(c.width * c.workers));
            assert!(["same", "cross"].contains(&c.layout.as_str()));
        } else {
            assert!(["spread", "concentrated"].contains(&c.layout.as_str()));
            assert!(
                c.hot_actors > 0 && c.hot_actors < c.actors && c.hot_actors <= c.actors / c.workers
            );
        }
        if c.mode == "offered" {
            assert!(c.rate > 0);
        }
        c
    }
    fn target(&self, id: usize) -> (usize, bool) {
        if self.mode == "graph" {
            return ((id % (self.actors / self.width)) * self.width, false);
        }
        if id % 5 < 4 {
            ((id / 5) % self.hot_actors, true)
        } else {
            (
                self.hot_actors + (id / 5) % (self.actors - self.hot_actors),
                false,
            )
        }
    }
    fn intended(&self, epoch: Instant, id: usize) -> Instant {
        let slot = (id / self.burst) * self.burst;
        epoch + Duration::from_nanos((slot as u128 * 1_000_000_000 / self.rate as u128) as u64)
    }
}
fn pin_driver(cpus: &str) {
    let tid = fs::read_link("/proc/thread-self").unwrap();
    let result = std::process::Command::new("taskset")
        .args(["-pc", cpus, tid.file_name().unwrap().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "thread affinity: {}",
        String::from_utf8_lossy(&result.stderr)
    );
}
fn wait_until(at: Instant) {
    let slack = at.saturating_duration_since(Instant::now());
    if slack > Duration::from_micros(40) {
        std::thread::sleep(slack - Duration::from_micros(20));
    }
    while Instant::now() < at {
        std::hint::spin_loop();
    }
}
#[repr(align(64))]
#[derive(Default)]
struct Meter {
    submitted: AtomicU64,
    started: AtomicU64,
    completed: AtomicU64,
}
#[derive(Clone)]
struct Route {
    id: u64,
    enqueued: Instant,
    iterations: u32,
    measured: bool,
}
impl Message for Route {
    type Result = Ack;
}
struct Ack {
    id: u64,
    nodes: usize,
    tag: u64,
    checksum: u64,
    completed: Instant,
    queue_ns: u64,
    service_ns: u64,
}
struct Wire {
    children: Vec<ActorRef<Node>>,
    expected_shard: usize,
}
impl Message for Wire {
    type Result = ();
}
struct Node {
    id: usize,
    children: Vec<ActorRef<Node>>,
    meter: Arc<Meter>,
}
impl Actor for Node {}
impl Handler<Wire> for Node {
    async fn handle(&mut self, msg: Wire, _: &Context<Self>) {
        if std::env::var("SPAWNED_EXPERIMENT_SCHEDULER").as_deref() == Ok("sharded") {
            assert_eq!(
                std::thread::current().name().unwrap(),
                format!("spawned-shard-{}", msg.expected_shard)
            );
        }
        self.children = msg.children;
    }
}
#[inline(never)]
fn cpu_work(mut value: u64, iterations: u32) -> u64 {
    for _ in 0..iterations {
        value = black_box(value.wrapping_mul(6364136223846793005).wrapping_add(1));
    }
    value
}
impl Handler<Route> for Node {
    async fn handle(&mut self, msg: Route, _: &Context<Self>) -> Ack {
        let started = Instant::now();
        if msg.measured {
            self.meter.started.fetch_add(1, Ordering::Relaxed);
        }
        let value = cpu_work(msg.id ^ (self.id as u64), msg.iterations);
        // Start all fan-out requests before awaiting any; chains have one child.
        let replies: Vec<_> = self
            .children
            .iter()
            .map(|c| c.request_raw(msg.clone()).unwrap())
            .collect();
        let mut nodes = 1;
        let mut tag = self.id as u64 + 1;
        let mut checksum = value;
        for reply in futures::future::join_all(replies).await {
            let ack = reply.unwrap();
            assert_eq!(ack.id, msg.id);
            nodes += ack.nodes;
            tag += ack.tag;
            checksum ^= ack.checksum;
        }
        let completed = Instant::now();
        if msg.measured {
            self.meter.completed.fetch_add(1, Ordering::Relaxed);
        }
        Ack {
            id: msg.id,
            nodes,
            tag,
            checksum,
            completed,
            queue_ns: started.duration_since(msg.enqueued).as_nanos() as u64,
            service_ns: completed.duration_since(started).as_nanos() as u64,
        }
    }
}
fn layout(c: &Config) -> (Vec<usize>, Vec<usize>) {
    let mut buckets: Vec<Vec<usize>> = (0..c.workers).map(|_| Vec::new()).collect();
    if c.mode == "graph" {
        for id in 0..c.actors {
            let (g, j) = (id / c.width, id % c.width);
            let worker = if c.layout == "same" {
                g % c.workers
            } else if c.topology == "chain" {
                (g + j) % c.workers
            } else if j == 0 {
                g % c.workers
            } else {
                (g + 1 + (j - 1) % (c.workers - 1)) % c.workers
            };
            buckets[worker].push(id);
        }
    } else {
        for id in 0..c.hot_actors {
            buckets[if c.layout == "concentrated" {
                0
            } else {
                id % c.workers
            }]
            .push(id);
        }
        for id in c.hot_actors..c.actors {
            let worker = (0..c.workers).min_by_key(|w| buckets[*w].len()).unwrap();
            buckets[worker].push(id);
        }
    }
    assert!(buckets.iter().all(|b| b.len() == c.actors / c.workers));
    let mut order = Vec::new();
    let mut assignment = vec![0; c.actors];
    for n in 0..c.actors {
        let w = n % c.workers;
        let id = buckets[w].pop().unwrap();
        order.push(id);
        assignment[id] = w;
    }
    (order, assignment)
}
#[derive(Default)]
struct Sample {
    id: usize,
    target: usize,
    late_observed: bool,
    late_completed: bool,
    hot: bool,
    shard0: bool,
    success: bool,
    timed_out: bool,
    completion: u64,
    observed: u64,
    lag: u64,
    queue: u64,
    service: u64,
    observer_lag: u64,
}
struct Meta {
    id: usize,
    target: usize,
    hot: bool,
    intended: Instant,
    enqueued: Instant,
}
struct Pending {
    meta: Meta,
    reply: tokio::sync::oneshot::Receiver<Ack>,
}
fn sample(
    p: Meta,
    ack: Option<Ack>,
    observed: Instant,
    c: &Config,
    assignment: &[usize],
) -> Sample {
    let mut s = Sample {
        id: p.id,
        target: p.target,
        late_observed: observed > p.intended + Duration::from_millis(c.deadline_ms),
        hot: p.hot,
        shard0: assignment[p.target] == 0,
        success: ack.is_some(),
        timed_out: ack.is_none(),
        observed: observed.duration_since(p.intended).as_nanos() as u64,
        lag: p.enqueued.duration_since(p.intended).as_nanos() as u64,
        ..Default::default()
    };
    if let Some(ack) = ack {
        s.observer_lag = observed.duration_since(ack.completed).as_nanos() as u64;
        assert_eq!(ack.id, p.id as u64);
        black_box(ack.checksum);
        let expected_nodes = if c.mode == "graph" { c.width } else { 1 };
        let expected_tag = if c.mode == "graph" {
            (2 * p.target + c.width + 1) * c.width / 2
        } else {
            p.target + 1
        };
        assert_eq!(ack.nodes, expected_nodes);
        assert_eq!(ack.tag, expected_tag as u64);
        s.late_completed = ack.completed > p.intended + Duration::from_millis(c.deadline_ms);
        s.completion = ack.completed.duration_since(p.intended).as_nanos() as u64;
        s.queue = ack.queue_ns;
        s.service = ack.service_ns;
    }
    s
}
fn rss() -> u64 {
    fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find_map(|l| {
            l.strip_prefix("VmRSS:")
                .map(|v| v.split_whitespace().next().unwrap().parse::<u64>().unwrap() * 1024)
        })
        .unwrap()
}
fn observe_series(
    c: &Config,
    meters: &[Arc<Meter>],
    epoch: Instant,
    done: &AtomicBool,
    observed: &AtomicU64,
) {
    pin_driver(&c.collector_cpus);
    let mut file = File::create(&c.series).unwrap();
    writeln!(file,"elapsed_ms,submitted,started,completed,observed,queued,outstanding,hot_queued,cold_queued,rss_bytes").unwrap();
    wait_until(epoch);
    loop {
        // Acquire completion before taking the final snapshot, so it includes
        // every counter update that preceded the measured drain.
        let finished = done.load(Ordering::Acquire);
        let mut sums = [0u64; 3];
        let mut hot_queued = 0;
        let mut cold_queued = 0;
        for (i, m) in meters.iter().enumerate() {
            let counts = [
                m.submitted.load(Ordering::Relaxed),
                m.started.load(Ordering::Relaxed),
                m.completed.load(Ordering::Relaxed),
            ];
            for j in 0..3 {
                sums[j] += counts[j];
            }
            let queued = counts[0].saturating_sub(counts[1]);
            if i < c.hot_actors {
                hot_queued += queued;
            } else {
                cold_queued += queued;
            }
        }
        writeln!(
            file,
            "{:.3},{},{},{},{},{},{},{},{},{}",
            epoch.elapsed().as_secs_f64() * 1000.,
            sums[0],
            sums[1],
            sums[2],
            observed.load(Ordering::Relaxed),
            sums[0].saturating_sub(sums[1]),
            sums[0].saturating_sub(sums[2]),
            hot_queued,
            cold_queued,
            rss()
        )
        .unwrap();
        if finished {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
#[derive(Clone)]
struct ThreadStat {
    name: String,
    runtime: u64,
    wait: u64,
    slices: u64,
    voluntary: u64,
    involuntary: u64,
}
fn thread_stats() -> HashMap<String, ThreadStat> {
    let mut out = HashMap::new();
    for entry in fs::read_dir("/proc/self/task").unwrap().flatten() {
        let dir = entry.path();
        let Ok(name) = fs::read_to_string(dir.join("comm")) else {
            continue;
        };
        if !(name.starts_with("actor-worker")
            || name.starts_with("spawned-shard-")
            || name.starts_with("actor-reactor"))
        {
            continue;
        }
        let sched = fs::read_to_string(dir.join("schedstat")).unwrap();
        let v: Vec<u64> = sched
            .split_whitespace()
            .map(|s| s.parse().unwrap())
            .collect();
        let status = fs::read_to_string(dir.join("status")).unwrap();
        let number = |key: &str| {
            status
                .lines()
                .find_map(|l| l.strip_prefix(key).map(|s| s.trim().parse().unwrap()))
                .unwrap()
        };
        out.insert(
            entry.file_name().to_string_lossy().into_owned(),
            ThreadStat {
                name: name.trim().into(),
                runtime: v[0],
                wait: v[1],
                slices: v[2],
                voluntary: number("voluntary_ctxt_switches:"),
                involuntary: number("nonvoluntary_ctxt_switches:"),
            },
        );
    }
    out
}
fn save_threads(
    path: &str,
    before: HashMap<String, ThreadStat>,
    after: HashMap<String, ThreadStat>,
) {
    let mut f = File::create(path).unwrap();
    writeln!(
        f,
        "tid,name,runtime_ns,runqueue_wait_ns,slices,voluntary_switches,involuntary_switches"
    )
    .unwrap();
    for (tid, b) in before {
        let a = &after[&tid];
        writeln!(
            f,
            "{tid},{},{},{},{},{},{}",
            b.name,
            a.runtime - b.runtime,
            a.wait - b.wait,
            a.slices - b.slices,
            a.voluntary - b.voluntary,
            a.involuntary - b.involuntary
        )
        .unwrap();
    }
}
fn percentile(mut values: Vec<u64>, p: usize) -> u64 {
    if values.is_empty() {
        return 0;
    }
    values.sort_unstable();
    values[(values.len() * p).div_ceil(100).saturating_sub(1)]
}
fn main() {
    assert!(
        std::path::Path::new("/proc/self/task").is_dir(),
        "this research driver uses Linux /proc and taskset"
    );
    let c = Config::parse();
    let scheduler = std::env::var("SPAWNED_EXPERIMENT_SCHEDULER").unwrap_or_default();
    let sharded = scheduler == "sharded";
    let dispatched = matches!(
        scheduler.as_str(),
        "native-global" | "native-local" | "native-steal" | "native-root" | "tokio-pool"
    );
    let mut builder = if sharded || dispatched {
        tokio::runtime::Builder::new_current_thread()
    } else {
        let mut b = tokio::runtime::Builder::new_multi_thread();
        b.worker_threads(c.workers).thread_name("actor-worker");
        b
    };
    let runtime = builder.enable_all().build().unwrap();
    let _entered = runtime.enter();
    let (order, assignment) = layout(&c);
    let meters: Vec<_> = (0..c.actors).map(|_| Arc::new(Meter::default())).collect();
    let mut nodes: Vec<Option<ActorRef<Node>>> = (0..c.actors).map(|_| None).collect();
    for id in order {
        nodes[id] = Some(
            Node {
                id,
                children: Vec::new(),
                meter: meters[id].clone(),
            }
            .start(),
        );
    }
    let nodes: Vec<_> = nodes.into_iter().map(Option::unwrap).collect();
    for id in 0..c.actors {
        let mut children = Vec::new();
        if c.mode == "graph" {
            let j = id % c.width;
            if c.topology == "chain" && j + 1 < c.width {
                children.push(nodes[id + 1].clone());
            }
            if c.topology == "fanout" && j == 0 {
                children.extend(nodes[id + 1..id + c.width].iter().cloned());
            }
        }
        runtime
            .block_on(
                nodes[id]
                    .request_raw(Wire {
                        children,
                        expected_shard: assignment[id],
                    })
                    .unwrap(),
            )
            .unwrap();
    }
    for (id, node) in nodes.iter().enumerate() {
        let reply = node
            .request_raw(Route {
                id: id as u64,
                enqueued: Instant::now(),
                iterations: c.iterations,
                measured: false,
            })
            .unwrap();
        runtime.block_on(reply).unwrap();
    }
    let done = Arc::new(AtomicBool::new(false));
    let observed = Arc::new(AtomicU64::new(0));
    let gate = Arc::new(Barrier::new(if c.caller == "async" {
        c.caller_threads + 1
    } else {
        c.producers * if c.mode == "offered" { 2 } else { 1 } + 1
    }));
    let (epoch_tx, epoch_rx) = tokio::sync::watch::channel(None::<Instant>);
    let mut batches = Vec::new();
    let mut actor_before = None;
    let mut actor_after = None;
    let mut elapsed = Duration::ZERO;
    let mut submit_elapsed = Duration::ZERO;
    let mut observation_elapsed = Duration::ZERO;
    std::thread::scope(|scope| {
        let mut producers = Vec::new();
        let mut collectors = Vec::new();
        if c.caller == "async" {
            for driver in 0..c.caller_threads {
                let gate = gate.clone();
                let mut epoch = epoch_rx.clone();
                let (c, nodes, assignment, observed) = (&c, &nodes, &assignment, &observed);
                producers.push(
                    std::thread::Builder::new()
                        .name("graph-driver".into())
                        .spawn_scoped(scope, move || {
                            pin_driver(&c.driver_cpus);
                            let runtime = tokio::runtime::Builder::new_current_thread()
                                .enable_all()
                                .build()
                                .unwrap();
                            gate.wait();
                            runtime.block_on(async {
                                epoch.changed().await.unwrap();
                                let start = epoch.borrow().unwrap();
                                wait_until(start);
                                let producers = (driver..c.producers)
                                    .step_by(c.caller_threads)
                                    .map(|producer| async move {
                                        let mut samples =
                                            Vec::with_capacity(c.messages.div_ceil(c.producers));
                                        for id in (producer..c.messages).step_by(c.producers) {
                                            let intended = Instant::now();
                                            let enqueued = Instant::now();
                                            let (target, hot) = c.target(id);
                                            let reply = nodes[target]
                                                .request_raw(Route {
                                                    id: id as u64,
                                                    enqueued,
                                                    iterations: c.iterations,
                                                    measured: false,
                                                })
                                                .unwrap();
                                            let ack = reply.await.unwrap();
                                            samples.push(sample(
                                                Meta {
                                                    id,
                                                    target,
                                                    hot,
                                                    intended,
                                                    enqueued,
                                                },
                                                Some(ack),
                                                Instant::now(),
                                                c,
                                                assignment,
                                            ));
                                            if c.observation_counter {
                                                observed.fetch_add(1, Ordering::Relaxed);
                                            }
                                        }
                                        samples
                                    });
                                futures::future::join_all(producers)
                                    .await
                                    .into_iter()
                                    .flatten()
                                    .collect()
                            })
                        })
                        .unwrap(),
                );
            }
        } else {
            for producer in 0..c.producers {
                let (tx, rx) = if c.mode == "offered" {
                    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Pending>();
                    (Some(tx), Some(rx))
                } else {
                    (None, None)
                };
                if let Some(mut rx) = rx {
                    let collector_gate = gate.clone();
                    let mut collector_epoch = epoch_rx.clone();
                    let observed = observed.clone();
                    let assignment = &assignment;
                    let c = &c;
                    collectors.push(std::thread::Builder::new().name("load-collector".into()).spawn_scoped(scope,move|| {
                    pin_driver(&c.collector_cpus);
                    let rt=tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
                    let mut samples=Vec::with_capacity(c.messages.div_ceil(c.producers));collector_gate.wait();
                    rt.block_on(async {
                        collector_epoch.changed().await.unwrap();
                        let mut futures=FuturesUnordered::new();let mut open=true;
                        loop {tokio::select! {
                            next=rx.recv(),if open=>match next {Some(p)=>{
                                let deadline=p.meta.intended+Duration::from_millis(c.deadline_ms);
                                futures.push(async move {
                                    let ack=match tokio::time::timeout_at(tokio::time::Instant::from_std(deadline),p.reply).await {
                                        Ok(Ok(ack))=>Some(ack),Ok(Err(_))=>panic!("actor dropped an admitted request"),Err(_)=>None};
                                    sample(p.meta,ack,Instant::now(),c,assignment)
                                });},None=>open=false},
                            Some(s)=futures.next(),if !futures.is_empty()=>{observed.fetch_add(1,Ordering::Relaxed);samples.push(s);},
                            else=>break,
                        }}
                    });samples
                }).unwrap());
                }
                let producer_gate = gate.clone();
                let mut producer_epoch = epoch_rx.clone();
                let nodes = &nodes;
                let meters = &meters;
                let c = &c;
                let assignment = &assignment;
                let observed = observed.clone();
                producers.push(
                    std::thread::Builder::new()
                        .name("load-producer".into())
                        .spawn_scoped(scope, move || {
                            pin_driver(&c.driver_cpus);
                            let mut samples = if c.mode == "offered" {
                                Vec::new()
                            } else {
                                Vec::with_capacity(c.messages.div_ceil(c.producers))
                            };
                            producer_gate.wait();
                            let epoch = futures::executor::block_on(async {
                                producer_epoch.changed().await.unwrap();
                                producer_epoch.borrow().unwrap()
                            });
                            wait_until(epoch);
                            for id in (producer..c.messages).step_by(c.producers) {
                                let intended = if c.mode == "offered" {
                                    c.intended(epoch, id)
                                } else {
                                    Instant::now()
                                };
                                if c.mode == "offered" {
                                    wait_until(intended);
                                }
                                let enqueued = Instant::now();
                                let (target, hot) = c.target(id);
                                let iterations = if c.mode == "graph" || !hot {
                                    c.iterations
                                } else {
                                    c.hot_iterations
                                };
                                if c.mode != "graph" {
                                    meters[target].submitted.fetch_add(1, Ordering::Relaxed);
                                }
                                let reply = nodes[target]
                                    .request_raw(Route {
                                        id: id as u64,
                                        enqueued,
                                        iterations,
                                        measured: c.mode != "graph",
                                    })
                                    .unwrap();
                                let meta = Meta {
                                    id,
                                    target,
                                    hot,
                                    intended,
                                    enqueued,
                                };
                                if let Some(tx) = &tx {
                                    tx.send(Pending { meta, reply }).unwrap();
                                } else {
                                    let ack = futures::executor::block_on(reply).unwrap();
                                    samples.push(sample(
                                        meta,
                                        Some(ack),
                                        Instant::now(),
                                        c,
                                        assignment,
                                    ));
                                    if c.observation_counter {
                                        observed.fetch_add(1, Ordering::Relaxed);
                                    }
                                }
                            }
                            samples
                        })
                        .unwrap(),
                );
            }
        }
        gate.wait();
        actor_before = Some(thread_stats());
        let profile = profiling::start();
        let epoch = Instant::now() + Duration::from_millis(200);
        let (sampler_c, sampler_meters, sampler_done, sampler_observed) =
            (&c, &meters, &done, &observed);
        let sampler = std::thread::Builder::new()
            .name("load-sampler".into())
            .spawn_scoped(scope, move || {
                observe_series(
                    sampler_c,
                    sampler_meters,
                    epoch,
                    sampler_done,
                    sampler_observed,
                )
            })
            .unwrap();
        epoch_tx.send(Some(epoch)).unwrap();
        for p in producers {
            batches.push(p.join().unwrap());
        }
        submit_elapsed = epoch.elapsed();
        for collector in collectors {
            batches.push(collector.join().unwrap());
        }
        observation_elapsed = epoch.elapsed();
        // Offered-load deadlines can leave work running. Fences wait for it
        // without discarding messages. Closed-loop producers already awaited
        // every reply, so do not charge graphs for extra per-node fence RPCs.
        if c.mode == "offered" {
            for (id, node) in nodes.iter().enumerate() {
                runtime
                    .block_on(
                        node.request_raw(Wire {
                            children: Vec::new(),
                            expected_shard: assignment[id],
                        })
                        .unwrap(),
                    )
                    .unwrap();
            }
        }
        elapsed = epoch.elapsed();
        actor_after = Some(thread_stats());
        done.store(true, Ordering::Release);
        sampler.join().unwrap();
        profiling::finish(profile);
    });
    save_threads(&c.threads, actor_before.unwrap(), actor_after.unwrap());
    let samples: Vec<_> = batches.into_iter().flatten().collect();
    assert_eq!(samples.len(), c.messages);
    let mut seen = vec![false; c.messages];
    for s in &samples {
        assert!(!seen[s.id]);
        seen[s.id] = true;
    }
    assert!(seen.into_iter().all(|v| v));
    let succeeded = samples.iter().filter(|s| s.success).count();
    let timeouts = samples.iter().filter(|s| s.timed_out).count();
    assert_eq!(succeeded + timeouts, c.messages);
    if c.mode != "graph" {
        assert_eq!(
            meters
                .iter()
                .map(|m| m.completed.load(Ordering::Relaxed))
                .sum::<u64>(),
            c.messages as u64
        );
    }
    let mut per_cold_actor: HashMap<usize, Vec<u64>> = HashMap::new();
    for s in samples.iter().filter(|s| !s.hot) {
        per_cold_actor.entry(s.target).or_default().push(s.observed);
    }
    let worst_cold_actor_p99 = per_cold_actor
        .into_values()
        .map(|v| percentile(v, 99))
        .max()
        .unwrap_or(0);
    let fields = [
        (
            "observer_lag_p99_ns",
            percentile(
                samples
                    .iter()
                    .filter(|s| s.success)
                    .map(|s| s.observer_lag)
                    .collect(),
                99,
            ),
        ),
        (
            "cold_samples",
            samples.iter().filter(|s| !s.hot).count() as u64,
        ),
        (
            "hot_samples",
            samples.iter().filter(|s| s.hot).count() as u64,
        ),
        (
            "cold_other_observed_p99_ns",
            percentile(
                samples
                    .iter()
                    .filter(|s| !s.hot && !s.shard0)
                    .map(|s| s.observed)
                    .collect(),
                99,
            ),
        ),
        (
            "cold_shard0_observed_p99_ns",
            percentile(
                samples
                    .iter()
                    .filter(|s| !s.hot && s.shard0)
                    .map(|s| s.observed)
                    .collect(),
                99,
            ),
        ),
        ("worst_cold_actor_observed_p99_ns", worst_cold_actor_p99),
        (
            "cold_late_observed",
            samples
                .iter()
                .filter(|s| !s.hot && (s.late_observed || s.timed_out))
                .count() as u64,
        ),
        (
            "hot_late_observed",
            samples
                .iter()
                .filter(|s| s.hot && (s.late_observed || s.timed_out))
                .count() as u64,
        ),
        (
            "cold_late_completed",
            samples
                .iter()
                .filter(|s| !s.hot && s.late_completed)
                .count() as u64,
        ),
        (
            "hot_late_completed",
            samples.iter().filter(|s| s.hot && s.late_completed).count() as u64,
        ),
        (
            "completion_p50_ns",
            percentile(
                samples
                    .iter()
                    .filter(|s| s.success)
                    .map(|s| s.completion)
                    .collect(),
                50,
            ),
        ),
        (
            "completion_p99_ns",
            percentile(
                samples
                    .iter()
                    .filter(|s| s.success)
                    .map(|s| s.completion)
                    .collect(),
                99,
            ),
        ),
        (
            "observed_p99_ns",
            percentile(samples.iter().map(|s| s.observed).collect(), 99),
        ),
        (
            "lag_p99_ns",
            percentile(samples.iter().map(|s| s.lag).collect(), 99),
        ),
        (
            "queue_p99_ns",
            percentile(
                samples
                    .iter()
                    .filter(|s| s.success)
                    .map(|s| s.queue)
                    .collect(),
                99,
            ),
        ),
        (
            "service_p99_ns",
            percentile(
                samples
                    .iter()
                    .filter(|s| s.success)
                    .map(|s| s.service)
                    .collect(),
                99,
            ),
        ),
        (
            "hot_p99_ns",
            percentile(
                samples
                    .iter()
                    .filter(|s| s.success && s.hot)
                    .map(|s| s.completion)
                    .collect(),
                99,
            ),
        ),
        (
            "cold_p99_ns",
            percentile(
                samples
                    .iter()
                    .filter(|s| s.success && !s.hot)
                    .map(|s| s.completion)
                    .collect(),
                99,
            ),
        ),
        (
            "cold_shard0_p99_ns",
            percentile(
                samples
                    .iter()
                    .filter(|s| s.success && !s.hot && s.shard0)
                    .map(|s| s.completion)
                    .collect(),
                99,
            ),
        ),
        (
            "cold_other_p99_ns",
            percentile(
                samples
                    .iter()
                    .filter(|s| s.success && !s.hot && !s.shard0)
                    .map(|s| s.completion)
                    .collect(),
                99,
            ),
        ),
        (
            "hot_timeouts",
            samples.iter().filter(|s| s.timed_out && s.hot).count() as u64,
        ),
        (
            "cold_timeouts",
            samples.iter().filter(|s| s.timed_out && !s.hot).count() as u64,
        ),
        (
            "lag_over_1ms",
            samples.iter().filter(|s| s.lag > 1_000_000).count() as u64,
        ),
    ];
    println!("messages,succeeded,timeouts,elapsed_s,replies_per_s,logical_handlers,submit_elapsed_s,observation_elapsed_s,{}",fields.iter().map(|(k,_)|*k).collect::<Vec<_>>().join(","));
    println!(
        "{},{},{},{:.9},{:.3},{},{:.9},{:.9},{}",
        c.messages,
        succeeded,
        timeouts,
        elapsed.as_secs_f64(),
        succeeded as f64 / elapsed.as_secs_f64(),
        c.messages * if c.mode == "graph" { c.width } else { 1 },
        submit_elapsed.as_secs_f64(),
        observation_elapsed.as_secs_f64(),
        fields
            .iter()
            .map(|(_, v)| v.to_string())
            .collect::<Vec<_>>()
            .join(",")
    );
    for node in &nodes {
        node.child_handle().stop();
    }
    for node in &nodes {
        runtime.block_on(node.wait_exit());
    }
    drop(nodes);
    if sharded {
        spawned_rt::tasks::experimental::shutdown_sharded().unwrap();
    }
    if dispatched {
        spawned_rt::tasks::experimental::dispatcher::shutdown().unwrap();
    }
}
