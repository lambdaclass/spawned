//! Same end-to-end computation, streamed through one or many actor pipelines.
use spawned_concurrency::{
    message::Message,
    tasks::{Actor, ActorRef, ActorStart, Context, Handler},
    ExitReason,
};
use std::{
    fs,
    hint::black_box,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::sync::oneshot;
const WORK: usize = 800;
const MUL: u64 = 6364136223846793005;
#[derive(Default)]
struct Counts {
    started: AtomicUsize,
    stopped: AtomicUsize,
    dropped: AtomicUsize,
}
struct Stage {
    next: Option<ActorRef<Stage>>,
    iterations: usize,
    counts: Arc<Counts>,
    handled: u64,
}
impl Actor for Stage {
    async fn started(&mut self, _: &Context<Self>) {
        self.counts.started.fetch_add(1, Ordering::Relaxed);
    }
    async fn stopped(&mut self, _: &Context<Self>) {
        self.counts.stopped.fetch_add(1, Ordering::Relaxed);
    }
}
impl Drop for Stage {
    fn drop(&mut self) {
        self.counts.dropped.fetch_add(1, Ordering::Relaxed);
    }
}
struct Wire(Option<ActorRef<Stage>>);
impl Message for Wire {
    type Result = ();
}
impl Handler<Wire> for Stage {
    async fn handle(&mut self, m: Wire, _: &Context<Self>) {
        self.next = m.0;
    }
}
struct Work {
    id: u64,
    value: u64,
    hops: usize,
    reply: oneshot::Sender<(u64, u64, usize, Instant)>,
}
impl Message for Work {
    type Result = ();
}
impl Handler<Work> for Stage {
    async fn handle(&mut self, mut m: Work, _: &Context<Self>) {
        for _ in 0..self.iterations {
            m.value = black_box(m.value.wrapping_mul(MUL).wrapping_add(1));
        }
        m.hops += 1;
        self.handled += 1;
        if let Some(next) = &self.next {
            next.send(m).unwrap();
        } else {
            m.reply
                .send((m.id, m.value, m.hops, Instant::now()))
                .unwrap();
        }
    }
}
struct Audit;
impl Message for Audit {
    type Result = (String, String, u64);
}
impl Handler<Audit> for Stage {
    async fn handle(&mut self, _: Audit, _: &Context<Self>) -> (String, String, u64) {
        let status = fs::read_to_string("/proc/thread-self/status").unwrap();
        let affinity = status
            .lines()
            .find_map(|s| s.strip_prefix("Cpus_allowed_list:"))
            .unwrap()
            .trim()
            .to_owned();
        (
            std::thread::current().name().unwrap_or("main").to_owned(),
            affinity,
            self.handled,
        )
    }
}
fn pin(mask: &str) {
    let tid = fs::read_link("/proc/thread-self").unwrap();
    let result = std::process::Command::new("taskset")
        .args(["-pc", mask, tid.file_name().unwrap().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}
fn cpu_ticks() -> u64 {
    let stat = fs::read_to_string("/proc/self/stat").unwrap();
    let fields: Vec<_> = stat
        .rsplit_once(')')
        .unwrap()
        .1
        .split_whitespace()
        .collect();
    fields[11].parse::<u64>().unwrap() + fields[12].parse::<u64>().unwrap()
}
fn arg(name: &str, default: &str) -> String {
    std::env::args()
        .find_map(|s| s.strip_prefix(&format!("--{name}=")).map(str::to_owned))
        .unwrap_or_else(|| default.into())
}
fn quantile(values: &[u64], pct: usize) -> u64 {
    values[(values.len() * pct).div_ceil(100).saturating_sub(1)]
}
fn main() {
    let workers: usize = arg("workers", "4").parse().unwrap();
    let groups: usize = arg("groups", "1").parse().unwrap();
    let messages: usize = arg("messages", "1048576").parse().unwrap();
    let producers: usize = arg("producers", "256").parse().unwrap();
    let idle_ms: u64 = arg("idle-ms", "1000").parse().unwrap();
    assert!(
        [1, 2, 4, 8].contains(&workers)
            && [1, 16].contains(&groups)
            && messages >= producers
            && messages <= 2_097_152
    );
    assert_eq!(WORK % workers, 0);
    let cores = arg("actor-cpus", "2,3,4,5");
    let core_ids: Vec<_> = cores.split(',').map(str::to_owned).collect();
    assert_eq!(core_ids.len(), workers);
    let driver = arg("driver-cpus", "10,11,12,13");
    let scheduler = std::env::var("SPAWNED_EXPERIMENT_SCHEDULER").unwrap_or_default();
    let sharded = scheduler == "sharded";
    let mut builder = if sharded {
        tokio::runtime::Builder::new_current_thread()
    } else {
        let mut b = tokio::runtime::Builder::new_multi_thread();
        b.worker_threads(workers).thread_name("pipeline-worker");
        b
    };
    let actor_runtime = builder.enable_all().build().unwrap();
    let entered = actor_runtime.enter();
    let counts = Arc::new(Counts::default());
    let actors: Vec<_> = (0..workers * groups)
        .map(|_| {
            Stage {
                next: None,
                iterations: WORK / workers,
                counts: counts.clone(),
                handled: 0,
            }
            .start()
        })
        .collect();
    actor_runtime.block_on(async {
        for (i, actor) in actors.iter().enumerate() {
            let next = if i % workers + 1 < workers {
                Some(actors[i + 1].clone())
            } else {
                None
            };
            actor.request(Wire(next)).await.unwrap();
        }
    });
    let mut audit = String::from("actor,thread,allowed_cpus,handled_before\n");
    actor_runtime.block_on(async {
        for (i, actor) in actors.iter().enumerate() {
            let (name, affinity, handled) = actor.request(Audit).await.unwrap();
            if sharded {
                assert_eq!(name, format!("spawned-shard-{}", i % workers));
                assert_eq!(
                    affinity,
                    core_ids[i % workers],
                    "pin must precede actor startup"
                );
            }
            audit.push_str(&format!("{i},{name},\"{affinity}\",{handled}\n"));
        }
    });
    fs::write(arg("audit", "/tmp/pipeline-audit.csv"), audit).unwrap();
    let roots: Vec<_> = (0..groups).map(|g| actors[g * workers].clone()).collect();
    // Affine composition verifies the 800-step LCG with one multiply/add per reply.
    let (mut a, mut b) = (1u64, 0u64);
    for _ in 0..WORK {
        a = a.wrapping_mul(MUL);
        b = b.wrapping_mul(MUL).wrapping_add(1);
    }
    actor_runtime.block_on(async {
        for (id, root) in roots.iter().enumerate() {
            let (tx, rx) = oneshot::channel();
            root.send(Work {
                id: id as u64,
                value: id as u64,
                hops: 0,
                reply: tx,
            })
            .unwrap();
            let (reply, value, hops, _) = rx.await.unwrap();
            assert_eq!(
                (reply, value, hops),
                (
                    id as u64,
                    (id as u64).wrapping_mul(a).wrapping_add(b),
                    workers
                )
            );
        }
    });
    drop(entered);
    pin(&driver);
    let driver_mask = driver.clone();
    let driver_runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .thread_name("pipeline-driver")
        .on_thread_start(move || pin(&driver_mask))
        .enable_all()
        .build()
        .unwrap();
    let hz = String::from_utf8(
        std::process::Command::new("getconf")
            .arg("CLK_TCK")
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .parse::<f64>()
    .unwrap();
    let idle_start = Instant::now();
    let idle_cpu = cpu_ticks();
    std::thread::sleep(Duration::from_millis(idle_ms));
    let idle_cpu_s = (cpu_ticks() - idle_cpu) as f64 / hz;
    let idle_wall_s = idle_start.elapsed().as_secs_f64();
    let barrier = Arc::new(tokio::sync::Barrier::new(producers + 1));
    let mut jobs = Vec::new();
    for producer in 0..producers {
        let roots = roots.clone();
        let barrier = barrier.clone();
        jobs.push(driver_runtime.spawn(async move {
            let mut observed = Vec::with_capacity(messages.div_ceil(producers));
            let mut completed = Vec::with_capacity(messages.div_ceil(producers));
            barrier.wait().await;
            for id in (producer..messages).step_by(producers) {
                let (tx, rx) = oneshot::channel();
                let start = Instant::now();
                roots[id % groups]
                    .send(Work {
                        id: id as u64,
                        value: id as u64,
                        hops: 0,
                        reply: tx,
                    })
                    .unwrap();
                let (reply, value, hops, done) = rx.await.unwrap();
                let end = Instant::now();
                assert_eq!(
                    (reply, value, hops),
                    (
                        id as u64,
                        (id as u64).wrapping_mul(a).wrapping_add(b),
                        workers
                    )
                );
                observed.push(end.duration_since(start).as_nanos() as u64);
                completed.push(done.duration_since(start).as_nanos() as u64);
            }
            (observed, completed)
        }));
    }
    let start = Instant::now();
    let active_cpu = cpu_ticks();
    let results = driver_runtime.block_on(async {
        barrier.wait().await;
        futures::future::join_all(jobs).await
    });
    let elapsed = start.elapsed().as_secs_f64();
    let active_cpu_s = (cpu_ticks() - active_cpu) as f64 / hz;
    let (mut observed, mut completed) =
        (Vec::with_capacity(messages), Vec::with_capacity(messages));
    for result in results {
        let (o, c) = result.unwrap();
        observed.extend(o);
        completed.extend(c);
    }
    assert_eq!(observed.len(), messages);
    driver_runtime.block_on(async {
        let mut handled = 0;
        for actor in &actors {
            handled += actor.request(Audit).await.unwrap().2;
        }
        assert_eq!(handled, (messages + groups) as u64 * workers as u64);
        for actor in &actors {
            actor.child_handle().stop();
        }
        for actor in &actors {
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(5), actor.wait_exit())
                    .await
                    .unwrap(),
                ExitReason::Normal
            );
        }
    });
    drop(roots);
    drop(actors);
    assert_eq!(counts.started.load(Ordering::Relaxed), workers * groups);
    assert_eq!(counts.stopped.load(Ordering::Relaxed), workers * groups);
    if sharded {
        spawned_rt::tasks::experimental::shutdown_sharded().unwrap();
    }
    // Joining runtime workers also joins actor task-frame destruction.
    drop(actor_runtime);
    assert_eq!(counts.dropped.load(Ordering::Relaxed), workers * groups);
    observed.sort_unstable();
    completed.sort_unstable();
    println!("messages,succeeded,timeouts,replies_per_s,observed_p99_ns,completion_p99_ns,idle_cpu_s,idle_wall_s,active_cpu_s,elapsed_s,started,stopped,dropped");
    println!("{messages},{messages},0,{:.3},{},{},{idle_cpu_s:.3},{idle_wall_s:.3},{active_cpu_s:.3},{elapsed:.6},{},{},{}",messages as f64/elapsed,quantile(&observed,99),quantile(&completed,99),workers*groups,workers*groups,workers*groups);
}
