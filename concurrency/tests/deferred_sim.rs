//! Simulator races for deferred replies and pipe-to-self jobs
//! (docs/design/deferred-replies.md), explored with PCT and bounded DFS over
//! real actors: a stored token's reply against its deadline, the owner's
//! death and the caller leaving; keyed replacement against the old job's
//! completion; emits against the terminal message and a cancel. Run with:
//!
//! ```sh
//! RUSTFLAGS="--cfg spawned_sim --cfg tokio_unstable" cargo test -p spawned-concurrency --test deferred_sim
//! ```
#![cfg(all(spawned_sim, feature = "async"))]

use spawned_concurrency::{
    message::Message,
    tasks::{
        Actor, ActorRef, ActorStart, Context, Deadline, Delivery, Handler, JobCtx, JobHandle,
        JobOptions, JobOutcome, PendingReply, ReplyTo,
    },
    ActorError,
};
use spawned_rt::tasks::{
    sim::explore::{explore, Strategy},
    sleep, spawn,
};
use std::{
    future::Future,
    sync::{Arc, Mutex},
    time::Duration,
};

type Log = Arc<Mutex<Vec<String>>>;

fn push(log: &Log, entry: impl Into<String>) {
    log.lock().unwrap().push(entry.into());
}

fn entries(log: &Log) -> Vec<String> {
    log.lock().unwrap().clone()
}

struct Owner {
    log: Log,
    stored: Vec<ReplyTo<u32>>,
    job: Option<JobHandle>,
}
impl Actor for Owner {}

fn owner(log: &Log) -> ActorRef<Owner> {
    Owner {
        log: log.clone(),
        stored: Vec::new(),
        job: None,
    }
    .start()
}

struct Hold;
impl Message for Hold {
    type Result = u32;
}
impl Handler<Hold> for Owner {
    async fn handle(&mut self, _: Hold, reply: PendingReply<'_, u32>, _: &Context<Self>) {
        if let Ok(token) = reply.defer() {
            self.stored.push(token);
        }
    }
}

struct Sync;
impl Message for Sync {
    type Result = ();
}
impl Handler<Sync> for Owner {
    async fn handle(&mut self, _: Sync, reply: PendingReply<'_, ()>, _: &Context<Self>) {
        reply.reply(());
    }
}

/// Answer every stored token and log how each delivery went.
struct Flush;
impl Message for Flush {
    type Result = ();
}
impl Handler<Flush> for Owner {
    async fn handle(&mut self, _: Flush, _: PendingReply<'_, ()>, _: &Context<Self>) {
        for token in self.stored.drain(..) {
            let delivery = token.reply(7);
            push(&self.log, format!("{delivery:?}"));
        }
    }
}

