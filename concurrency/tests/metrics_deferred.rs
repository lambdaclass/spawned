//! `spawned_deferred_*` and `spawned_jobs_*` metric families: every deferred
//! token and job outcome is counted once, and the gauges return to zero.
//! One test per binary: the counters are process-wide.
#![cfg(feature = "async")]
#![cfg(feature = "metrics")]
use spawned_concurrency::{
    message::Message,
    metrics::{self, Config},
    tasks::{
        Actor, ActorRef, ActorStart, Context, Deadline, Handler, JobCtx, JobOptions, JobOutcome,
        PendingReply, ReplyTo,
    },
    ActorError, ExitReason,
};
use spawned_rt::tasks::{oneshot, sleep};
use std::time::Duration;

#[derive(Default)]
struct Owner {
    stored: Vec<ReplyTo<u32>>,
    gates: Vec<oneshot::Sender<()>>,
}
impl Actor for Owner {
    const MAX_OUTSTANDING: usize = 2;
    const MAX_NATIVE_JOBS: Option<usize> = Some(1);
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

enum Flush {
    Reply,
    Drop,
}
impl Message for Flush {
    type Result = ();
}
impl Handler<Flush> for Owner {
    async fn handle(&mut self, msg: Flush, reply: PendingReply<'_, ()>, _: &Context<Self>) {
        for token in self.stored.drain(..) {
            match msg {
                Flush::Reply => {
                    token.reply(1);
                }
                Flush::Drop => drop(token),
            }
        }
        reply.reply(());
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

struct Done;
impl Message for Done {
    type Result = ();
}
impl Handler<Done> for Owner {
    async fn handle(&mut self, _: Done, _: PendingReply<'_, ()>, _: &Context<Self>) {}
}
fn done<T>(_: JobOutcome<T>) -> Done {
    Done
}

enum Jobs {
    Ok,
    Panics,
    Deadline,
    Emits,
    /// A native job finished, then cancelled: its queued result is fenced.
    Fenced,
    /// A gated native job holds the budget; a second one is rejected.
    Budget,
    Cancel,
}
impl Message for Jobs {
    type Result = ();
}
impl Handler<Jobs> for Owner {
    async fn handle(&mut self, msg: Jobs, reply: PendingReply<'_, ()>, ctx: &Context<Self>) {
        match msg {
            Jobs::Ok => drop(ctx.pipe(JobOptions::new(), |_| async { 1 }, done).unwrap()),
            Jobs::Panics => drop(
                ctx.pipe(JobOptions::new(), |_| async { panic!("job") }, done::<()>)
                    .unwrap(),
            ),
            Jobs::Deadline => {
                let opts = JobOptions::new().deadline(Duration::from_millis(10));
                ctx.pipe(opts, |_| sleep(Duration::from_millis(60)), done)
                    .unwrap();
            }
            Jobs::Emits => {
                ctx.pipe(
                    JobOptions::new(),
                    |job: JobCtx<Owner>| async move {
                        job.emit(Done).await.unwrap();
                        job.emit(Done).await.unwrap();
                    },
                    done,
                )
                .unwrap();
            }
            Jobs::Fenced => {
                let job = ctx
                    .pipe_blocking(JobOptions::new(), |_: JobCtx<Owner>| 1, done)
                    .unwrap();
                while job.is_running() {
                    std::thread::sleep(Duration::from_millis(1));
                }
                job.cancel();
            }
            Jobs::Budget => {
                let (open, gate) = std::sync::mpsc::channel::<()>();
                ctx.pipe_blocking(
                    JobOptions::new(),
                    move |_: JobCtx<Owner>| gate.recv().ok(),
                    done,
                )
                .unwrap();
                assert!(ctx
                    .pipe_blocking(JobOptions::new(), |_: JobCtx<Owner>| (), done)
                    .is_err());
                open.send(()).unwrap();
            }
            Jobs::Cancel => {
                let (open, gate) = oneshot::channel::<()>();
                let job = ctx
                    .pipe(
                        JobOptions::new(),
                        |_| async move { gate.await.is_ok() },
                        done,
                    )
                    .unwrap();
                job.cancel();
                self.gates.push(open);
            }
        }
        reply.reply(());
    }
}

/// Wait until no job runs and their messages have been dispatched.
async fn settle(actor: &ActorRef<Owner>) {
    for _ in 0..400 {
        if metrics::snapshot().jobs_running == 0 {
            break;
        }
        sleep(Duration::from_millis(5)).await;
    }
    actor.request(Sync).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deferred_and_job_families_count_each_outcome_once() {
    metrics::configure(Config::default().timing_sample_every(1)).unwrap();
    let before = metrics::snapshot();
    let actor = Owner::default().start();

    // Replied, and Overloaded at the bound.
    let first = actor
        .send_request(Hold, spawned_concurrency::DEFAULT_REQUEST_TIMEOUT)
        .unwrap();
    let second = actor
        .send_request(Hold, spawned_concurrency::DEFAULT_REQUEST_TIMEOUT)
        .unwrap();
    assert!(matches!(
        actor.request(Hold).await,
        Err(ActorError::Overloaded)
    ));
    actor.request(Flush::Reply).await.unwrap();
    assert!(matches!(first.await, Ok(1)));
    assert!(matches!(second.await, Ok(1)));
    // Expired, dropped, abandoned.
    let deadline = Deadline::At(spawned_rt::time::Instant::now() + Duration::from_millis(20));
    let expiring = actor.send_request(Hold, deadline).unwrap();
    assert!(matches!(expiring.await, Err(ActorError::RequestTimeout)));
    let dropped = actor
        .send_request(Hold, spawned_concurrency::DEFAULT_REQUEST_TIMEOUT)
        .unwrap();
    actor.request(Flush::Drop).await.unwrap();
    assert!(matches!(dropped.await, Err(ActorError::ReplyDropped)));
    let abandoned = actor
        .send_request(Hold, spawned_concurrency::DEFAULT_REQUEST_TIMEOUT)
        .unwrap();
    actor.request(Sync).await.unwrap();
    drop(abandoned);
    actor.request(Flush::Reply).await.unwrap();

    for jobs in [
        Jobs::Ok,
        Jobs::Panics,
        Jobs::Deadline,
        Jobs::Emits,
        Jobs::Fenced,
        Jobs::Budget,
        Jobs::Cancel,
    ] {
        actor.request(jobs).await.unwrap();
        // One job at a time: they share the actor's two credits.
        settle(&actor).await;
    }

    // Stopped: a token still stored at death.
    let stopped = actor
        .send_request(Hold, spawned_concurrency::DEFAULT_REQUEST_TIMEOUT)
        .unwrap();
    actor.request(Flush::Reply).await.unwrap();
    let late = actor
        .send_request(Hold, spawned_concurrency::DEFAULT_REQUEST_TIMEOUT)
        .unwrap();
    sleep(Duration::from_millis(20)).await;
    actor.context().stop();
    assert!(matches!(
        late.await,
        Err(ActorError::ActorStopped(ExitReason::Normal))
    ));
    assert!(matches!(stopped.await, Ok(1)));
    actor.join().await;

    let after = metrics::snapshot();
    let d = |f: fn(&metrics::Snapshot) -> u64| f(&after) - f(&before);
    assert_eq!(d(|s| s.deferred_tokens_total), 7, "{after:#?}");
    assert_eq!(after.deferred_pending, before.deferred_pending);
    assert_eq!(d(|s| s.deferred_replied_total), 3);
    assert_eq!(d(|s| s.deferred_overloaded_total), 1);
    assert_eq!(d(|s| s.deferred_expired_total), 1);
    assert_eq!(d(|s| s.deferred_dropped_total), 1);
    assert_eq!(d(|s| s.deferred_abandoned_total), 1);
    assert_eq!(d(|s| s.deferred_stopped_total), 1);
    assert_eq!(
        after.deferred_reply_latency.count - before.deferred_reply_latency.count,
        3
    );
    assert_eq!(d(|s| s.jobs_started_total), 7);
    assert_eq!(after.jobs_running, before.jobs_running);
    // Ok, Emits, Budget's gated job, Fenced's (taken before the cancel).
    assert_eq!(d(|s| s.jobs_completed_total), 4);
    assert_eq!(d(|s| s.jobs_panicked_total), 1);
    assert_eq!(d(|s| s.jobs_deadline_exceeded_total), 1);
    assert_eq!(d(|s| s.jobs_emits_total), 2);
    assert_eq!(d(|s| s.jobs_fenced_total), 1);
    assert_eq!(d(|s| s.jobs_rejected_total), 1);
    // Only Cancel's job was running when cancelled (Fenced's had finished;
    // the owner's exit finds none running).
    assert_eq!(d(|s| s.jobs_cancelled_total), 1);
    let text = metrics::render_prometheus();
    for family in [
        "spawned_deferred_pending ",
        "spawned_deferred_replied_total ",
        "spawned_deferred_reply_latency_seconds_count ",
        "spawned_jobs_running ",
        "spawned_jobs_fenced_total ",
    ] {
        assert!(text.contains(family), "{family} missing");
    }
}
