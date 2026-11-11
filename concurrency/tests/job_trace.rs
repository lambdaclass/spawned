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
        Done(match outcome {
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
    /// emit_at in the future, work done first: the result is held and the
    /// last fire releases it.
    HeldRelease,
    /// emit_at already due: fired at the call.
    DueNow,
    /// The job deadline takes a held result; the later fire is dropped.
    HeldDeadline,
    /// A cancel drops the scheduled emit.
    CancelScheduled,
}

struct Run(Scenario);
impl Message for Run {
    type Result = ();
}
impl Handler<Run> for Owner {
    async fn handle(&mut self, msg: Run, reply: PendingReply<'_, ()>, ctx: &Context<Self>) {
        match msg.0 {
            Scenario::Plain => {
                ctx.pipe(JobOptions::new(), |_| async { 1u32 }, done("plain"))
                    .unwrap();
            }
            Scenario::KeyedFenced => {
                let (open, gate) = oneshot::channel::<()>();
                let old = ctx
                    .pipe(
                        JobOptions::new().key("k"),
                        |_| async move { gate.await.map(|()| 1u32).unwrap_or(0) },
                        done("old"),
                    )
                    .unwrap();
                *self.old.lock().unwrap() = Some((old, open));
            }
            Scenario::Emits => {
                let opts = JobOptions::new().emit_credits(std::num::NonZeroU16::new(1).unwrap());
                ctx.pipe(
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
                        JobOptions::new(),
                        |_| async move { gate.await.map(|()| 1u32).unwrap_or(0) },
                        done("gated"),
                    )
                    .unwrap();
                self.handles.push(handle);
                self.gates.push(open);
            }
            Scenario::Deadline => {
                let opts = JobOptions::new().deadline(Duration::from_millis(30));
                ctx.pipe(
                    opts,
                    |_| async {
                        sleep(Duration::from_millis(120)).await;
                        1u32
                    },
                    done("slow"),
                )
                .unwrap();
            }
            Scenario::Blocking => {
                let opts = JobOptions::new().emit_credits(std::num::NonZeroU16::new(2).unwrap());
                ctx.pipe_blocking(
                    opts,
                    |job: JobCtx<Owner>| {
                        let _ = job.emit_blocking(Emitted(0));
                        let _ = job.emit_blocking(Emitted(1));
                        5u32
                    },
                    done("blocking"),
                )
                .unwrap();
            }
            Scenario::HeldRelease => {
                let opts = JobOptions::new().emit_credits(std::num::NonZeroU16::new(1).unwrap());
                ctx.pipe(
                    opts,
                    |job: JobCtx<Owner>| async move {
                        let at = spawned_concurrency::Instant::now() + Duration::from_millis(60);
                        job.emit_at(at, Emitted(9)).await.unwrap();
                        7u32
                    },
                    done("held"),
                )
                .unwrap();
            }
            Scenario::DueNow => {
                ctx.pipe(
                    JobOptions::new(),
                    |job: JobCtx<Owner>| async move {
                        let at = spawned_concurrency::Instant::now();
                        job.emit_at(at, Emitted(4)).await.unwrap();
                        4u32
                    },
                    done("due"),
                )
                .unwrap();
            }
            Scenario::HeldDeadline => {
                let opts = JobOptions::new().deadline(Duration::from_millis(30));
                ctx.pipe(
                    opts,
                    |job: JobCtx<Owner>| async move {
                        let at = spawned_concurrency::Instant::now() + Duration::from_millis(150);
                        job.emit_at(at, Emitted(8)).await.unwrap();
                        8u32
                    },
                    done("held_dl"),
                )
                .unwrap();
            }
            Scenario::CancelScheduled => {
                let handle = ctx
                    .pipe(
                        JobOptions::new(),
                        |job: JobCtx<Owner>| async move {
                            let at = spawned_concurrency::Instant::now() + Duration::from_secs(60);
                            job.emit_at(at, Emitted(6)).await.unwrap();
                            job.cancelled().await;
                            6u32
                        },
                        done("cancel_sched"),
                    )
                    .unwrap();
                self.handles.push(handle);
            }
        }
        reply.reply(());
    }
}

/// Hold the turn until `go` fires, then replace key `k`: the old job finished
/// meanwhile, so its terminal message is queued behind this turn and fenced.
/// The turn awaits instead of blocking the worker: a blocked worker strands
/// the old job, spawned into its non-stealable LIFO slot in the same poll.
struct ReplaceWhen(oneshot::Receiver<()>);
impl Message for ReplaceWhen {
    type Result = ();
}
impl Handler<ReplaceWhen> for Owner {
    async fn handle(&mut self, msg: ReplaceWhen, _: PendingReply<'_, ()>, ctx: &Context<Self>) {
        msg.0.await.unwrap();
        ctx.pipe(JobOptions::new().key("k"), |_| async { 2u32 }, done("new"))
            .unwrap();
    }
}

/// A keyed job that schedules an emit far away and waits to be cancelled;
/// `armed` fires once the emit is registered.
struct KeyedScheduled(oneshot::Sender<()>);
impl Message for KeyedScheduled {
    type Result = ();
}
impl Handler<KeyedScheduled> for Owner {
    async fn handle(
        &mut self,
        msg: KeyedScheduled,
        reply: PendingReply<'_, ()>,
        ctx: &Context<Self>,
    ) {
        let armed = msg.0;
        ctx.pipe(
            JobOptions::new().key("k"),
            |job: JobCtx<Owner>| async move {
                let at = spawned_concurrency::Instant::now() + Duration::from_secs(60);
                job.emit_at(at, Emitted(5)).await.unwrap();
                let _ = armed.send(());
                job.cancelled().await;
                5u32
            },
            done("old_sched"),
        )
        .unwrap();
        reply.reply(());
    }
}
/// Replace key `k` now.
struct ReplaceNow;
impl Message for ReplaceNow {
    type Result = ();
}
impl Handler<ReplaceNow> for Owner {
    async fn handle(&mut self, _: ReplaceNow, reply: PendingReply<'_, ()>, ctx: &Context<Self>) {
        ctx.pipe(JobOptions::new().key("k"), |_| async { 2u32 }, done("new"))
            .unwrap();
        reply.reply(());
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
    let (go, held) = oneshot::channel::<()>();
    actor.send(ReplaceWhen(held)).unwrap();
    let (handle, open) = old.lock().unwrap().take().unwrap();
    open.send(()).unwrap();
    while handle.is_running() {
        sleep(Duration::from_millis(1)).await;
    }
    go.send(()).unwrap();
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delayed_emit_releases_the_held_result() {
    let (actor, log) = start();
    actor.request(Run(Scenario::HeldRelease)).await.unwrap();
    wait_for(&log, "held:ok:7").await;
    let actions = finish("held_release", actor, 1).await;
    assert_eq!(log.snapshot(), ["emit:9", "held:ok:7"]);
    for a in ["Schedule", "Finish", "Fire", "Release"] {
        assert!(actions.iter().any(|x| x == a), "{a} missing: {actions:?}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn due_delayed_emit_fires_at_the_call() {
    let (actor, log) = start();
    actor.request(Run(Scenario::DueNow)).await.unwrap();
    wait_for(&log, "due:ok:4").await;
    let actions = finish("due_now", actor, 16).await;
    assert_eq!(log.snapshot(), ["emit:4", "due:ok:4"]);
    assert_eq!(
        actions,
        ["Start", "Schedule", "Fire", "Finish", "Dispatch", "Dispatch", "Exit"]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn job_deadline_takes_a_held_result() {
    let (actor, log) = start();
    actor.request(Run(Scenario::HeldDeadline)).await.unwrap();
    wait_for(&log, "held_dl:job deadline exceeded").await;
    sleep(Duration::from_millis(250)).await;
    let actions = finish("held_deadline", actor, 16).await;
    assert_eq!(log.snapshot(), ["held_dl:job deadline exceeded"]);
    assert_eq!(
        actions,
        [
            "Start",
            "Schedule",
            "Finish",
            "JobDeadline",
            "Dispatch",
            "Fire",
            "Exit"
        ]
    );
}

/// Final review (agy P1 2): a keyed replacement cancels the old job in the
/// step of the new job's `Start` (the model's CancelCore(ToReplace)), so
/// the old job's dropped emit is recorded after that `Start`. The
/// replacement used to cancel first, so the drop preceded the `Start` and
/// TraceJobs rejected the trace; the old job's payloads also dropped inside
/// the trace section.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keyed_replacement_drops_the_old_emit_after_the_new_start() {
    let (actor, log) = start();
    let (armed_tx, armed) = oneshot::channel();
    actor.request(KeyedScheduled(armed_tx)).await.unwrap();
    armed.await.unwrap(); // the old job's emit is registered
    actor.request(ReplaceNow).await.unwrap();
    wait_for(&log, "new:ok:2").await;
    let actions = finish("keyed_replace_scheduled", actor, 16).await;
    let second_start = actions
        .iter()
        .enumerate()
        .filter(|(_, a)| *a == "Start")
        .nth(1)
        .map(|(i, _)| i)
        .expect("the replacement's Start");
    let dropped_fire = actions
        .iter()
        .position(|a| a == "Fire")
        .expect("the old job's dropped emit");
    assert!(
        second_start < dropped_fire,
        "the replaced job's emit dropped before the new Start: {actions:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_drops_a_scheduled_emit() {
    let (actor, log) = start();
    actor.request(Run(Scenario::CancelScheduled)).await.unwrap();
    // The emit is registered before the job awaits its cancellation.
    sleep(Duration::from_millis(30)).await;
    actor.request(CancelStored).await.unwrap();
    sleep(Duration::from_millis(30)).await;
    let actions = finish("cancel_scheduled", actor, 16).await;
    assert!(log.snapshot().is_empty(), "{:?}", log.snapshot());
    assert_eq!(
        actions,
        ["Start", "Schedule", "Cancel", "Fire", "Finish", "Exit"]
    );
}

/// Threads mode: `emit_at_blocking` from a `pipe_blocking` job, result held
/// until the delayed emit fires.
mod threads_mode {
    use super::*;
    use spawned_concurrency::threads::{
        Actor as TActor, ActorStart as _, Context as TContext, Handler as THandler,
        JobCtx as TJobCtx,
    };

    struct TOwner(Arc<Log>);
    impl TActor for TOwner {}
    impl THandler<Done> for TOwner {
        fn handle(&mut self, msg: Done, _: PendingReply<'_, ()>, _: &TContext<Self>) {
            self.0.push(msg.0);
        }
    }
    impl THandler<Emitted> for TOwner {
        fn handle(&mut self, msg: Emitted, _: PendingReply<'_, ()>, _: &TContext<Self>) {
            self.0.push(format!("emit:{}", msg.0));
        }
    }
    struct Go;
    impl Message for Go {
        type Result = ();
    }
    impl THandler<Go> for TOwner {
        fn handle(&mut self, _: Go, reply: PendingReply<'_, ()>, ctx: &TContext<Self>) {
            let opts = JobOptions::new().emit_credits(std::num::NonZeroU16::new(1).unwrap());
            ctx.pipe_blocking(
                opts,
                |job: TJobCtx<TOwner>| {
                    let at = spawned_concurrency::Instant::now() + Duration::from_millis(60);
                    job.emit_at_blocking(at, Emitted(3)).unwrap();
                    3u32
                },
                done("theld"),
            )
            .unwrap();
            reply.reply(());
        }
    }

    #[test]
    fn delayed_emit_releases_the_held_result_threads() {
        let log = Arc::new(Log::default());
        let actor = TOwner(log.clone()).start();
        let id: u64 = actor.id().to_string().parse().unwrap();
        lifecycle_trace::watch_deferred(id);
        actor.request(Go).unwrap();
        for _ in 0..400 {
            if log.snapshot().iter().any(|e| e == "theld:ok:3") {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(log.snapshot(), ["emit:3", "theld:ok:3"]);
        actor.context().stop();
        actor.join();
        let events = lifecycle_trace::take_jobs(id);
        let actions: Vec<&str> = events.iter().map(|e| e.action).collect();
        for a in ["Schedule", "Finish", "Fire", "Release", "Exit"] {
            assert!(actions.contains(&a), "{a} missing: {actions:?}");
        }
        let json = format!(
            "{{\"name\":\"jobs_held_release_threads\",\"schema\":\"jobs-v1\",\"credits\":1,\"keys\":1,\"events\":[\n{}\n]}}\n",
            events.iter().map(|e| e.to_json()).collect::<Vec<_>>().join(",\n")
        );
        let dir = trace_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("jobs_held_release_threads.json"), json).unwrap();
    }
}
