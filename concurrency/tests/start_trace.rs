//! Records unified-start traces (stage 1, the startup decision, the cell,
//! notices, names, the init timer, continuations) from real starts for TLA+
//! trace validation against `verification/tla/Start.tla` (mapping in
//! `TraceStart.tla`). Each scenario writes `start_<name>.json` to
//! `$LIFECYCLE_TRACE_DIR` (default `target/lifecycle-traces`);
//! `verification/tla/trace_check.py` checks them (schema `start-v1`).
//!
//! Recording is process-wide, so the scenarios run one at a time. Every
//! scenario waits for the events it needs explicitly (the children's
//! publication and owned-work join, the parent's notices) before taking
//! the trace: nothing races the end of the recording.
#![cfg(all(feature = "lifecycle-trace", not(spawned_sim)))]

use spawned_concurrency::{
    lifecycle_trace,
    message::Message,
    tasks::{Actor, ActorStart, Context, Handler, Init, Name, StartOptions},
    ChildHandle, Down, Exit, ExitReason, PendingReply, StartError, StartOutcome,
};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

const LIMIT: Duration = Duration::from_secs(10);

/// Held for a whole scenario (also across awaits, so an async mutex).
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn trace_dir() -> PathBuf {
    std::env::var_os("LIFECYCLE_TRACE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/lifecycle-traces")
        })
}

/// Wait until `cond` holds (the trace has the events a scenario needs).
async fn until(what: &str, cond: impl Fn() -> bool) {
    let deadline = std::time::Instant::now() + LIMIT;
    while !cond() {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

fn count(action: &str) -> usize {
    lifecycle_trace::start_count(action, "")
}

/// Stop recording and write the trace; returns `action[:x]` per event.
fn write(name: &str, pplace: &str, ptrap: bool, cap: usize) -> Vec<String> {
    let events = lifecycle_trace::take_start();
    let json = format!(
        "{{\"name\":\"start_{name}\",\"schema\":\"start-v1\",\"pplace\":\"{pplace}\",\"ptrap\":{ptrap},\"cap\":{cap},\"events\":[\n{}\n]}}\n",
        events
            .iter()
            .map(|e| e.to_json())
            .collect::<Vec<_>>()
            .join(",\n")
    );
    let dir = trace_dir();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("start_{name}.json")), json).unwrap();
    events
        .iter()
        .map(|e| {
            if e.x.is_empty() {
                e.action.to_string()
            } else {
                format!("{}:{}", e.action, e.x)
            }
        })
        .collect()
}

