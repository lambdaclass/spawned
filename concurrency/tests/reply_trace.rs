//! Records reply-slot traces from real actors for TLA+ trace validation
//! against `verification/tla/DeferredReplies.tla` (verification/tla/README.md).
//! Each scenario writes `replies_<name>.json` to `$LIFECYCLE_TRACE_DIR`
//! (default `target/lifecycle-traces`); `verification/tla/trace_check.py`
//! checks them (schema `replies-v1`).
#![cfg(feature = "lifecycle-trace")]

use spawned_concurrency::{
    lifecycle_trace,
    message::Message,
    supervision::{ChildSpec, SupervisorConfig},
    system::ActorSystem,
    tasks::{
        Actor, ActorRef, ActorStart, Context, Deadline, Handler, JobError, JobOptions, JobOutcome,
        PendingReply, ReplyTo,
    },
    ActorError, ExitReason,
};
use spawned_rt::tasks::sleep;
use std::{path::PathBuf, time::Duration};

const BOUND: usize = 1;

fn trace_dir() -> PathBuf {
    std::env::var_os("LIFECYCLE_TRACE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/lifecycle-traces")
        })
}

#[derive(Default)]
struct Owner {
    stored: Vec<ReplyTo<u32>>,
    /// The downstream actor B its jobs call.
    backend: Option<ActorRef<Backend>>,
}

impl Actor for Owner {
    const MAX_OUTSTANDING: usize = BOUND;
}

/// Replied in its turn.
struct Echo(u32);
impl Message for Echo {
    type Result = u32;
}
impl Handler<Echo> for Owner {
    async fn handle(&mut self, msg: Echo, reply: PendingReply<'_, u32>, _: &Context<Self>) {
        reply.reply(msg.0);
    }
}

/// Deferred and stored for a later turn (`Overloaded` at the bound).
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