struct StartKeyed(&'static str, u64);
impl Message for StartKeyed {
    type Result = ();
}
impl Handler<StartKeyed> for Owner {
    async fn handle(&mut self, msg: StartKeyed, _: PendingReply<'_, ()>, ctx: &Context<Self>) {
        let StartKeyed(name, delay) = msg;
        ctx.pipe_keyed(
            "k",
            async move {
                sleep(Duration::from_millis(delay)).await;
                name
            },
            |outcome: JobOutcome<&'static str>| Done(outcome.into_result().unwrap()),
        )
        .unwrap();
        push(&self.log, format!("start:{name}"));
    }
}

struct Done(&'static str);
impl Message for Done {
    type Result = ();
}
impl Handler<Done> for Owner {
    async fn handle(&mut self, msg: Done, _: PendingReply<'_, ()>, _: &Context<Self>) {
        push(&self.log, format!("done:{}", msg.0));
    }
}

struct Tick(u32);
impl Message for Tick {
    type Result = ();
}
impl Handler<Tick> for Owner {
    async fn handle(&mut self, msg: Tick, _: PendingReply<'_, ()>, _: &Context<Self>) {
        push(&self.log, format!("tick:{}", msg.0));
    }
}

struct StartEmitter;
impl Message for StartEmitter {
    type Result = ();
}
impl Handler<StartEmitter> for Owner {
    async fn handle(&mut self, _: StartEmitter, _: PendingReply<'_, ()>, ctx: &Context<Self>) {
        let opts = JobOptions {
            emit_credits: 1,
            ..Default::default()
        };
        let job = ctx
            .pipe_with_ctx(
                opts,
                |job: JobCtx<Owner>| async move {
                    for i in 0..3 {
                        if job.emit(Tick(i)).await.is_err() {
                            break;
                        }
                        sleep(Duration::from_millis(1)).await;
                    }
                    "emitter"
                },
                |outcome: JobOutcome<&'static str>| Done(outcome.into_result().unwrap()),
            )
            .unwrap();
        self.job = Some(job);
    }
}

struct CancelJob;
impl Message for CancelJob {
    type Result = ();
}
impl Handler<CancelJob> for Owner {
    async fn handle(&mut self, _: CancelJob, _: PendingReply<'_, ()>, _: &Context<Self>) {
        if let Some(job) = self.job.take() {
            job.cancel();
            push(&self.log, "cancelled");
        }
    }
}

// ---------------------------------------------------------------------------
// Bodies
// ---------------------------------------------------------------------------

/// A stored token's reply races its deadline, the owner's stop and (on odd
/// seeds) the caller dropping its receiver. The caller sees `Ok` exactly when
/// the reply reports `Delivered`, and always gets an answer.
async fn reply_vs_expiry_vs_death_vs_drop(seed: u64) {
    let log = Log::default();
    let actor = owner(&log);
    let d = Duration::from_millis(10);
    let receiver = actor
        .request_raw_until(Hold, Deadline::At(spawned_rt::time::Instant::now() + d))
        .unwrap();
    actor.request(Sync).await.unwrap();
    let replier = actor.clone();
    spawn(async move {
        sleep(d).await;
        let _ = replier.send(Flush);
    });
    let stopper = actor.clone();
    spawn(async move {
        sleep(d).await;
        stopper.context().stop();
    });
    let awaited = seed % 2 == 0;
    let caller = spawn(async move {
        if awaited {
            Some(receiver.await)
        } else {
            sleep(d).await;
            drop(receiver);
            None
        }
    });
    let result = caller.await.unwrap();
    actor.join().await;
    let deliveries = entries(&log);
    assert!(deliveries.len() <= 1, "{deliveries:?}");
    let delivered = deliveries.first().map(String::as_str) == Some("Delivered");
    match result {
        Some(Ok(7)) => assert!(delivered, "Ok without a delivery: {deliveries:?}"),
        Some(Ok(other)) => panic!("unexpected value {other}"),
        Some(Err(e)) => {
            assert!(!delivered, "{e:?} after a delivery");
            assert!(
                matches!(e, ActorError::RequestTimeout | ActorError::ActorStopped(_)),
                "{e:?}"
            );
        }
        None => {}
    }
    if let Some(delivery) = deliveries.first() {
        let expected = [
            format!("{:?}", Delivery::Delivered),
            format!("{:?}", Delivery::Expired),
            format!("{:?}", Delivery::CallerGone),
        ];
        assert!(expected.contains(delivery), "{delivery}");
    }
}

/// A keyed job is replaced while it completes: its result never reaches a
/// turn after the replacement started.
async fn keyed_replacement(seed: u64) {
    let log = Log::default();
    let actor = owner(&log);
    actor.send(StartKeyed("a", 5)).unwrap();
    let replacer = actor.clone();
    spawn(async move {
        sleep(Duration::from_millis(4 + seed % 3)).await;
        let _ = replacer.send(StartKeyed("b", 1));
    });
    for _ in 0..100 {
        if entries(&log).iter().any(|e| e == "done:b") {
            break;
        }
        sleep(Duration::from_millis(1)).await;
    }
    actor.request(Sync).await.unwrap();
    actor.context().stop();
    actor.join().await;
    let log = entries(&log);
    assert!(log.contains(&"done:b".to_string()), "{log:?}");
    let replaced = log.iter().position(|e| e == "start:b").unwrap();
    assert!(
        !log[replaced..].contains(&"done:a".to_string()),
        "a replaced job's result after its replacement: {log:?}"
    );
}

/// Emits race the terminal message and a cancel: no tick after the terminal
/// message, and nothing from the job after the cancelling turn.
async fn emit_vs_terminal_and_cancel(seed: u64) {
    let log = Log::default();
    let actor = owner(&log);
    actor.send(StartEmitter).unwrap();
    let canceller = actor.clone();
    spawn(async move {
        sleep(Duration::from_millis(seed % 5)).await;
        let _ = canceller.send(CancelJob);
    });
    sleep(Duration::from_millis(10)).await;
    actor.request(Sync).await.unwrap();
    actor.context().stop();
    actor.join().await;
    let log = entries(&log);
    if let Some(done) = log.iter().position(|e| e.starts_with("done:")) {
        assert!(
            !log[done + 1..].iter().any(|e| e.starts_with("tick:")),
            "an emit after the terminal message: {log:?}"
        );
    }
    if let Some(cancelled) = log.iter().position(|e| e == "cancelled") {
        assert!(
            log[cancelled + 1..].is_empty(),
            "a cancelled job's message dispatched: {log:?}"
        );
    }
    let ticks: Vec<_> = log.iter().filter(|e| e.starts_with("tick:")).collect();
    let in_order = ticks
        .iter()
        .enumerate()
        .all(|(i, t)| **t == format!("tick:{i}"));
    assert!(in_order, "emits out of order: {log:?}");
}

// ---------------------------------------------------------------------------
// Driver
// ---------------------------------------------------------------------------

const STRATEGIES: [(&str, usize); 2] = [("pct:d=3", 300), ("dfs:bound=3,depth=300", 300)];

fn check<F, Fut>(name: &str, body: F)
where
    F: Fn(u64) -> Fut,
    Fut: Future<Output = ()>,
{
    for (strategy, budget) in STRATEGIES {
        let report = explore(&Strategy::parse(strategy).unwrap(), 0, budget, &body);
        assert!(
            report.found.is_none(),
            "{name} under {strategy}: {:?}",
            report.found
        );
        println!(
            "{name} {strategy}: {} runs, {} schedules",
            report.runs, report.schedules
        );
    }
}

#[test]
fn reply_races_expiry_death_and_caller_drop() {
    check(
        "reply vs expiry vs death vs drop",
        reply_vs_expiry_vs_death_vs_drop,
    );
}

#[test]
fn keyed_replacement_fences_the_completing_job() {
    check("keyed replacement", keyed_replacement);
}

#[test]
fn emits_never_follow_the_terminal_message_or_a_cancel() {
    check("emit vs terminal and cancel", emit_vs_terminal_and_cancel);
}