fn has(events: &[String], wanted: &[&str]) {
    // `wanted` must appear in this order (other events may interleave).
    let mut at = 0;
    for w in wanted {
        match events[at..].iter().position(|e| e == w) {
            Some(i) => at += i + 1,
            None => panic!("{w} missing (in order) from {events:#?}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Actors
// ---------------------------------------------------------------------------

#[derive(Default, Clone)]
struct Seen {
    exits: Arc<Mutex<Vec<Exit>>>,
    downs: Arc<Mutex<Vec<Down>>>,
}

/// The model's P.
struct Parent {
    trap: bool,
    seen: Seen,
}

impl Actor for Parent {
    async fn started(&mut self, ctx: &Context<Self>) {
        if self.trap {
            ctx.trap_exit(true);
        }
    }
    async fn exit_received(&mut self, exit: Exit, _: &Context<Self>) {
        self.seen.exits.lock().unwrap().push(exit);
    }
}

impl Handler<Down> for Parent {
    async fn handle(&mut self, down: Down, reply: PendingReply<'_, ()>, _: &Context<Self>) {
        self.seen.downs.lock().unwrap().push(down);
        reply.reply(());
    }
}

struct Panic;
impl Message for Panic {
    type Result = ();
}
impl Handler<Panic> for Parent {
    async fn handle(&mut self, _: Panic, _: PendingReply<'_, ()>, _: &Context<Self>) {
        panic!("parent fails");
    }
}

struct Stop;
impl Message for Stop {
    type Result = ();
}
impl Handler<Stop> for Parent {
    async fn handle(&mut self, _: Stop, reply: PendingReply<'_, ()>, ctx: &Context<Self>) {
        ctx.stop();
        reply.reply(());
    }
}

async fn parent(trap: bool) -> (spawned_concurrency::tasks::ActorRef<Parent>, Seen) {
    let seen = Seen::default();
    let p = Parent {
        trap,
        seen: seen.clone(),
    }
    .start()
    .await
    .unwrap();
    (p, seen)
}

#[derive(Clone, Copy)]
enum Outcome {
    Ready,
    Continue,
    Error,
    Ignore,
    Panic,
    StopShutdown,
    StopPayload,
}

/// A child whose `started()` may wait on a gate, then returns `outcome`.
struct Child {
    outcome: Outcome,
    gate: Option<tokio::sync::oneshot::Receiver<()>>,
    log: Arc<Mutex<Vec<&'static str>>>,
}

impl Child {
    fn new(outcome: Outcome) -> Self {
        Self {
            outcome,
            gate: None,
            log: Arc::default(),
        }
    }
    fn gated(outcome: Outcome) -> (Self, tokio::sync::oneshot::Sender<()>) {
        let (release, gate) = tokio::sync::oneshot::channel();
        let mut child = Self::new(outcome);
        child.gate = Some(gate);
        (child, release)
    }
}

impl Actor for Child {
    async fn started(&mut self, _: &Context<Self>) -> Init<Self> {
        if let Some(gate) = self.gate.take() {
            let _ = gate.await;
        }
        match self.outcome {
            Outcome::Ready => Init::ready(),
            Outcome::Continue => Init::continue_with(Cont),
            Outcome::Error => Init::error("no config"),
            Outcome::Ignore => Init::ignore(),
            Outcome::Panic => panic!("child fails in started()"),
            Outcome::StopShutdown => Init::stop(ExitReason::Shutdown(None)),
            Outcome::StopPayload => Init::stop(ExitReason::shutdown_with("no backend")),
        }
    }
}

struct Cont;
impl Message for Cont {
    type Result = ();
}
impl Handler<Cont> for Child {
    async fn handle(&mut self, _: Cont, _: PendingReply<'_, ()>, _: &Context<Self>) {
        self.log.lock().unwrap().push("continue");
    }
}

struct Work;
impl Message for Work {
    type Result = ();
}
impl Handler<Work> for Child {
    async fn handle(&mut self, _: Work, reply: PendingReply<'_, ()>, _: &Context<Self>) {
        self.log.lock().unwrap().push("work");
        reply.reply(());
    }
}

impl Handler<Stop> for Child {
    async fn handle(&mut self, _: Stop, reply: PendingReply<'_, ()>, ctx: &Context<Self>) {
        ctx.stop();
        reply.reply(());
    }
}

struct Unlink(ChildHandle);
impl Message for Unlink {
    type Result = ();
}
impl Handler<Unlink> for Child {
    async fn handle(&mut self, msg: Unlink, reply: PendingReply<'_, ()>, ctx: &Context<Self>) {
        ctx.unlink(&msg.0);
        reply.reply(());
    }
}

/// Wait until `n` children have finished their owned work (published, jobs
/// joined): the last events of a child.
async fn children_done(n: usize) {
    until("children's owned work", || count("SWorkDone") >= n).await;
}

// ---------------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------------

/// start_link of a ready child; a message; an administrative stop (the
/// child's ordinary exit); its Normal notice reaches the non-trapping P.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn link_ready_then_stop() {
    let _serial = SERIAL.lock().await;
    let (p, _) = parent(false).await;
    lifecycle_trace::record_start_parent(&p.child_handle());
    let child = Child::new(Outcome::Ready)
        .start_link(&p.context())
        .await
        .unwrap();
    child.request(Work).await.unwrap();
    child.request(Stop).await.unwrap();
    assert_eq!(child.wait_exit().await, ExitReason::Normal);
    children_done(1).await;
    until("P's notice", || count("PNotice") >= 1).await;
    let events = write("link_ready_then_stop", "U", false, 1000);
    has(
        &events,
        &[
            "SBegin:link",
            "SCommit:U",
            "SStartedBegin",
            "SResolve:ready",
            "SAnswer:ok",
            "SSend",
            "SHandle",
            "SExit:Normal",
            "SDie:Normal",
            "SPublish:deliver",
            "PNotice:link",
            "SWorkDone",
        ],
    );
    p.request(Stop).await.unwrap();
    p.wait_exit().await;
}

/// A failed start_monitor (Init::error) consumes its Down; ready()
/// reports the same result.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn monitor_error_consumes_down() {
    let _serial = SERIAL.lock().await;
    let (p, seen) = parent(false).await;
    lifecycle_trace::record_start_parent(&p.child_handle());
    let starting = Child::new(Outcome::Error).start_monitor(&p.context());
    let child = starting.actor_ref().unwrap();
    let result = starting.await;
    assert!(
        matches!(&result, Err(StartError::InitFailed { cause, pending: None }) if cause == "no config"),
        "{result:?}"
    );
    assert!(matches!(
        child.ready().await,
        Err(StartError::InitFailed { pending: None, .. })
    ));
    children_done(1).await;
    let events = write("monitor_error_consumes_down", "U", false, 1000);
    has(
        &events,
        &[
            "SBegin:monitor",
            "SCommit:U",
            "SStartedBegin",
            "SResolve:error",
            "SDie:Normal",
            "SPublish:divert",
            "SAnswer:failed",
            "SReady:failed",
        ],
    );
    assert!(seen.downs.lock().unwrap().is_empty());
    p.request(Stop).await.unwrap();
    p.wait_exit().await;
}

/// A trapping parent: a child panicking in started() fails start_link with
/// the final reason, and the Exit is consumed (OTP's flush).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn link_panic_in_started_trapping_parent() {
    let _serial = SERIAL.lock().await;
    let (p, seen) = parent(true).await;
    lifecycle_trace::record_start_parent(&p.child_handle());
    let result = Child::new(Outcome::Panic).start_link(&p.context()).await;
    assert!(matches!(
        result,
        Err(StartError::InitExited {
            reason: ExitReason::Panic(_),
            pending: None
        })
    ));
    children_done(1).await;
    let events = write("link_panic_in_started_trapping_parent", "U", true, 1000);
    has(
        &events,
        &[
            "SBegin:link",
            "SResolve:panic",
            "SDie:Panic",
            "SPublish:divert",
            "SAnswer:failed",
        ],
    );
    assert!(seen.exits.lock().unwrap().is_empty());
    p.request(Stop).await.unwrap();
    p.wait_exit().await;
}

/// A non-trapping parent: a child stopping with Shutdown in started()
/// fails start_link, and the notice still ends the parent (OTP: the link
/// kills the caller).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn link_stop_in_started_ends_nontrapping_parent() {
    let _serial = SERIAL.lock().await;
    let (p, _) = parent(false).await;
    lifecycle_trace::record_start_parent(&p.child_handle());
    let result = Child::new(Outcome::StopShutdown)
        .start_link(&p.context())
        .await;
    assert!(
        matches!(
            result,
            Err(StartError::InitExited {
                reason: ExitReason::Shutdown(None),
                pending: None
            })
        ),
        "{result:?}"
    );
    assert_eq!(p.wait_exit().await, ExitReason::Shutdown(None));
    children_done(1).await;
    until("P's publication", || count("PPublish") >= 1).await;
    let events = write(
        "link_stop_in_started_ends_nontrapping_parent",
        "U",
        false,
        1000,
    );
    has(
        &events,
        &[
            "SResolve:stopS",
            "SDie:Shutdown",
            "SPublish:deliver",
            "PNotice:link",
            "PDie",
            "PPublish",
        ],
    );
}