/// Never answered: the unused obligation answers `ReplyDropped` at turn end.
struct Ignore;
impl Message for Ignore {
    type Result = u32;
}
impl Handler<Ignore> for Owner {
    async fn handle(&mut self, _: Ignore, _reply: PendingReply<'_, u32>, _: &Context<Self>) {}
}

/// Deferred, then the turn panics: the token is held until the death point,
/// which answers it `ActorStopped(Panic(..))`.
struct Boom;
impl Message for Boom {
    type Result = u32;
}
impl Handler<Boom> for Owner {
    async fn handle(&mut self, _: Boom, reply: PendingReply<'_, u32>, _: &Context<Self>) {
        let _token = reply.defer();
        panic!("boom");
    }
}

/// A sent (not requested) message: answer, or drop, every stored token.
enum Flush {
    Reply,
    Drop,
}
impl Message for Flush {
    type Result = ();
}
impl Handler<Flush> for Owner {
    async fn handle(&mut self, msg: Flush, _: PendingReply<'_, ()>, _: &Context<Self>) {
        for token in self.stored.drain(..) {
            match msg {
                Flush::Reply => {
                    token.reply(7);
                }
                Flush::Drop => drop(token),
            }
        }
    }
}

/// Wait until every message sent before has been handled.
struct Sync;
impl Message for Sync {
    type Result = ();
}
impl Handler<Sync> for Owner {
    async fn handle(&mut self, _: Sync, reply: PendingReply<'_, ()>, _: &Context<Self>) {
        reply.reply(());
    }
}

/// Deferred, its deadline shortened, then stored.
struct ShortHold;
impl Message for ShortHold {
    type Result = u32;
}
impl Handler<ShortHold> for Owner {
    async fn handle(&mut self, _: ShortHold, reply: PendingReply<'_, u32>, _: &Context<Self>) {
        if let Ok(token) = reply.defer() {
            token.shorten_deadline(Duration::from_secs(3));
            self.stored.push(token);
        }
    }
}

/// Deferred into a `pipe_reply` job: `work_ms` of work under an optional job
/// deadline (`deadline_ms`), optionally cancelled when the caller gives up;
/// with `downstream`, the work asks the backend B (`Ask`, or `AskNow`).
struct Piped {
    work_ms: u64,
    deadline_ms: Option<u64>,
    cancel_on_gone: bool,
    downstream: Option<bool>,
}
impl Message for Piped {
    type Result = u32;
}
/// The job's notification, with the token back.
struct JobDone {
    res: JobOutcome<u32>,
    token: ReplyTo<u32>,
}
impl Message for JobDone {
    type Result = ();
}
impl Handler<Piped> for Owner {
    async fn handle(&mut self, msg: Piped, reply: PendingReply<'_, u32>, ctx: &Context<Self>) {
        let Ok(token) = reply.defer() else { return };
        let mut opts = JobOptions::new();
        if let Some(ms) = msg.deadline_ms {
            opts = opts.deadline(Duration::from_millis(ms));
        }
        if msg.cancel_on_gone {
            opts = opts.cancel_on_caller_gone();
        }
        let backend = msg
            .downstream
            .map(|later| (later, self.backend.clone().unwrap()));
        let work_ms = msg.work_ms;
        let piped = ctx.pipe_reply(
            opts,
            token,
            move |_| async move {
                let got = match backend {
                    Some((true, b)) => b.request(Ask).await.unwrap_or(0),
                    Some((false, b)) => b.request(AskNow).await.unwrap_or(0),
                    None => 5,
                };
                sleep(Duration::from_millis(work_ms)).await;
                got
            },
            |res, token| JobDone { res, token },
        );
        if let Err(e) = piped {
            drop(e.into_token());
        }
    }
}
impl Handler<JobDone> for Owner {
    async fn handle(&mut self, msg: JobDone, _: PendingReply<'_, ()>, ctx: &Context<Self>) {
        match msg.res {
            Ok(v) => {
                msg.token.reply(v);
            }
            Err(JobError::DeadlineExceeded) => {
                // The work still runs and holds the request's credit: a
                // second job needs a fresh one, and BOUND = 1 refuses it.
                let again = ctx.pipe_reply(
                    JobOptions::new(),
                    msg.token,
                    |_| async { 1u32 },
                    |res, token| JobDone { res, token },
                );
                match again {
                    Ok(_) => panic!("a second job found a free credit"),
                    Err(e) => {
                        e.into_token().reply(9);
                    }
                }
            }
            Err(_) => drop(msg.token),
        }
    }
}

/// The downstream actor B: defers `Ask` until `Release`, answers `AskNow`.
#[derive(Default)]
struct Backend {
    held: Vec<ReplyTo<u32>>,
}
impl Actor for Backend {
    const MAX_OUTSTANDING: usize = 1;
}
struct Ask;
impl Message for Ask {
    type Result = u32;
}
struct AskNow;
impl Message for AskNow {
    type Result = u32;
}
struct Release;
impl Message for Release {
    type Result = ();
}
struct Held;
impl Message for Held {
    type Result = usize;
}
impl Handler<Ask> for Backend {
    async fn handle(&mut self, _: Ask, reply: PendingReply<'_, u32>, _: &Context<Self>) {
        if let Ok(token) = reply.defer() {
            self.held.push(token);
        }
    }
}
impl Handler<AskNow> for Backend {
    async fn handle(&mut self, _: AskNow, reply: PendingReply<'_, u32>, _: &Context<Self>) {
        reply.reply(4);
    }
}
impl Handler<Release> for Backend {
    async fn handle(&mut self, _: Release, _: PendingReply<'_, ()>, _: &Context<Self>) {
        for token in self.held.drain(..) {
            token.reply(8);
        }
    }
}
impl Handler<Held> for Backend {
    async fn handle(&mut self, _: Held, reply: PendingReply<'_, usize>, _: &Context<Self>) {
        reply.reply(self.held.len());
    }
}

/// Start an owner whose reply slots are traced.
fn start() -> ActorRef<Owner> {
    let actor = Owner::default().start();
    lifecycle_trace::watch_deferred(id(&actor));
    actor
}

fn id(actor: &ActorRef<Owner>) -> u64 {
    actor.id().to_string().parse().unwrap()
}

/// Write the owner's reply trace once it is dead.
fn write(name: &str, owner: u64) -> Vec<String> {
    let events = lifecycle_trace::take_replies(owner);
    assert!(
        events.iter().any(|e| e.action == "Death"),
        "{name}: the owner's death was not recorded"
    );
    let json = format!(
        "{{\"name\":\"replies_{name}\",\"schema\":\"replies-v1\",\"bound\":{BOUND},\"events\":[\n{}\n]}}\n",
        events
            .iter()
            .map(|e| e.to_json())
            .collect::<Vec<_>>()
            .join(",\n")
    );
    let dir = trace_dir();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("replies_{name}.json")), json).unwrap();
    events
        .iter()
        .map(|e| {
            format!(
                "{}{}",
                e.action,
                if e.x.is_empty() {
                    String::new()
                } else {
                    format!(":{}", e.x)
                }
            )
        })
        .collect()
}

