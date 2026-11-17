//! Retained-structure gauges (`metrics` feature; docs/GUIDE.md, "Metrics"):
//! every gauge returns to its idle baseline once the actors that created
//! the structures are gone. This is also a leak test: a structure that
//! survives its actor (a slot, token, heap or service entry, job state,
//! monitor, link record, name or timer) keeps its gauge above the baseline.
//!
//! The churn exercises, in both backends: plain requests, raw
//! `send_request` receivers (owner deadline heap), deferred replies held in
//! `PendingReplies` (answered, and left for the death point), pipe jobs,
//! monitors and links in both stop orders, timers and registered names.
#![cfg(all(feature = "async", feature = "metrics", not(spawned_sim)))]
use spawned_concurrency::{
    message::Message, metrics, registry, tasks, threads, ChildHandle, Deadline, Down,
    PendingReplies, PendingReply,
};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The gauges are process-wide: the tests in this binary run one at a time.
static SERIAL: Mutex<()> = Mutex::new(());

const ACTORS: usize = 24;
/// Deferred requests expire quickly, so deadline-service entries of a dead
/// owner (removed when due) drain within the quiescence wait.
const PARK_DEADLINE: Duration = Duration::from_millis(200);

type Gauge = (&'static str, fn(&metrics::Snapshot) -> u64);
const GAUGES: &[Gauge] = &[
    ("reply_slots_alive", |s| s.reply_slots_alive),
    ("reply_tokens_alive", |s| s.reply_tokens_alive),
    ("reply_registries_alive", |s| s.reply_registries_alive),
    ("pending_replies_entries", |s| s.pending_replies_entries),
    ("deadline_heap_entries", |s| s.deadline_heap_entries),
    ("deadline_service_entries", |s| s.deadline_service_entries),
    ("deadline_service_actors", |s| s.deadline_service_actors),
    ("jobs_alive", |s| s.jobs_alive),
    ("monitors_alive", |s| s.monitors_alive),
    ("link_records_alive", |s| s.link_records_alive),
    ("registered_names", |s| s.registered_names),
    ("timers_alive", |s| s.timers_alive),
    ("actors_active", |s| s.actors_active),
    ("requests_active", |s| s.requests_active),
    ("deferred_pending", |s| s.deferred_pending),
    ("jobs_running", |s| s.jobs_running),
    ("mailbox_depth", |s| s.mailbox_depth),
];

fn read() -> Vec<u64> {
    let s = metrics::snapshot();
    GAUGES.iter().map(|(_, g)| g(&s)).collect()
}

/// Wait until every gauge is back at `baseline`; fail naming the drifted ones.
fn settles_to(baseline: &[u64], what: &str) {
    let limit = Instant::now() + Duration::from_secs(10);
    loop {
        let now = read();
        if now == baseline {
            return;
        }
        if Instant::now() > limit {
            let drift: Vec<String> = GAUGES
                .iter()
                .zip(baseline.iter().zip(&now))
                .filter(|(_, (b, n))| b != n)
                .map(|((name, _), (b, n))| format!("{name}: baseline {b}, now {n}"))
                .collect();
            panic!("{what}: gauges did not return to baseline: {drift:?}");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The retained-structure gauges (the first `RETAINED` of `GAUGES`).
const RETAINED: usize = 12;

/// Every retained-structure gauge rose during the round.
fn assert_exercised(before: &[u64], peak: &[u64]) {
    for (i, (name, _)) in GAUGES.iter().enumerate().take(RETAINED) {
        if *name == "deadline_service_actors" {
            continue; // visited and removed quickly; seen through its entries
        }
        assert!(
            peak[i] > before[i],
            "{name} never rose during the round: the churn does not exercise it"
        );
    }
}

struct Ask;
impl Message for Ask {
    type Result = u64;
}
struct Park;
impl Message for Park {
    type Result = u64;
}
struct Flush;
impl Message for Flush {
    type Result = ();
}
struct Watch(ChildHandle);
impl Message for Watch {
    type Result = ();
}
struct StartJob;
impl Message for StartJob {
    type Result = ();
}
struct JobDone;
impl Message for JobDone {
    type Result = ();
}
#[derive(Clone)]
struct Tick;
impl Message for Tick {
    type Result = ();
}

// ---------------------------------------------------------------------------
// tasks
// ---------------------------------------------------------------------------

struct AsyncWorker {
    name: String,
    parked: PendingReplies<u64, u64>,
    next: u64,
}
impl AsyncWorker {
    fn new(name: String) -> Self {
        Self {
            name,
            parked: PendingReplies::with_capacity(16),
            next: 0,
        }
    }
}
impl tasks::Actor for AsyncWorker {
    async fn started(&mut self, ctx: &tasks::Context<Self>) {
        registry::register(&self.name, self.next).unwrap();
        tasks::send_after(Duration::from_millis(5), ctx.clone(), Tick);
        tasks::send_interval(Duration::from_secs(3600), ctx.clone(), Tick);
    }
    async fn stopped(&mut self, _: &tasks::Context<Self>) {
        registry::unregister(&self.name);
    }
}
impl tasks::Handler<Ask> for AsyncWorker {
    async fn handle(&mut self, _: Ask, reply: PendingReply<'_, u64>, _: &tasks::Context<Self>) {
        reply.reply(1);
    }
}
impl tasks::Handler<Park> for AsyncWorker {
    async fn handle(&mut self, _: Park, reply: PendingReply<'_, u64>, _: &tasks::Context<Self>) {
        self.next += 1;
        let token = reply.defer().unwrap();
        self.parked.insert(self.next, token).unwrap();
    }
}
impl tasks::Handler<Flush> for AsyncWorker {
    async fn handle(&mut self, _: Flush, reply: PendingReply<'_, ()>, _: &tasks::Context<Self>) {
        for (key, token) in self.parked.drain() {
            token.reply(key);
        }
        reply.reply(());
    }
}
impl tasks::Handler<Watch> for AsyncWorker {
    async fn handle(&mut self, w: Watch, reply: PendingReply<'_, ()>, ctx: &tasks::Context<Self>) {
        ctx.monitor(&w.0);
        ctx.link(&w.0);
        reply.reply(());
    }
}
impl tasks::Handler<StartJob> for AsyncWorker {
    async fn handle(
        &mut self,
        _: StartJob,
        reply: PendingReply<'_, ()>,
        ctx: &tasks::Context<Self>,
    ) {
        ctx.pipe(
            tasks::JobOptions::new(),
            |_| async { 7u64 },
            |_: tasks::JobOutcome<u64>| JobDone,
        )
        .unwrap();
        ctx.pipe_blocking(
            tasks::JobOptions::new(),
            |_| 7u64,
            |_: tasks::JobOutcome<u64>| JobDone,
        )
        .unwrap();
        reply.reply(());
    }
}
impl tasks::Handler<JobDone> for AsyncWorker {
    async fn handle(&mut self, _: JobDone, r: PendingReply<'_, ()>, _: &tasks::Context<Self>) {
        r.reply(());
    }
}
impl tasks::Handler<Tick> for AsyncWorker {
    async fn handle(&mut self, _: Tick, r: PendingReply<'_, ()>, _: &tasks::Context<Self>) {
        r.reply(());
    }
}
impl tasks::Handler<Down> for AsyncWorker {
    async fn handle(&mut self, _: Down, r: PendingReply<'_, ()>, _: &tasks::Context<Self>) {
        r.reply(());
    }
}

async fn async_pair(round: usize, i: usize) {
    use tasks::ActorStart;
    let peer = AsyncWorker::new(format!("retained-a-{round}-{i}-peer")).start();
    let worker = AsyncWorker::new(format!("retained-a-{round}-{i}")).start();
    worker.request(Watch(peer.child_handle())).await.unwrap();
    for _ in 0..4 {
        assert_eq!(worker.request(Ask).await.unwrap(), 1);
    }
    // A raw receiver awaited while pending puts its deadline in the
    // owner's heap.
    let raw = worker
        .send_request(Ask, Deadline::after(PARK_DEADLINE))
        .unwrap();
    assert_eq!(raw.await.unwrap(), 1);
    let parked: Vec<_> = (0..3)
        .map(|_| worker.send_request(Park, PARK_DEADLINE).unwrap())
        .collect();
    worker.request(Flush).await.unwrap();
    for rx in parked {
        assert!(rx.await.is_ok());
    }
    worker.request(StartJob).await.unwrap();
    // Left deferred: answered at the death point.
    let orphan = worker.send_request(Park, PARK_DEADLINE).unwrap();
    worker.request(Ask).await.unwrap();
    let (first, second) = if i.is_multiple_of(2) {
        (&worker, &peer)
    } else {
        (&peer, &worker)
    };
    first.context().stop();
    first.join().await;
    second.context().stop();
    second.join().await;
    assert!(orphan.await.is_err());
}

fn async_round(rt: &tokio::runtime::Runtime, round: usize) {
    rt.block_on(futures::future::join_all(
        (0..ACTORS).map(|i| async_pair(round, i)),
    ));
}

// ---------------------------------------------------------------------------
// threads
// ---------------------------------------------------------------------------

struct NativeWorker {
    name: String,
    parked: PendingReplies<u64, u64>,
    next: u64,
}
impl NativeWorker {
    fn new(name: String) -> Self {
        Self {
            name,
            parked: PendingReplies::with_capacity(16),
            next: 0,
        }
    }
}
impl threads::Actor for NativeWorker {
    fn started(&mut self, ctx: &threads::Context<Self>) {
        registry::register(&self.name, self.next).unwrap();
        threads::send_after(Duration::from_millis(5), ctx.clone(), Tick);
        threads::send_interval(Duration::from_secs(3600), ctx.clone(), Tick);
    }
    fn stopped(&mut self, _: &threads::Context<Self>) {
        registry::unregister(&self.name);
    }
}
impl threads::Handler<Ask> for NativeWorker {
    fn handle(&mut self, _: Ask, reply: PendingReply<'_, u64>, _: &threads::Context<Self>) {
        reply.reply(1);
    }
}
impl threads::Handler<Park> for NativeWorker {
    fn handle(&mut self, _: Park, reply: PendingReply<'_, u64>, _: &threads::Context<Self>) {
        self.next += 1;
        let token = reply.defer().unwrap();
        self.parked.insert(self.next, token).unwrap();
    }
}
impl threads::Handler<Flush> for NativeWorker {
    fn handle(&mut self, _: Flush, reply: PendingReply<'_, ()>, _: &threads::Context<Self>) {
        for (key, token) in self.parked.drain() {
            token.reply(key);
        }
        reply.reply(());
    }
}
impl threads::Handler<Watch> for NativeWorker {
    fn handle(&mut self, w: Watch, reply: PendingReply<'_, ()>, ctx: &threads::Context<Self>) {
        ctx.monitor(&w.0);
        ctx.link(&w.0);
        reply.reply(());
    }
}
impl threads::Handler<StartJob> for NativeWorker {
    fn handle(&mut self, _: StartJob, reply: PendingReply<'_, ()>, ctx: &threads::Context<Self>) {
        ctx.pipe_blocking(
            threads::JobOptions::new(),
            |_| 7u64,
            |_: threads::JobOutcome<u64>| JobDone,
        )
        .unwrap();
        reply.reply(());
    }
}
impl threads::Handler<JobDone> for NativeWorker {
    fn handle(&mut self, _: JobDone, r: PendingReply<'_, ()>, _: &threads::Context<Self>) {
        r.reply(());
    }
}
impl threads::Handler<Tick> for NativeWorker {
    fn handle(&mut self, _: Tick, r: PendingReply<'_, ()>, _: &threads::Context<Self>) {
        r.reply(());
    }
}
impl threads::Handler<Down> for NativeWorker {
    fn handle(&mut self, _: Down, r: PendingReply<'_, ()>, _: &threads::Context<Self>) {
        r.reply(());
    }
}

fn native_pair(round: usize, i: usize) {
    use threads::ActorStart;
    let peer = NativeWorker::new(format!("retained-n-{round}-{i}-peer")).start();
    let worker = NativeWorker::new(format!("retained-n-{round}-{i}")).start();
    worker.request(Watch(peer.child_handle())).unwrap();
    for _ in 0..4 {
        assert_eq!(worker.request(Ask).unwrap(), 1);
    }
    let parked: Vec<_> = (0..3)
        .map(|_| worker.send_request(Park, PARK_DEADLINE).unwrap())
        .collect();
    worker.request(Flush).unwrap();
    for rx in parked {
        assert!(rx.recv().is_ok());
    }
    worker.request(StartJob).unwrap();
    let orphan = worker.send_request(Park, PARK_DEADLINE).unwrap();
    worker.request(Ask).unwrap();
    let (first, second) = if i.is_multiple_of(2) {
        (&worker, &peer)
    } else {
        (&peer, &worker)
    };
    first.context().stop();
    first.join();
    second.context().stop();
    second.join();
    assert!(orphan.recv().is_err());
}

fn native_round(round: usize) {
    std::thread::scope(|scope| {
        for i in 0..ACTORS {
            scope.spawn(move || native_pair(round, i));
        }
    });
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap()
}

/// Run `round` once to warm process-wide lazies (deadline service, detached
/// registry), then measure: the round must raise every gauge, and every
/// gauge must settle back to the baseline.
fn churn_returns_to_baseline(what: &str, mut round: impl FnMut(usize)) {
    round(0);
    std::thread::sleep(PARK_DEADLINE * 2);
    let baseline = read();
    // Sample the peak from a watcher while the round runs.
    let peak = Mutex::new(vec![0u64; GAUGES.len()]);
    let done = std::sync::atomic::AtomicBool::new(false);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            while !done.load(std::sync::atomic::Ordering::Relaxed) {
                let now = read();
                let mut peak = peak.lock().unwrap();
                for (p, n) in peak.iter_mut().zip(now) {
                    *p = (*p).max(n);
                }
                drop(peak);
                std::thread::sleep(Duration::from_micros(200));
            }
        });
        round(1);
        done.store(true, std::sync::atomic::Ordering::Relaxed);
    });
    let peak = peak.into_inner().unwrap();
    assert_exercised(&baseline, &peak);
    settles_to(&baseline, what);
}