/// Names: committed with the start, found by a typed lookup, refused to a
/// competitor, released before publication, free again after.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn named_start_clash_and_release() {
    let _serial = SERIAL.lock().await;
    let (p, _) = parent(false).await;
    lifecycle_trace::record_start_parent(&p.child_handle());
    static NAME: Name<Child> = Name::new("start-trace-name");
    let first = Child::new(Outcome::Ready)
        .with_options(StartOptions::new().name(NAME.clone()))
        .start()
        .await
        .unwrap();
    assert_eq!(NAME.whereis().map(|a| a.id()), Some(first.id()));
    let clash = Child::new(Outcome::Ready)
        .with_options(StartOptions::new().name(NAME.clone()))
        .start()
        .await;
    match clash {
        Err(StartError::AlreadyStarted { existing, .. }) => {
            assert_eq!(existing.map(|h| h.id()), Some(first.id()))
        }
        other => panic!("{other:?}"),
    }
    first.request(Stop).await.unwrap();
    first.wait_exit().await;
    assert!(NAME.whereis().is_none());
    let again = Child::new(Outcome::Ready)
        .with_options(StartOptions::new().name(NAME.clone()))
        .start()
        .await
        .unwrap();
    again.request(Stop).await.unwrap();
    again.wait_exit().await;
    children_done(2).await;
    let events = write("named_start_clash_and_release", "U", false, 1000);
    has(
        &events,
        &[
            "SCommit:U",
            "SLookup:hit",
            "SRefuse:already",
            "SReleaseName",
            "SPublish:none",
            "SLookup:miss",
            "SCommit:U",
        ],
    );
    p.request(Stop).await.unwrap();
    p.wait_exit().await;
}

/// spawn + ready() for an ignoring child, and a waiting start that is
/// ignored.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ignore_answers_ignored() {
    let _serial = SERIAL.lock().await;
    let (p, _) = parent(false).await;
    lifecycle_trace::record_start_parent(&p.child_handle());
    let spawned = Child::new(Outcome::Ignore).spawn();
    assert!(matches!(
        spawned.ready().await,
        Err(StartError::Ignored { pending: None })
    ));
    assert_eq!(spawned.wait_exit().await, ExitReason::Normal);
    let waited = StartOutcome::from_result(Child::new(Outcome::Ignore).start().await).unwrap();
    assert!(waited.is_ignored());
    // Invalid options: refused before anything exists.
    assert!(matches!(
        Child::new(Outcome::Ready)
            .with_options(StartOptions::new().init_timeout(Duration::ZERO))
            .spawn(),
        Err(StartError::InvalidOptions(_))
    ));
    children_done(2).await;
    let events = write("ignore_answers_ignored", "U", false, 1000);
    has(
        &events,
        &[
            "SBegin:spawn",
            "SResolve:ignore",
            "SReady:ignored",
            "SBegin:start",
            "SResolve:ignore",
            "SAnswer:ignored",
        ],
    );
    p.request(Stop).await.unwrap();
    p.wait_exit().await;
}