async fn stop(name: &str, actor: ActorRef<Owner>) -> Vec<String> {
    let owner = id(&actor);
    actor.context().stop();
    actor.join().await;
    write(name, owner)
}

#[tokio::test]
async fn reply_in_turn() {
    let actor = start();
    let got = actor.request(Echo(3)).await;
    assert!(matches!(got, Ok(3)), "{got:?}");
    let events = stop("in_turn", actor).await;
    assert_eq!(
        events,
        ["Send", "Dispatch", "Discharge:ok", "Exit", "Death"]
    );
}

#[tokio::test]
async fn stored_token_answered_later() {
    let actor = start();
    let held = actor
        .send_request(Hold, spawned_concurrency::DEFAULT_REQUEST_TIMEOUT)
        .unwrap();
    actor.request(Sync).await.unwrap();
    actor.send(Flush::Reply).unwrap();
    let got = held.await;
    assert!(matches!(got, Ok(7)), "{got:?}");
    stop("later_reply", actor).await;
}

#[tokio::test]
async fn stored_token_dropped_later() {
    let actor = start();
    let held = actor
        .send_request(Hold, spawned_concurrency::DEFAULT_REQUEST_TIMEOUT)
        .unwrap();
    actor.request(Sync).await.unwrap();
    actor.send(Flush::Drop).unwrap();
    let got = held.await;
    assert!(matches!(got, Err(ActorError::ReplyDropped)), "{got:?}");
    stop("later_drop", actor).await;
}

#[tokio::test]
async fn unused_obligation_answers_dropped() {
    let actor = start();
    let got = actor.request(Ignore).await;
    assert!(matches!(got, Err(ActorError::ReplyDropped)), "{got:?}");
    stop("unused", actor).await;
}

#[tokio::test]
async fn defer_at_the_bound_is_overloaded() {
    let actor = start();
    let first = actor
        .send_request(Hold, spawned_concurrency::DEFAULT_REQUEST_TIMEOUT)
        .unwrap();
    let got = actor.request(Hold).await;
    assert!(matches!(got, Err(ActorError::Overloaded)), "{got:?}");
    actor.send(Flush::Reply).unwrap();
    let got = first.await;
    assert!(matches!(got, Ok(7)), "{got:?}");
    stop("overloaded", actor).await;
}

#[tokio::test]
async fn stored_token_expires_at_its_deadline() {
    let actor = start();
    let deadline = Deadline::At(spawned_rt::time::Instant::now() + Duration::from_millis(30));
    let held = actor.send_request(Hold, deadline).unwrap();
    let got = held.await;
    assert!(matches!(got, Err(ActorError::RequestTimeout)), "{got:?}");
    actor.send(Flush::Reply).unwrap();
    actor.request(Sync).await.unwrap();
    stop("expired", actor).await;
}