#[test]
fn tasks_churn_returns_every_gauge_to_baseline() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let rt = runtime();
    churn_returns_to_baseline("tasks churn", |round| async_round(&rt, round));
}

#[test]
fn threads_churn_returns_every_gauge_to_baseline() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    churn_returns_to_baseline("threads churn", native_round);
}

/// Repeated churn in both backends for a bounded time
/// (`SPAWNED_RETAINED_CHURN_SECS`, default 6): after every few rounds every
/// gauge is back at the baseline, so nothing accumulates across rounds.
#[test]
fn long_churn_does_not_drift() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let secs = std::env::var("SPAWNED_RETAINED_CHURN_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(6);
    let rt = runtime();
    async_round(&rt, 0);
    native_round(0);
    std::thread::sleep(PARK_DEADLINE * 2);
    let baseline = read();
    let end = Instant::now() + Duration::from_secs(secs);
    let mut round = 1;
    while Instant::now() < end {
        for _ in 0..4 {
            async_round(&rt, round);
            native_round(round);
            round += 1;
        }
        settles_to(&baseline, &format!("after {round} rounds"));
    }
    assert!(round > 4, "the churn ran at least one batch");
}

/// The gauges are exported as Prometheus gauges.
#[test]
fn retained_gauges_are_exported() {
    let text = metrics::render_prometheus();
    for (name, _) in GAUGES {
        assert!(
            text.contains(&format!("# TYPE spawned_{name} gauge\n")),
            "spawned_{name} is not exported as a gauge"
        );
    }
}