/// The continuation runs before the messages sent while started() ran.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn continuation_precedes_queued_messages() {
    let _serial = SERIAL.lock().await;
    let (p, _) = parent(false).await;
    lifecycle_trace::record_start_parent(&p.child_handle());
    let (child, release) = Child::gated(Outcome::Continue);
    let log = child.log.clone();
    let child = child.spawn();
    until("started() began", || count("SStartedBegin") >= 1).await;
    child.send(Work).unwrap();
    child.send(Work).unwrap();
    release.send(()).unwrap();
    child.request(Stop).await.unwrap();
    child.wait_exit().await;
    children_done(1).await;
    assert_eq!(*log.lock().unwrap(), ["continue", "work", "work"]);
    let events = write("continuation_precedes_queued_messages", "U", false, 1000);
    has(
        &events,
        &[
            "SStartedBegin",
            "SSend",
            "SSend",
            "SResolve:continue",
            "SContinue",
            "SHandle",
            "SHandle",
        ],
    );
    p.request(Stop).await.unwrap();
    p.wait_exit().await;
}

/// The init timer wins over a stuck started(): the waiting start answers
/// InitTimeout after publication; the trapping parent's Exit is consumed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn init_timeout_waiting() {
    let _serial = SERIAL.lock().await;
    let (p, seen) = parent(true).await;
    lifecycle_trace::record_start_parent(&p.child_handle());
    let (child, release) = Child::gated(Outcome::Ready);
    let starting = child
        .with_options(StartOptions::new().init_timeout(Duration::from_millis(20)))
        .start_link(&p.context());
    until("the timer", || count("STimer") >= 1).await;
    release.send(()).unwrap();
    let result = starting.await;
    assert!(
        matches!(result, Err(StartError::InitTimeout { pending: None })),
        "{result:?}"
    );
    children_done(1).await;
    let events = write("init_timeout_waiting", "U", true, 1000);
    has(
        &events,
        &[
            "SStartedBegin",
            "STimer:won",
            "SResolve:ready",
            "SDie:Killed",
            "SPublish:divert",
            "SAnswer:timeout",
        ],
    );
    assert!(seen.exits.lock().unwrap().is_empty());
    p.request(Stop).await.unwrap();
    p.wait_exit().await;
}

/// An abandoned start_link whose child then times out: the diverted
/// notice is handed on, and the trapping parent gets the Exit (Killed).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn abandoned_timeout_delivers_killed() {
    let _serial = SERIAL.lock().await;
    let (p, seen) = parent(true).await;
    lifecycle_trace::record_start_parent(&p.child_handle());
    let (child, release) = Child::gated(Outcome::Ready);
    let starting = child
        .with_options(StartOptions::new().init_timeout(Duration::from_millis(20)))
        .start_link(&p.context());
    let child = starting.actor_ref().unwrap();
    drop(starting);
    until("the timer", || count("STimer") >= 1).await;
    release.send(()).unwrap();
    assert_eq!(child.wait_exit().await, ExitReason::Killed);
    children_done(1).await;
    until("P's notice", || count("PNotice") >= 1).await;
    until("the Exit", || !seen.exits.lock().unwrap().is_empty()).await;
    let events = write("abandoned_timeout_delivers_killed", "U", true, 1000);
    has(
        &events,
        &[
            "SAbandon",
            "STimer:won",
            "SResolve:ready",
            "SPublish:deliver",
            "PNotice:link",
        ],
    );
    assert_eq!(seen.exits.lock().unwrap()[0].reason, ExitReason::Killed);
    p.request(Stop).await.unwrap();
    p.wait_exit().await;
}

/// A kill while started() runs fails the start (row 2, no stopped()); an
/// administrative stop waits for started(): the start succeeds, then the
/// actor exits through stopped() (OTP gen_server:stop).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kill_and_admin_stop_during_started() {
    let _serial = SERIAL.lock().await;
    let (p, _) = parent(false).await;
    lifecycle_trace::record_start_parent(&p.child_handle());
    let (child, release) = Child::gated(Outcome::Ready);
    let starting = child.start();
    let handle = starting.actor_ref().unwrap().child_handle();
    until("started() began", || count("SStartedBegin") >= 1).await;
    handle.kill();
    release.send(()).unwrap();
    assert!(matches!(
        starting.await,
        Err(StartError::InitExited {
            reason: ExitReason::Killed,
            pending: None
        })
    ));
    let (child, release) = Child::gated(Outcome::Ready);
    let starting = child.start();
    let handle = starting.actor_ref().unwrap().child_handle();
    until("started() began", || count("SStartedBegin") >= 2).await;
    handle.stop_with_reason(ExitReason::Shutdown(None));
    release.send(()).unwrap();
    // OTP gen_server:stop: the stop waits for started(); the start succeeds,
    // then the actor exits with the stop's reason through stopped().
    let child = match starting.await {
        Ok(child) => child,
        Err(error) => panic!("{error:?}"),
    };
    assert_eq!(child.wait_exit().await, ExitReason::Shutdown(None));
    children_done(2).await;
    let events = write("kill_and_admin_stop_during_started", "U", false, 1000);
    has(
        &events,
        &[
            "SStartedBegin",
            "SKill",
            "SResolve:ready",
            "SDie:Killed",
            "SAnswer:failed",
            "SStartedBegin",
            "SAdminStop",
            "SResolve:ready",
            "SExit:Shutdown",
            "SDie:Shutdown",
        ],
    );
    // The ready answer races the stop's death; it follows the ready resolve.
    has(&events, &["SAdminStop", "SResolve:ready", "SAnswer:ok"]);
    p.request(Stop).await.unwrap();
    p.wait_exit().await;
}