#[tokio::test]
async fn caller_gives_up() {
    let actor = start();
    let held = actor
        .send_request(Hold, spawned_concurrency::DEFAULT_REQUEST_TIMEOUT)
        .unwrap();
    actor.request(Sync).await.unwrap();
    drop(held);
    actor.send(Flush::Reply).unwrap();
    actor.request(Sync).await.unwrap();
    stop("abandoned", actor).await;
}

#[tokio::test]
async fn death_closes_stored_tokens() {
    let actor = start();
    let held = actor
        .send_request(Hold, spawned_concurrency::DEFAULT_REQUEST_TIMEOUT)
        .unwrap();
    actor.request(Sync).await.unwrap();
    let owner = id(&actor);
    actor.context().stop();
    let got = held.await;
    assert!(
        matches!(got, Err(ActorError::ActorStopped(ExitReason::Normal))),
        "{got:?}"
    );
    actor.join().await;
    let events = write("death", owner);
    assert!(
        !events.iter().any(|e| e == "Discharge:stopped"),
        "{events:?}"
    );
}

#[tokio::test]
async fn panic_in_turn_answers_stopped() {
    let actor = start();
    // The token unwound by the panic is exit pending: answered at the death
    // point with the final reason, not by a discharge of its own.
    let got = actor.request(Boom).await;
    assert!(
        matches!(got, Err(ActorError::ActorStopped(ExitReason::Panic(_)))),
        "{got:?}"
    );
    actor.join().await;
    sleep(Duration::from_millis(10)).await;
    write("panic", id(&actor));
}

#[tokio::test]
async fn shortened_deadline_then_reply() {
    let actor = start();
    let held = actor
        .send_request(ShortHold, spawned_concurrency::DEFAULT_REQUEST_TIMEOUT)
        .unwrap();
    actor.request(Sync).await.unwrap();
    actor.send(Flush::Reply).unwrap();
    assert!(matches!(held.await, Ok(7)));
    let events = stop("shorten", actor).await;
    in_order(
        &events,
        &["Dispatch", "Shorten", "Discharge:ok", "Exit", "Death"],
    );
}

/// `expected` occurs in `events` in this order.
fn in_order(events: &[String], expected: &[&str]) {
    let mut rest = events.iter();
    for want in expected {
        assert!(
            rest.any(|e| e == want),
            "{want} missing or out of order: {events:?}"
        );
    }
}

fn piped(work_ms: u64) -> Piped {
    Piped {
        work_ms,
        deadline_ms: None,
        cancel_on_gone: false,
        downstream: None,
    }
}

#[tokio::test]
async fn job_result_answers_the_caller() {
    let actor = start();
    assert!(matches!(actor.request(piped(1)).await, Ok(5)));
    let events = stop("job_done", actor).await;
    assert_eq!(
        events,
        [
            "Send",
            "Dispatch",
            "PipeReply:linked",
            "JobNotify:done",
            "Discharge:ok",
            "Exit",
            "Death"
        ]
    );
}

#[tokio::test]
async fn job_deadline_hands_the_token_back() {
    let actor = start();
    let got = actor
        .request(Piped {
            deadline_ms: Some(20),
            ..piped(150)
        })
        .await;
    assert!(matches!(got, Ok(9)), "{got:?}");
    // The work runs on past its deadline, then ends quietly.
    sleep(Duration::from_millis(250)).await;
    let events = stop("job_deadline", actor).await;
    assert_eq!(
        events,
        [
            "Send",
            "Dispatch",
            "PipeReply:linked",
            "JobNotify:deadline",
            "PipeReply:refused",
            "Discharge:ok",
            "JobEnd",
            "Exit",
            "Death"
        ]
    );
}

