//! Records pipe-to-self job traces from real actors for TLA+ trace
//! validation against `verification/tla/DeferredJobs.tla`
//! (verification/tla/README.md). Each scenario writes `jobs_<name>.json` to
//! `$LIFECYCLE_TRACE_DIR` (default `target/lifecycle-traces`);
//! `verification/tla/trace_check.py` checks them (schema `jobs-v1`).
#![cfg(feature = "lifecycle-trace")]

use spawned_concurrency::{
    lifecycle_trace,
    message::Message,
    tasks::{
        Actor, ActorRef, ActorStart, Context, Handler, JobCtx, JobHandle, JobOptions, JobOutcome,
        PendingReply,
    },
};
use spawned_rt::tasks::{oneshot, sleep};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

fn trace_dir() -> PathBuf {
    std::env::var_os("LIFECYCLE_TRACE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/lifecycle-traces")
        })
}

/// The gated old keyed job and its gate, handed from the turn to the test.
type OldJob = Arc<Mutex<Option<(JobHandle, oneshot::Sender<()>)>>>;

#[derive(Default)]
struct Log(Mutex<Vec<String>>);
impl Log {
    fn push(&self, entry: impl Into<String>) {
        self.0.lock().unwrap().push(entry.into());
    }
    fn snapshot(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

struct Owner {
    log: Arc<Log>,
    handles: Vec<JobHandle>,
    gates: Vec<oneshot::Sender<()>>,
    /// The gated old keyed job, handed to the test.
    old: OldJob,
}

impl Actor for Owner {}

struct Done(String);
impl Message for Done {
    type Result = ();
}
impl Handler<Done> for Owner {
    async fn handle(&mut self, msg: Done, _: PendingReply<'_, ()>, _: &Context<Self>) {
        self.log.push(msg.0);
    }
}

struct Emitted(u32);
impl Message for Emitted {
    type Result = ();
}
impl Handler<Emitted> for Owner {
    async fn handle(&mut self, msg: Emitted, _: PendingReply<'_, ()>, _: &Context<Self>) {
        self.log.push(format!("emit:{}", msg.0));
    }
}

fn done<T: std::fmt::Debug>(name: &'static str) -> impl FnOnce(JobOutcome<T>) -> Done {
    move |outcome| {
        Done(match outcome.into_result() {
            Ok(v) => format!("{name}:ok:{v:?}"),
            Err(e) => format!("{name}:{e}"),
        })
    }
}

#[derive(Clone, Copy)]
enum Scenario {
    Plain,
    KeyedFenced,
    Emits,
    Gated,
    Deadline,
    Blocking,
}

struct Run(Scenario);
impl Message for Run {
    type Result = ();
}
impl Handler<Run> for Owner {
    async fn handle(&mut self, msg: Run, reply: PendingReply<'_, ()>, ctx: &Context<Self>) {
        match msg.0 {
            Scenario::Plain => {
                ctx.pipe(async { 1u32 }, done("plain")).unwrap();
            }
            Scenario::KeyedFenced => {
                let (open, gate) = oneshot::channel::<()>();
                let old = ctx
                    .pipe_keyed(
                        "k",
                        async move { gate.await.map(|()| 1u32).unwrap_or(0) },
                        done("old"),
                    )
                    .unwrap();
                *self.old.lock().unwrap() = Some((old, open));
            }
            Scenario::Emits => {
                let opts = JobOptions {
                    emit_credits: 1,
                    ..Default::default()
                };
                ctx.pipe_with_ctx(
                    opts,
                    |job: JobCtx<Owner>| async move {
                        for i in 0..3 {
                            if job.emit(Emitted(i)).await.is_err() {
                                break;
                            }
                        }
                        3u32
                    },
                    done("emits"),
                )
                .unwrap();
            }
            Scenario::Gated => {
                let (open, gate) = oneshot::channel::<()>();
                let handle = ctx
                    .pipe(
                        async move { gate.await.map(|()| 1u32).unwrap_or(0) },
                        done("gated"),
                    )
                    .unwrap();
                self.handles.push(handle);
                self.gates.push(open);
            }
            Scenario::Deadline => {
                let opts = JobOptions {
                    deadline: Some(spawned_rt::time::Instant::now() + Duration::from_millis(30)),
                    ..Default::default()
                };
                ctx.pipe_with(
                    opts,
                    async {
                        sleep(Duration::from_millis(120)).await;
                        1u32
                    },
                    done("slow"),
                )
                .unwrap();
            }
            Scenario::Blocking => {
                let opts = JobOptions {
                    emit_credits: 2,
                    ..Default::default()
                };
                ctx.pipe_blocking_with(
                    opts,
                    |job: &JobCtx<Owner>| {
                        let _ = job.emit_blocking(Emitted(0));
                        let _ = job.emit_blocking(Emitted(1));
                        5u32
                    },
                    done("blocking"),
                )
                .unwrap();
            }
        }
        reply.reply(());
    }
}

/// Hold the turn until `go`, then replace key `k`: the old job finished
/// meanwhile, so its terminal message is queued behind this turn and fenced.
struct ReplaceWhen(Arc<std::sync::atomic::AtomicBool>);
impl Message for ReplaceWhen {
    type Result = ();
}
impl Handler<ReplaceWhen> for Owner {
    async fn handle(&mut self, msg: ReplaceWhen, _: PendingReply<'_, ()>, ctx: &Context<Self>) {
        while !msg.0.load(std::sync::atomic::Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(1));
        }
        ctx.pipe_keyed("k", async { 2u32 }, done("new")).unwrap();
    }
}

/// Cancel the stored handles, then open the gates.
struct CancelStored;
impl Message for CancelStored {
    type Result = ();
}
impl Handler<CancelStored> for Owner {
    async fn handle(&mut self, _: CancelStored, reply: PendingReply<'_, ()>, _: &Context<Self>) {
        for handle in self.handles.drain(..) {
            handle.cancel();
        }
        for gate in self.gates.drain(..) {
            let _ = gate.send(());
        }
        reply.reply(());
    }
}

fn start() -> (ActorRef<Owner>, Arc<Log>) {
    start_with(Arc::default())
}

fn start_with(old: OldJob) -> (ActorRef<Owner>, Arc<Log>) {
    let log = Arc::new(Log::default());
    let actor = Owner {
        old,
        log: log.clone(),
        handles: Vec::new(),
        gates: Vec::new(),
    }
    .start();
    lifecycle_trace::watch_deferred(actor.id().to_string().parse().unwrap());
    (actor, log)
}

async fn wait_for(log: &Log, entry: &str) {
    for _ in 0..400 {
        if log.snapshot().iter().any(|e| e == entry) {
            return;
        }
        sleep(Duration::from_millis(5)).await;
    }
    panic!("{entry} never logged: {:?}", log.snapshot());
}

/// Stop the owner, wait for its cleanup (the bounded job join), and write
/// its job trace.
async fn finish(name: &str, actor: ActorRef<Owner>, credits: u16) -> Vec<String> {
    let id: u64 = actor.id().to_string().parse().unwrap();
    actor.context().stop();
    actor.join().await;
    let events = lifecycle_trace::take_jobs(id);
    assert!(
        events.iter().any(|e| e.action == "Exit"),
        "{name}: the owner's exit was not recorded"
    );
    let actions: Vec<String> = events.iter().map(|e| e.action.to_string()).collect();
    let json = format!(
        "{{\"name\":\"jobs_{name}\",\"schema\":\"jobs-v1\",\"credits\":{credits},\"keys\":1,\"events\":[\n{}\n]}}\n",
        events
            .iter()
            .map(|e| e.to_json())
            .collect::<Vec<_>>()
            .join(",\n")
    );
    let dir = trace_dir();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("jobs_{name}.json")), json).unwrap();
    actions
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plain_job_result_is_dispatched() {
    let (actor, log) = start();
    actor.request(Run(Scenario::Plain)).await.unwrap();
    wait_for(&log, "plain:ok:1").await;
    let actions = finish("plain", actor, 16).await;
    assert_eq!(actions, ["Start", "Finish", "Dispatch", "Exit"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replaced_finished_keyed_job_is_fenced() {
    let old = Arc::default();
    let (actor, log) = start_with(Arc::clone(&old));
    actor.request(Run(Scenario::KeyedFenced)).await.unwrap();
    let go = Arc::new(std::sync::atomic::AtomicBool::new(false));
    actor.send(ReplaceWhen(go.clone())).unwrap();
    let (handle, open) = old.lock().unwrap().take().unwrap();
    open.send(()).unwrap();
    while handle.is_running() {
        sleep(Duration::from_millis(1)).await;
    }
    go.store(true, std::sync::atomic::Ordering::SeqCst);
    wait_for(&log, "new:ok:2").await;
    finish("keyed_fenced", actor, 16).await;
    assert!(!log.snapshot().iter().any(|e| e.starts_with("old")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn emits_precede_the_terminal_message() {
    let (actor, log) = start();
    actor.request(Run(Scenario::Emits)).await.unwrap();
    wait_for(&log, "emits:ok:3").await;
    finish("emits", actor, 1).await;
    assert_eq!(log.snapshot(), ["emit:0", "emit:1", "emit:2", "emits:ok:3"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_job_is_quiet() {
    let (actor, log) = start();
    actor.request(Run(Scenario::Gated)).await.unwrap();
    actor.request(CancelStored).await.unwrap();
    sleep(Duration::from_millis(30)).await;
    finish("cancelled", actor, 16).await;
    assert!(log.snapshot().is_empty(), "{:?}", log.snapshot());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn job_deadline_reports_while_work_runs() {
    let (actor, log) = start();
    actor.request(Run(Scenario::Deadline)).await.unwrap();
    wait_for(&log, "slow:job deadline exceeded").await;
    sleep(Duration::from_millis(200)).await;
    let actions = finish("deadline", actor, 16).await;
    assert_eq!(
        actions,
        ["Start", "JobDeadline", "Dispatch", "Finish", "Exit"]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn owner_exit_cancels_a_running_job() {
    let (actor, _log) = start();
    actor.request(Run(Scenario::Gated)).await.unwrap();
    let actions = finish("exit_running", actor, 16).await;
    assert_eq!(actions, ["Start", "Exit", "Finish"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocking_job_emits_then_completes() {
    let (actor, log) = start();
    actor.request(Run(Scenario::Blocking)).await.unwrap();
    wait_for(&log, "blocking:ok:5").await;
    finish("blocking", actor, 2).await;
    assert_eq!(log.snapshot(), ["emit:0", "emit:1", "blocking:ok:5"]);
}