/// The parent's abnormal exit reaches a linked child still in started():
/// held, it fails the start (row 3). The waiter here is outside P.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parent_exit_during_child_started() {
    let _serial = SERIAL.lock().await;
    let (p, _) = parent(false).await;
    lifecycle_trace::record_start_parent(&p.child_handle());
    let (child, release) = Child::gated(Outcome::Ready);
    let starting = child.start_link(&p.context());
    until("started() began", || count("SStartedBegin") >= 1).await;
    let _ = p.send(Panic);
    assert!(matches!(p.wait_exit().await, ExitReason::Panic(_)));
    until("P's publication", || count("PPublish") >= 1).await;
    // A start through the dead parent's context is refused.
    assert!(matches!(
        Child::new(Outcome::Ready).start_link(&p.context()).await,
        Err(StartError::ParentExited)
    ));
    release.send(()).unwrap();
    assert!(matches!(
        starting.await,
        Err(StartError::InitExited {
            reason: ExitReason::Panic(_),
            pending: None
        })
    ));
    children_done(1).await;
    let events = write("parent_exit_during_child_started", "U", false, 1000);
    has(
        &events,
        &[
            "SStartedBegin",
            "PExitBegin:Panic",
            "PDie",
            "PPublish",
            "SResolve:ready",
            "SDie:Panic",
            "SAnswer:failed",
        ],
    );
}

/// A ready child unlinks from P, then dies: no notice reaches P.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unlinked_child_sends_no_notice() {
    let _serial = SERIAL.lock().await;
    let (p, seen) = parent(true).await;
    lifecycle_trace::record_start_parent(&p.child_handle());
    let child = Child::new(Outcome::Ready)
        .start_link(&p.context())
        .await
        .unwrap();
    child.request(Unlink(p.child_handle())).await.unwrap();
    child.request(Stop).await.unwrap();
    child.wait_exit().await;
    children_done(1).await;
    let events = write("unlinked_child_sends_no_notice", "U", true, 1000);
    has(
        &events,
        &["SAnswer:ok", "SUnlink", "SExit:Normal", "SPublish:deliver"],
    );
    assert!(!events.iter().any(|e| e.starts_with("PNotice")));
    assert!(seen.exits.lock().unwrap().is_empty());
    p.request(Stop).await.unwrap();
    p.wait_exit().await;
}

/// A managed parent in a thread group: link and monitor children inherit
/// its group; a full group refuses (Capacity); the slot is released at the
/// scope's completion, after publication.
#[cfg(not(spawned_sim))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inherited_thread_group_capacity() {
    let _serial = SERIAL.lock().await;
    let system = spawned_concurrency::system::ActorSystem::new().unwrap();
    let group = system.thread_group("start-trace", 2).unwrap();
    let seen = Seen::default();
    let p = Parent {
        trap: false,
        seen: seen.clone(),
    }
    .with_options(&group)
    .start()
    .await
    .unwrap();
    lifecycle_trace::record_start_parent(&p.child_handle());
    let first = Child::new(Outcome::Ready)
        .start_link(&p.context())
        .await
        .unwrap();
    let full = Child::new(Outcome::Ready).start_monitor(&p.context()).await;
    assert!(matches!(full, Err(StartError::Capacity)), "{full:?}");
    first.request(Stop).await.unwrap();
    first.wait_exit().await;
    until("the slot's release", || count("SReleaseSlot") >= 1).await;
    let (second, _monitor) = Child::new(Outcome::Ready)
        .start_monitor(&p.context())
        .await
        .unwrap();
    second.request(Stop).await.unwrap();
    second.wait_exit().await;
    until("both slots", || count("SReleaseSlot") >= 2).await;
    until("the Down", || !seen.downs.lock().unwrap().is_empty()).await;
    let events = write("inherited_thread_group_capacity", "G", false, 2);
    has(
        &events,
        &[
            "SBegin:link",
            "SCommit:G",
            "SAnswer:ok",
            "SBegin:monitor",
            "SRefuse:capacity",
            "SPublish:deliver",
            "SWorkDone",
            "SReleaseSlot",
            "SBegin:monitor",
            "SCommit:G",
            "SPublish:deliver",
            "PNotice:monitor",
        ],
    );
    p.request(Stop).await.unwrap();
    p.wait_exit().await;
    assert!(system.shutdown(LIMIT).await.complete);
}