#[tokio::test]
async fn caller_gone_cancels_the_job() {
    let actor = start();
    let held = actor
        .send_request(
            Piped {
                cancel_on_gone: true,
                ..piped(10_000)
            },
            spawned_concurrency::DEFAULT_REQUEST_TIMEOUT,
        )
        .unwrap();
    actor.request(Sync).await.unwrap();
    drop(held);
    sleep(Duration::from_millis(50)).await;
    let events = stop("job_cancelled", actor).await;
    in_order(
        &events,
        &[
            "PipeReply:linked",
            "Discharge:abandoned",
            "JobCancel",
            "JobEnd",
            "Exit",
            "Death",
        ],
    );
}

#[tokio::test]
async fn owner_exit_cancels_the_job() {
    let actor = start();
    let held = actor
        .send_request(piped(10_000), spawned_concurrency::DEFAULT_REQUEST_TIMEOUT)
        .unwrap();
    actor.request(Sync).await.unwrap();
    let owner = id(&actor);
    actor.context().stop();
    let got = held.await;
    assert!(
        matches!(got, Err(ActorError::ActorStopped(ExitReason::Normal))),
        "{got:?}"
    );
    actor.join().await;
    sleep(Duration::from_millis(50)).await;
    let events = write("job_exit", owner);
    in_order(&events, &["PipeReply:linked", "Exit", "JobCancel"]);
    in_order(&events, &["JobCancel", "JobEnd"]);
    in_order(&events, &["JobCancel", "Death"]);
}

/// Start an owner with a backend B for its jobs.
fn start_with_backend() -> (ActorRef<Owner>, ActorRef<Backend>) {
    let backend = Backend::default().start();
    let actor = Owner {
        backend: Some(backend.clone()),
        ..Owner::default()
    }
    .start();
    lifecycle_trace::watch_deferred(id(&actor));
    (actor, backend)
}

#[tokio::test]
async fn downstream_request_deferred_then_answered() {
    let (actor, backend) = start_with_backend();
    let held = actor
        .send_request(
            Piped {
                downstream: Some(true),
                ..piped(1)
            },
            spawned_concurrency::DEFAULT_REQUEST_TIMEOUT,
        )
        .unwrap();
    while backend.request(Held).await.unwrap() == 0 {
        sleep(Duration::from_millis(2)).await;
    }
    backend.send(Release).unwrap();
    assert!(matches!(held.await, Ok(8)));
    let events = stop("downstream_later", actor).await;
    assert_eq!(
        events,
        [
            "Send",
            "Dispatch",
            "PipeReply:linked",
            "CallB",
            "BDispatch:stateB",
            "BReply:ok",
            "JobNotify:done",
            "Discharge:ok",
            "Exit",
            "Death"
        ]
    );
}

#[tokio::test]
async fn downstream_request_answered_in_its_turn() {
    let (actor, _backend) = start_with_backend();
    let got = actor
        .request(Piped {
            downstream: Some(false),
            ..piped(1)
        })
        .await;
    assert!(matches!(got, Ok(4)), "{got:?}");
    let events = stop("downstream_now", actor).await;
    assert_eq!(
        events,
        [
            "Send",
            "Dispatch",
            "PipeReply:linked",
            "CallB",
            "BDispatch:done",
            "JobNotify:done",
            "Discharge:ok",
            "Exit",
            "Death"
        ]
    );
}

