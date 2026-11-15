//! Bounded-memory lifecycle/churn soak. Correctness/resource evidence, not capacity.
#[path = "support/args.rs"]
mod cli;
use spawned_concurrency::{
    message::Message,
    tasks::{Actor, ActorRef, ActorStart, Context, Handler},
    ChildHandle, Down, ExitReason, MonitorRef,
};
use std::{
    collections::HashMap,
    fs::{self, File},
    io::Write,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
const LIMIT: Duration = Duration::from_secs(15);
#[derive(Default)]
struct Counts {
    started: AtomicU64,
    stopped: AtomicU64,
    dropped: AtomicU64,
    requests: AtomicU64,
    down: AtomicU64,
    cycles: AtomicU64,
}
struct Worker {
    counts: Arc<Counts>,
    state: u64,
    bytes: Vec<u8>,
}
impl Worker {
    fn new(counts: Arc<Counts>) -> Self {
        Self {
            counts,
            state: 0,
            bytes: vec![13; 1024],
        }
    }
}
impl Actor for Worker {
    async fn started(&mut self, _: &Context<Self>) {
        self.counts.started.fetch_add(1, Ordering::Relaxed);
    }
    async fn stopped(&mut self, _: &Context<Self>) {
        self.counts.stopped.fetch_add(1, Ordering::Relaxed);
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.counts.dropped.fetch_add(1, Ordering::Relaxed);
    }
}
struct Step {
    expected: u64,
    helper: bool,
}
impl Message for Step {
    type Result = u64;
}
impl Handler<Step> for Worker {
    async fn handle(
        &mut self,
        msg: Step,
        reply: spawned_concurrency::PendingReply<'_, u64>,
        _: &Context<Self>,
    ) {
        let __reply_value: u64 = {
            assert_eq!(self.state, msg.expected);
            assert_eq!(self.bytes[0], 13);
            if msg.helper {
                assert_eq!(
                    tokio::spawn(async {
                        tokio::time::sleep(Duration::from_millis(1)).await;
                        42
                    })
                    .await
                    .unwrap(),
                    42
                );
            }
            self.state += 1;
            self.counts.requests.fetch_add(1, Ordering::Relaxed);
            self.state
        };
        reply.reply(__reply_value);
    }
}
struct Crash;
impl Message for Crash {
    type Result = ();
}
impl Handler<Crash> for Worker {
    async fn handle(
        &mut self,
        _: Crash,
        _reply: spawned_concurrency::PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
        panic!("soak injected handler failure");
    }
}
struct Watcher {
    counts: Arc<Counts>,
    pending: HashMap<MonitorRef, bool>,
}
impl Actor for Watcher {}
struct Watch(Vec<(ChildHandle, bool)>);
impl Message for Watch {
    type Result = ();
}
impl Handler<Watch> for Watcher {
    async fn handle(
        &mut self,
        msg: Watch,
        reply: spawned_concurrency::PendingReply<'_, ()>,
        ctx: &Context<Self>,
    ) {
        for (child, panic) in msg.0 {
            assert!(self.pending.insert(ctx.monitor(&child), panic).is_none());
        }
        reply.reply(());
    }
}
impl Handler<Down> for Watcher {
    async fn handle(
        &mut self,
        msg: Down,
        reply: spawned_concurrency::PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
        let panic = self
            .pending
            .remove(&msg.monitor_ref)
            .expect("duplicate/unknown monitor notification");
        assert_eq!(matches!(msg.reason, ExitReason::Panic(_)), panic);
        if !panic {
            assert_eq!(msg.reason, ExitReason::Normal);
        }
        self.counts.down.fetch_add(1, Ordering::Release);
        reply.reply(());
    }
}
struct Remaining;
impl Message for Remaining {
    type Result = usize;
}
impl Handler<Remaining> for Watcher {
    async fn handle(
        &mut self,
        _: Remaining,
        reply: spawned_concurrency::PendingReply<'_, usize>,
        _: &Context<Self>,
    ) {
        reply.reply(self.pending.len());
    }
}
fn pin(cpus: &str) {
    let tid = fs::read_link("/proc/thread-self").unwrap();
    let r = std::process::Command::new("taskset")
        .args(["-pc", cpus, tid.file_name().unwrap().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(r.status.success());
}
fn rss() -> u64 {
    fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find_map(|l| {
            l.strip_prefix("VmRSS:")
                .map(|s| s.split_whitespace().next().unwrap().parse::<u64>().unwrap() * 1024)
        })
        .unwrap()
}
async fn stop(actors: &[ActorRef<Worker>], panic_first: bool) {
    if panic_first {
        assert!(tokio::time::timeout(LIMIT, actors[0].request(Crash))
            .await
            .unwrap()
            .is_err());
    }
    for a in actors {
        a.child_handle().stop();
    }
    for (i, a) in actors.iter().enumerate() {
        let reason = tokio::time::timeout(LIMIT, a.wait_exit()).await.unwrap();
        assert_eq!(
            matches!(reason, ExitReason::Panic(_)),
            panic_first && i == 0
        );
        if !(panic_first && i == 0) {
            assert_eq!(reason, ExitReason::Normal);
        }
    }
}
async fn watched(observer: &ActorRef<Watcher>, actors: &[ActorRef<Worker>], panic_first: bool) {
    observer
        .request_with_timeout(
            Watch(
                actors
                    .iter()
                    .enumerate()
                    .map(|(i, a)| (a.child_handle(), panic_first && i == 0))
                    .collect(),
            ),
            LIMIT,
        )
        .await
        .unwrap();
}
async fn notifications(
    observer: &ActorRef<Watcher>,
    counts: &Counts,
    expected: u64,
    remaining: usize,
) {
    tokio::time::timeout(LIMIT, async {
        while counts.down.load(Ordering::Acquire) < expected {
            assert!(observer.exit_reason().is_none());
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(counts.down.load(Ordering::Acquire), expected);
    assert_eq!(
        observer
            .request_with_timeout(Remaining, LIMIT)
            .await
            .unwrap(),
        remaining
    );
}
fn main() {
    assert!(std::path::Path::new("/proc/self/task").is_dir());
    let args: Vec<String> = std::env::args().skip(1).collect();
    cli::validate(
        &args,
        &[
            "seconds",
            "workers",
            "idle",
            "churn",
            "steps",
            "series",
            "driver-cpus",
            "sampler-cpus",
        ],
    );
    let get = |k: &str, d: &str| {
        args.iter()
            .find_map(|a| a.strip_prefix(&format!("--{k}=")).map(str::to_owned))
            .unwrap_or_else(|| d.into())
    };
    let seconds: u64 = get("seconds", "600").parse().unwrap();
    let workers: usize = get("workers", "4").parse().unwrap();
    let idle: usize = get("idle", "1024").parse().unwrap();
    let churn: usize = get("churn", "128").parse().unwrap();
    let steps: u64 = get("steps", "16").parse().unwrap();
    assert!(
        (1..=1800).contains(&seconds)
            && (1..=16).contains(&workers)
            && (1..=4096).contains(&idle)
            && (1..=512).contains(&churn)
            && (1..=256).contains(&steps)
    );
    let scheduler = std::env::var("SPAWNED_EXPERIMENT_SCHEDULER").unwrap_or_default();
    let owned = scheduler == "sharded"
        || spawned_rt::tasks::experimental::dispatcher::policy_for_scheduler(&scheduler).is_some();
    let mut builder = if owned {
        tokio::runtime::Builder::new_current_thread()
    } else {
        let mut b = tokio::runtime::Builder::new_multi_thread();
        b.worker_threads(workers).thread_name("actor-worker");
        b
    };
    let runtime = builder.enable_all().build().unwrap();
    let entered = runtime.enter();
    let counts = Arc::new(Counts::default());
    let observer = Watcher {
        counts: counts.clone(),
        pending: HashMap::new(),
    }
    .start();
    // Initialize every owned actor worker/runtime while the caller still inherits actor CPUs.
    let seed = Worker::new(counts.clone()).start();
    runtime
        .block_on(seed.request(Step {
            expected: 0,
            helper: false,
        }))
        .unwrap();
    runtime.block_on(stop(&[seed], false));
    pin(&get("driver-cpus", "6,7,14,15"));
    let done = Arc::new(AtomicBool::new(false));
    let (seen, finish) = (counts.clone(), done.clone());
    let path = get("series", "/tmp/spawned-soak.csv");
    let cpus = get("sampler-cpus", "8,9,16,17");
    let started = Instant::now();
    let sampler = std::thread::spawn(move || {
        pin(&cpus);
        let mut f = File::create(path).unwrap();
        writeln!(
            f,
            "elapsed_s,started,stopped,dropped,requests,down,cycles,rss_bytes,fds,threads"
        )
        .unwrap();
        loop {
            let done = finish.load(Ordering::Acquire);
            writeln!(
                f,
                "{:.3},{},{},{},{},{},{},{},{},{}",
                started.elapsed().as_secs_f64(),
                seen.started.load(Ordering::Relaxed),
                seen.stopped.load(Ordering::Relaxed),
                seen.dropped.load(Ordering::Relaxed),
                seen.requests.load(Ordering::Relaxed),
                seen.down.load(Ordering::Relaxed),
                seen.cycles.load(Ordering::Relaxed),
                rss(),
                fs::read_dir("/proc/self/fd").unwrap().count(),
                fs::read_dir("/proc/self/task").unwrap().count()
            )
            .unwrap();
            f.flush().unwrap();
            if done {
                break;
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    });
    runtime.block_on(async {
        let residents: Vec<_> = (0..idle)
            .map(|_| Worker::new(counts.clone()).start())
            .collect();
        watched(&observer, &residents, false).await;
        for a in &residents {
            assert_eq!(
                a.request_with_timeout(
                    Step {
                        expected: 0,
                        helper: true
                    },
                    LIMIT
                )
                .await
                .unwrap(),
                1
            );
        }
        let mut expected_down = 0;
        while started.elapsed() < Duration::from_secs(seconds) {
            let actors: Vec<_> = (0..churn)
                .map(|_| Worker::new(counts.clone()).start())
                .collect();
            let panic = counts.cycles.load(Ordering::Relaxed).is_multiple_of(16);
            watched(&observer, &actors, panic).await;
            let jobs = actors.iter().enumerate().map(|(i, a)| async move {
                for expected in 0..steps {
                    assert_eq!(
                        a.request_with_timeout(
                            Step {
                                expected,
                                helper: expected == 0 && i % 16 == 0
                            },
                            LIMIT
                        )
                        .await
                        .unwrap(),
                        expected + 1
                    );
                }
            });
            futures::future::join_all(jobs).await;
            stop(&actors, panic).await;
            expected_down += churn as u64;
            notifications(&observer, &counts, expected_down, idle).await;
            drop(actors);
            counts.cycles.fetch_add(1, Ordering::Release);
        }
        stop(&residents, false).await;
        expected_down += idle as u64;
        notifications(&observer, &counts, expected_down, 0).await;
        drop(residents);
        observer.child_handle().stop();
        assert_eq!(
            tokio::time::timeout(LIMIT, observer.wait_exit())
                .await
                .unwrap(),
            ExitReason::Normal
        );
    });
    let cycles = counts.cycles.load(Ordering::Relaxed);
    assert!(cycles > 0, "increase duration to include churn after setup");
    assert_eq!(
        counts.started.load(Ordering::Relaxed),
        1 + idle as u64 + cycles * churn as u64
    );
    assert_eq!(
        counts.requests.load(Ordering::Relaxed),
        1 + idle as u64 + cycles * churn as u64 * steps
    );
    assert_eq!(
        counts.down.load(Ordering::Relaxed),
        idle as u64 + cycles * churn as u64
    );
    assert_eq!(
        counts.started.load(Ordering::Relaxed),
        counts.stopped.load(Ordering::Relaxed)
    );
    assert_eq!(
        counts.started.load(Ordering::Relaxed),
        counts.dropped.load(Ordering::Relaxed)
    );
    done.store(true, Ordering::Release);
    sampler.join().unwrap();
    if scheduler == "sharded" {
        spawned_rt::tasks::experimental::shutdown_sharded().unwrap();
    } else if owned {
        spawned_rt::tasks::experimental::dispatcher::shutdown().unwrap();
    }
    drop(entered);
    drop(runtime);
    println!("cycles,started,stopped,dropped,requests,down,elapsed_s");
    println!(
        "{},{},{},{},{},{},{:.3}",
        counts.cycles.load(Ordering::Relaxed),
        counts.started.load(Ordering::Relaxed),
        counts.stopped.load(Ordering::Relaxed),
        counts.dropped.load(Ordering::Relaxed),
        counts.requests.load(Ordering::Relaxed),
        counts.down.load(Ordering::Relaxed),
        started.elapsed().as_secs_f64()
    );
}