/// A failed start_link whose wait is abandoned after publication: the Exit
/// diverted to the waiter is handed on to the trapping parent, exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn abandon_after_publication_hands_on_exit() {
    let _serial = SERIAL.lock().await;
    let (p, seen) = parent(true).await;
    lifecycle_trace::record_start_parent(&p.child_handle());
    let starting = Child::new(Outcome::Panic).start_link(&p.context());
    let child = starting.actor_ref().unwrap();
    assert!(matches!(child.wait_exit().await, ExitReason::Panic(_)));
    until("the diversion", || count("SPublish") >= 1).await;
    drop(starting);
    until("the Exit", || !seen.exits.lock().unwrap().is_empty()).await;
    children_done(1).await;
    let events = write("abandon_after_publication_hands_on_exit", "U", true, 1000);
    has(
        &events,
        &[
            "SResolve:panic",
            "SPublish:divert",
            "SAbandon",
            "PNotice:link",
        ],
    );
    assert_eq!(seen.exits.lock().unwrap().len(), 1);
    p.request(Stop).await.unwrap();
    p.wait_exit().await;
}

/// The ignoring child's destructor hangs: the waiter answers `Ignored`
/// with `pending` at the cleanup deadline (5 s), before publication.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stuck_ignore_answers_pending() {
    struct HangsInDrop(std::sync::mpsc::Receiver<()>);
    impl Drop for HangsInDrop {
        fn drop(&mut self) {
            let _ = self.0.recv();
        }
    }
    struct Stuck(#[allow(dead_code)] Option<HangsInDrop>);
    impl Actor for Stuck {
        async fn started(&mut self, _: &Context<Self>) -> Init<Self> {
            Init::ignore()
        }
    }
    let _serial = SERIAL.lock().await;
    let (p, _) = parent(false).await;
    lifecycle_trace::record_start_parent(&p.child_handle());
    let (release, hang) = std::sync::mpsc::channel();
    let pending = match Stuck(Some(HangsInDrop(hang))).start().await {
        Err(StartError::Ignored {
            pending: Some(pending),
        }) => pending,
        other => panic!("{other:?}"),
    };
    release.send(()).unwrap();
    assert_eq!(pending.wait_exit_async().await, ExitReason::Normal);
    children_done(1).await;
    let events = write("stuck_ignore_answers_pending", "U", false, 1000);
    has(
        &events,
        &[
            "SResolve:ignore",
            "SDie:Normal",
            "SDeadline",
            "SAnswer:ignored",
            "SPublish:none",
        ],
    );
    p.request(Stop).await.unwrap();
    p.wait_exit().await;
}

/// Threads mode: a blocking start_link that fails, and a ready child with a
/// continuation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn threads_mode_link_and_continuation() {
    use spawned_concurrency::threads;
    let _serial = SERIAL.lock().await;
    struct TParent;
    impl threads::Actor for TParent {}
    struct TChild(bool, Arc<Mutex<Vec<&'static str>>>);
    impl threads::Actor for TChild {
        fn started(&mut self, _: &threads::Context<Self>) -> threads::Init<Self> {
            if self.0 {
                threads::Init::error("bad")
            } else {
                threads::Init::continue_with(Cont)
            }
        }
    }
    impl threads::Handler<Cont> for TChild {
        fn handle(&mut self, _: Cont, _: PendingReply<'_, ()>, _: &threads::Context<Self>) {
            self.1.lock().unwrap().push("continue");
        }
    }
    impl threads::Handler<Stop> for TChild {
        fn handle(&mut self, _: Stop, reply: PendingReply<'_, ()>, ctx: &threads::Context<Self>) {
            ctx.stop();
            reply.reply(());
        }
    }
    let p = threads::ActorStart::spawn(TParent);
    lifecycle_trace::record_start_parent(&p.child_handle());
    let ctx = p.context();
    let log: Arc<Mutex<Vec<&'static str>>> = Arc::default();
    let (failed, ready) = tokio::task::spawn_blocking({
        let log = log.clone();
        move || {
            let failed = threads::ActorStart::start_link(TChild(true, log.clone()), &ctx);
            let ready = threads::ActorStart::start_link(TChild(false, log), &ctx).unwrap();
            ready.request(Stop).unwrap();
            ready.wait_exit();
            (failed, ready)
        }
    })
    .await
    .unwrap();
    assert!(matches!(failed, Err(StartError::InitFailed { .. })));
    assert_eq!(*log.lock().unwrap(), ["continue"]);
    drop(ready);
    children_done(2).await;
    let events = write("threads_mode_link_and_continuation", "U", false, 1000);
    has(
        &events,
        &[
            "SResolve:error",
            "SPublish:drop",
            "SAnswer:failed",
            "SResolve:continue",
            "SContinue",
            "SExit:Normal",
        ],
    );
    p.context().stop();
    p.wait_exit();
}