/// A supervised child that traps exits (so its parent's shutdown runs
/// `stopped()`) and whose `stopped()` outlasts two shutdown timeouts.
struct Slow;
impl Actor for Slow {
    async fn started(&mut self, ctx: &Context<Self>) {
        ctx.trap_exit(true);
    }
    async fn stopped(&mut self, _: &Context<Self>) {
        sleep(Duration::from_millis(600)).await;
    }
}
impl Handler<Echo> for Slow {
    async fn handle(&mut self, msg: Echo, reply: PendingReply<'_, u32>, _: &Context<Self>) {
        reply.reply(msg.0);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cleanup_deadline_before_death_reports_stuck() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("reply-trace-stuck", 1).unwrap();
    let mut tree = system.supervisor(SupervisorConfig::default()).unwrap();
    let child = tree
        .add(
            ChildSpec::new("slow", &group, |_| async { Ok::<_, String>(Slow) })
                .shutdown_timeout(Duration::from_millis(100)),
        )
        .unwrap();
    let _supervisor = tree.start().await.unwrap();
    let actor = child.current().unwrap();
    let owner: u64 = actor.id().to_string().parse().unwrap();
    lifecycle_trace::watch_deferred(owner);
    assert!(matches!(actor.request(Echo(2)).await, Ok(2)));
    let report = system.shutdown(Duration::from_secs(5)).await;
    assert!(report.complete);
    let events = write("stuck", owner);
    assert_eq!(
        events,
        ["Send", "Dispatch", "Discharge:ok", "Exit", "Stuck", "Death"]
    );
}

/// Threads mode: a blocking linked job asks a backend B, which answers in
/// its turn; the job's result answers the caller.
mod threads_mode {
    use super::{in_order, trace_dir};
    use spawned_concurrency::{
        lifecycle_trace,
        message::Message,
        threads::{
            Actor, ActorRef, ActorStart, Context, Handler, JobOptions, JobOutcome, PendingReply,
            ReplyTo,
        },
    };

    struct Owner(ActorRef<Backend>);
    impl Actor for Owner {
        const MAX_OUTSTANDING: usize = super::BOUND;
    }
    struct Backend;
    impl Actor for Backend {}

    struct Ask;
    impl Message for Ask {
        type Result = u32;
    }
    impl Handler<Ask> for Backend {
        fn handle(&mut self, _: Ask, reply: PendingReply<'_, u32>, _: &Context<Self>) {
            reply.reply(4);
        }
    }

    struct Piped;
    impl Message for Piped {
        type Result = u32;
    }
    struct JobDone {
        res: JobOutcome<u32>,
        token: ReplyTo<u32>,
    }
    impl Message for JobDone {
        type Result = ();
    }
    impl Handler<Piped> for Owner {
        fn handle(&mut self, _: Piped, reply: PendingReply<'_, u32>, ctx: &Context<Self>) {
            let Ok(token) = reply.defer() else { return };
            let backend = self.0.clone();
            if let Err(e) = ctx.pipe_reply_blocking(
                JobOptions::new(),
                token,
                move |_| backend.request(Ask).unwrap_or(0),
                |res, token| JobDone { res, token },
            ) {
                drop(e.into_token());
            }
        }
    }
    impl Handler<JobDone> for Owner {
        fn handle(&mut self, msg: JobDone, _: PendingReply<'_, ()>, _: &Context<Self>) {
            match msg.res {
                Ok(v) => {
                    msg.token.reply(v);
                }
                Err(_) => drop(msg.token),
            }
        }
    }

    #[test]
    fn blocking_job_asks_downstream() {
        let backend = Backend.start();
        let actor = Owner(backend).start();
        let owner: u64 = actor.id().to_string().parse().unwrap();
        lifecycle_trace::watch_deferred(owner);
        assert!(matches!(actor.request(Piped), Ok(4)));
        actor.context().stop();
        actor.join();
        let events = lifecycle_trace::take_replies(owner);
        let json = format!(
            "{{\"name\":\"replies_threads_downstream\",\"schema\":\"replies-v1\",\"bound\":{},\"events\":[\n{}\n]}}\n",
            super::BOUND,
            events.iter().map(|e| e.to_json()).collect::<Vec<_>>().join(",\n")
        );
        let dir = trace_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("replies_threads_downstream.json"), json).unwrap();
        let names: Vec<String> = events
            .iter()
            .map(|e| {
                if e.x.is_empty() {
                    e.action.to_string()
                } else {
                    format!("{}:{}", e.action, e.x)
                }
            })
            .collect();
        in_order(
            &names,
            &[
                "Send",
                "Dispatch",
                "PipeReply:linked",
                "CallB",
                "BDispatch:done",
                "JobNotify:done",
                "Discharge:ok",
                "Exit",
                "Death",
            ],
        );
    }
}