/// Review #291 item 1: a committed managed child whose dispatch fails (its
/// group dropped mid-shutdown; failpoint `system.dispatch_actor`) is killed
/// before started() (`KilledEarly`): the start_link answers
/// `InitExited { Killed }` (not a stage-1 refusal with the edge applied),
/// the waiting trapping parent's Exit is consumed by the answer, and the
/// start_monitor's Down likewise. No waiter stays `Waiting`.
#[cfg(all(feature = "failpoints", not(spawned_sim)))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_failure_after_commit_kills_child() {
    use spawned_concurrency::failpoints::{self, Action, FailPoint, FailScenario};
    let _serial = SERIAL.lock().await;
    let _scenario = FailScenario::setup();
    let system = spawned_concurrency::system::ActorSystem::new().unwrap();
    let group = system.thread_group("start-dispatch", 3).unwrap();
    let seen = Seen::default();
    let p = Parent {
        trap: true,
        seen: seen.clone(),
    }
    .with_options(&group)
    .start()
    .await
    .unwrap();
    lifecycle_trace::record_start_parent(&p.child_handle());
    failpoints::configure("system.dispatch_actor", FailPoint::new(Action::Error)).unwrap();
    let linked = tokio::time::timeout(LIMIT, Child::new(Outcome::Ready).start_link(&p.context()))
        .await
        .expect("the start_link answers");
    assert!(
        matches!(
            linked,
            Err(StartError::InitExited {
                reason: ExitReason::Killed,
                pending: None
            })
        ),
        "{linked:?}"
    );
    let monitored = tokio::time::timeout(
        LIMIT,
        Child::new(Outcome::Ready).start_monitor(&p.context()),
    )
    .await
    .expect("the start_monitor answers");
    assert!(
        matches!(
            monitored,
            Err(StartError::InitExited {
                reason: ExitReason::Killed,
                pending: None
            })
        ),
        "{monitored:?}"
    );
    failpoints::remove("system.dispatch_actor");
    until("both slots", || count("SReleaseSlot") >= 2).await;
    let events = write("dispatch_failure_after_commit_kills_child", "G", true, 2);
    has(
        &events,
        &[
            "SBegin:link",
            "SCommit:G",
            "SKill",
            "SResolve:none",
            "SDie:Killed",
            "SPublish:divert",
            "SAnswer:failed",
            "SBegin:monitor",
            "SCommit:G",
            "SKill",
            "SResolve:none",
            "SPublish:divert",
            "SAnswer:failed",
        ],
    );
    assert!(
        seen.exits.lock().unwrap().is_empty(),
        "Exit consumed by the answer"
    );
    assert!(
        seen.downs.lock().unwrap().is_empty(),
        "Down consumed by the answer"
    );
    assert!(p.child_handle().is_alive(), "a trapping parent survives");
    p.request(Stop).await.unwrap();
    p.wait_exit().await;
    assert!(system.shutdown(LIMIT).await.complete);
}

/// Review #291 item 1: a committed `Backend::Thread` child whose thread
/// cannot be spawned (failpoint `actor.thread_spawn`) is killed before
/// started(), and its start_link answers `InitExited { Killed }` instead of
/// panicking in the parent.
#[cfg(feature = "failpoints")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thread_spawn_failure_after_commit_kills_child() {
    use spawned_concurrency::failpoints::{self, Action, FailPoint, FailScenario};
    let _serial = SERIAL.lock().await;
    let _scenario = FailScenario::setup();
    let (p, seen) = parent(true).await;
    lifecycle_trace::record_start_parent(&p.child_handle());
    failpoints::configure("actor.thread_spawn", FailPoint::new(Action::Error)).unwrap();
    let linked = Child::new(Outcome::Ready)
        .with_options(StartOptions::new().backend(spawned_concurrency::tasks::Backend::Thread))
        .start_link(&p.context())
        .await;
    failpoints::remove("actor.thread_spawn");
    assert!(
        matches!(
            linked,
            Err(StartError::InitExited {
                reason: ExitReason::Killed,
                pending: None
            })
        ),
        "{linked:?}"
    );
    children_done(1).await;
    let events = write(
        "thread_spawn_failure_after_commit_kills_child",
        "U",
        true,
        1000,
    );
    has(
        &events,
        &[
            "SBegin:link",
            "SCommit:U",
            "SKill",
            "SResolve:none",
            "SDie:Killed",
            "SPublish:divert",
            "SAnswer:failed",
        ],
    );
    assert!(seen.exits.lock().unwrap().is_empty());
    p.request(Stop).await.unwrap();
    p.wait_exit().await;
}

/// Starts a gated child linked to the parent from the parent's own turn and
/// reports the start's result.
type Done = tokio::sync::oneshot::Sender<Result<(), StartError>>;
struct LinkGated(Mutex<Option<Child>>, Mutex<Option<Done>>);
impl Message for LinkGated {
    type Result = ();
}
impl Handler<LinkGated> for Parent {
    async fn handle(&mut self, msg: LinkGated, reply: PendingReply<'_, ()>, ctx: &Context<Self>) {
        reply.reply(());
        let child = msg.0.lock().unwrap().take().unwrap();
        let result = child.start_link(ctx).await.map(|_| ());
        let done = msg.1.lock().unwrap().take();
        if let Some(done) = done {
            let _ = done.send(result);
        }
    }
}

/// Review #291 item 7: a non-trapping parent killed while it awaits a
/// `start_link` does not wait for the child's `started()` (OTP: the caller
/// dies at once): the wait is abandoned (`ParentExited`), the parent's turn
/// ends and it dies `Killed`; the child, still in `started()`, holds the
/// parent's exit and fails its start when `started()` returns.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parent_killed_while_awaiting_start_link_abandons_the_wait() {
    let _serial = SERIAL.lock().await;
    let (p, _) = parent(false).await;
    lifecycle_trace::record_start_parent(&p.child_handle());
    let (child, release) = Child::gated(Outcome::Ready);
    let (done_tx, done) = tokio::sync::oneshot::channel();
    p.request(LinkGated(
        Mutex::new(Some(child)),
        Mutex::new(Some(done_tx)),
    ))
    .await
    .unwrap();
    until("the child's started()", || count("SStartedBegin") >= 1).await;
    p.child_handle().kill();
    let result = tokio::time::timeout(LIMIT, done).await.unwrap().unwrap();
    assert!(
        matches!(result, Err(StartError::ParentExited)),
        "{result:?}"
    );
    assert_eq!(
        tokio::time::timeout(LIMIT, p.wait_exit()).await.unwrap(),
        ExitReason::Killed,
        "the parent died without waiting for started()"
    );
    release.send(()).unwrap();
    children_done(1).await;
    let events = write(
        "parent_killed_while_awaiting_start_link_abandons_the_wait",
        "U",
        false,
        1000,
    );
    has(
        &events,
        &[
            "SBegin:link",
            "SCommit:U",
            "SStartedBegin",
            "PExitBegin:Killed",
            "SAbandon",
            "PDie",
            "SResolve:ready",
            "SDie:Killed",
        ],
    );
}

/// `{shutdown, Term}` (G7): `Init::stop(shutdown_with(..))` fails the start
/// with the full reason, and the start trace classes it as `Shutdown`
/// (the model's `stopS`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn init_stop_with_shutdown_payload() {
    let _serial = SERIAL.lock().await;
    let (p, _) = parent(true).await;
    lifecycle_trace::record_start_parent(&p.child_handle());
    let result = Child::new(Outcome::StopPayload)
        .start_link(&p.context())
        .await;
    match &result {
        Err(StartError::InitExited {
            reason,
            pending: None,
        }) => {
            assert!(reason.is_shutdown());
            assert_eq!(reason.shutdown_payload(), Some("no backend"));
        }
        other => panic!("{other:?}"),
    }
    children_done(1).await;
    let events = write("init_stop_with_shutdown_payload", "U", true, 1000);
    has(
        &events,
        &[
            "SResolve:stopS",
            "SDie:Shutdown",
            "SPublish:divert",
            "SAnswer:failed",
        ],
    );
    p.request(Stop).await.unwrap();
    p.wait_exit().await;
}

/// A child whose `started()` fails and whose destructor is slow.
struct SlowDropChild;
impl Actor for SlowDropChild {
    async fn started(&mut self, _: &Context<Self>) -> Result<(), String> {
        Err("no config".into())
    }
}
impl Drop for SlowDropChild {
    fn drop(&mut self) {
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A failed start_link is answered after the child's publication, even
/// with a slow destructor and the waiter on another worker (design 5.5;
/// the model's `FailureAfterPublication`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_start_answers_after_a_slow_drop() {
    let _serial = SERIAL.lock().await;
    let (p, _) = parent(true).await;
    lifecycle_trace::record_start_parent(&p.child_handle());
    let result = SlowDropChild.start_link(&p.context()).await;
    assert!(
        matches!(result, Err(StartError::InitFailed { pending: None, .. })),
        "{result:?}"
    );
    children_done(1).await;
    let events = write("failed_start_answers_after_a_slow_drop", "U", true, 1000);
    has(
        &events,
        &[
            "SResolve:error",
            "SDie:Normal",
            "SPublish:divert",
            "SAnswer:failed",
        ],
    );
    p.request(Stop).await.unwrap();
    p.wait_exit().await;
}
