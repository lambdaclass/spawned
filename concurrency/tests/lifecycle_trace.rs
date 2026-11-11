//! Records base-lifecycle traces (exit signals, links, monitors, requests,
//! the death point and publication) from real actors for TLA+ trace
//! validation against `verification/tla/Lifecycle.tla` (mapping in
//! `TraceLifecycle.tla`). Each scenario writes `lifecycle_<name>.json` to
//! `$LIFECYCLE_TRACE_DIR` (default `target/lifecycle-traces`);
//! `verification/tla/trace_check.py` checks them (schema `lifecycle-v1`).
//!
//! Recording is process-wide, so the scenarios run one at a time.
#![cfg(feature = "lifecycle-trace")]

use spawned_concurrency::{
    lifecycle_trace,
    message::Message,
    tasks::{Actor, ActorRef, ActorStart, Backend, Context, Handler},
    ActorError, ChildHandle, Down, Exit, ExitReason, MonitorRef, PendingReply,
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

/// Stop recording and write the trace; returns the events as
/// `action[:x]` of the actor numbered by spawn order (`a1`, `a2`, ...).
fn write(name: &str) -> Vec<String> {
    let events = lifecycle_trace::take_lifecycle();
    let json = format!(
        "{{\"name\":\"lifecycle_{name}\",\"schema\":\"lifecycle-v1\",\"events\":[\n{}\n]}}\n",
        events
            .iter()
            .map(|e| e.to_json())
            .collect::<Vec<_>>()
            .join(",\n")
    );
    let dir = trace_dir();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("lifecycle_{name}.json")), json).unwrap();
    let actors: Vec<u64> = events
        .iter()
        .filter(|e| e.action == "Spawn")
        .map(|e| e.t)
        .collect();
    let index = |id: u64| actors.iter().position(|a| *a == id).map_or(0, |i| i + 1);
    events
        .iter()
        .filter(|e| e.action != "Spawn")
        .map(|e| {
            let x = if e.x.is_empty() {
                String::new()
            } else {
                format!(":{}", e.x)
            };
            format!("a{}.{}{x}", index(e.t), e.action)
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

type Log = Arc<Mutex<Vec<String>>>;

fn logged(log: &Log) -> Vec<String> {
    log.lock().unwrap().clone()
}

async fn wait_for(log: &Log, entry: &str) {
    spawned_rt::tasks::timeout(LIMIT, async {
        while !log.lock().unwrap().iter().any(|e| e == entry) {
            spawned_rt::tasks::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{entry} never logged: {:?}", logged(log)));
}

struct Node {
    trap: bool,
    log: Log,
    /// `stopped()` waits for it.
    hold_stopped: Option<tokio::sync::oneshot::Receiver<()>>,
    /// The state destructor waits for it (a native thread: Backend::Thread).
    hold_drop: Option<std::sync::mpsc::Receiver<()>>,
}

impl Node {
    fn new(log: &Log) -> Self {
        Self {
            trap: false,
            log: log.clone(),
            hold_stopped: None,
            hold_drop: None,
        }
    }
    fn trapping(log: &Log) -> Self {
        let mut node = Self::new(log);
        node.trap = true;
        node
    }
    fn holding_stopped(log: &Log, gate: tokio::sync::oneshot::Receiver<()>) -> Self {
        let mut node = Self::new(log);
        node.hold_stopped = Some(gate);
        node
    }
    fn holding_drop(gate: std::sync::mpsc::Receiver<()>) -> Self {
        let mut node = Self::new(&Log::default());
        node.hold_drop = Some(gate);
        node
    }
    fn note(&self, entry: impl Into<String>) {
        self.log.lock().unwrap().push(entry.into());
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        if let Some(gate) = self.hold_drop.take() {
            let _ = gate.recv_timeout(LIMIT);
        }
    }
}

impl Actor for Node {
    async fn started(&mut self, ctx: &Context<Self>) {
        if self.trap {
            ctx.trap_exit(true);
        }
    }
    async fn stopped(&mut self, _: &Context<Self>) {
        self.note("stopped");
        if let Some(gate) = self.hold_stopped.take() {
            let _ = gate.await;
        }
    }
    async fn exit_received(&mut self, exit: Exit, _: &Context<Self>) {
        self.note(format!("exit:{:?}", exit.reason));
    }
}

impl Handler<Down> for Node {
    async fn handle(&mut self, down: Down, _: PendingReply<'_, ()>, _: &Context<Self>) {
        self.note(format!("down:{:?}", down.reason));
    }
}

macro_rules! request {
    ($name:ident ( $($field:ty),* ) -> $result:ty, |$this:ident, $msg:ident, $ctx:ident| $body:expr) => {
        struct $name($(pub $field),*);
        impl Message for $name {
            type Result = $result;
        }
        impl Handler<$name> for Node {
            async fn handle(
                &mut self,
                $msg: $name,
                reply: PendingReply<'_, $result>,
                $ctx: &Context<Self>,
            ) {
                let $this = self;
                let result = $body;
                reply.reply(result);
            }
        }
    };
}

request!(Sync() -> (), |_this, _msg, _ctx| ());
request!(Link(ChildHandle) -> (), |_this, msg, ctx| ctx.link(&msg.0));
request!(TryLink(ChildHandle) -> bool, |_this, msg, ctx| ctx.try_link(&msg.0).is_ok());
request!(Unlink(ChildHandle) -> (), |_this, msg, ctx| ctx.unlink(&msg.0));
request!(Monitor(ChildHandle) -> MonitorRef, |_this, msg, ctx| ctx.monitor(&msg.0));
request!(Demonitor(MonitorRef) -> (), |_this, msg, ctx| ctx.demonitor(msg.0));

/// A request whose handler panics.
struct Boom();
impl Message for Boom {
    type Result = ();
}
impl Handler<Boom> for Node {
    async fn handle(&mut self, _: Boom, _: PendingReply<'_, ()>, _: &Context<Self>) {
        panic!("boom");
    }
}

/// A request whose handler waits for a gate.
struct Block(tokio::sync::oneshot::Receiver<()>);
impl Message for Block {
    type Result = ();
}
impl Handler<Block> for Node {
    async fn handle(&mut self, msg: Block, reply: PendingReply<'_, ()>, _: &Context<Self>) {
        self.note("blocked");
        let _ = msg.0.await;
        reply.reply(());
    }
}

async fn exit_of<A: Actor>(actor: &ActorRef<A>) -> ExitReason {
    spawned_rt::tasks::timeout(LIMIT, actor.wait_exit())
        .await
        .expect("actor did not exit")
}

/// A kill ends a running actor `Killed` without `stopped()`; the request
/// queued behind the current handler is answered at D with `Killed`, and a
/// request after D gets `NoProc`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kill_answers_queued_request_at_death() {
    let _serial = SERIAL.lock().await;
    lifecycle_trace::record_lifecycle();
    let log = Log::default();
    let a1 = Node::new(&log).start();
    let (release, gate) = tokio::sync::oneshot::channel();
    let blocked = a1.request_raw(Block(gate)).unwrap();
    wait_for(&log, "blocked").await;
    let queued = a1.request_raw(Sync()).unwrap();
    a1.child_handle().kill();
    release.send(()).unwrap();
    assert!(blocked.await.is_ok());
    assert!(matches!(
        queued.await,
        Err(ActorError::ActorStopped(ExitReason::Killed))
    ));
    assert_eq!(exit_of(&a1).await, ExitReason::Killed);
    assert!(matches!(a1.request(Sync()).await, Err(ActorError::NoProc)));
    assert!(!logged(&log).contains(&"stopped".to_string()));
    let events = write("kill_queued_request");
    has(
        &events,
        &[
            "a1.Dispatch",
            "a1.Send:queued",
            "a1.Signal:Kill",
            "a1.Hook:skip",
            "a1.Die:Killed",
            "a1.Publish",
            "a1.Send:noproc",
        ],
    );
}

/// A kill during a running `stopped()` replaces the requested reason; the
/// `stopped()` that began finishes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kill_during_stopped() {
    let _serial = SERIAL.lock().await;
    lifecycle_trace::record_lifecycle();
    let log = Log::default();
    let (release, gate) = tokio::sync::oneshot::channel();
    let a1 = Node::holding_stopped(&log, gate).start();
    a1.request(Sync()).await.unwrap();
    a1.child_handle().stop();
    wait_for(&log, "stopped").await;
    a1.child_handle().kill();
    release.send(()).unwrap();
    assert_eq!(exit_of(&a1).await, ExitReason::Killed);
    let events = write("kill_during_stopped");
    has(
        &events,
        &[
            "a1.Stop:Normal",
            "a1.Hook:run",
            "a1.Signal:Kill",
            "a1.Die:Killed",
            "a1.Publish",
        ],
    );
}

/// An untrapped non-Normal exit signal ends the actor with its reason and
/// skips `stopped()`; a Normal one is ignored.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn untrapped_signal_skips_stopped() {
    let _serial = SERIAL.lock().await;
    lifecycle_trace::record_lifecycle();
    let log = Log::default();
    let a1 = Node::new(&log).start();
    a1.request(Sync()).await.unwrap();
    a1.child_handle().exit(ExitReason::Normal);
    a1.request(Sync()).await.unwrap();
    a1.child_handle().exit(ExitReason::Shutdown);
    assert_eq!(exit_of(&a1).await, ExitReason::Shutdown);
    assert!(!logged(&log).contains(&"stopped".to_string()));
    let events = write("untrapped_signal");
    has(
        &events,
        &[
            "a1.Signal:Normal",
            "a1.Dispatch",
            "a1.Signal:Shutdown",
            "a1.Hook:skip",
            "a1.Die:Shutdown",
            "a1.Publish",
        ],
    );
}

/// A trapping actor gets exit signals as `Exit` messages, from outside and
/// from another actor, then stops normally through `stopped()`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trapped_exit_reaches_exit_received() {
    let _serial = SERIAL.lock().await;
    lifecycle_trace::record_lifecycle();
    let log = Log::default();
    let a1 = Node::trapping(&log).start();
    let a2 = Node::new(&log).start();
    a1.request(Sync()).await.unwrap();
    a2.request(Sync()).await.unwrap();
    a1.child_handle().exit(ExitReason::Shutdown);
    wait_for(&log, "exit:Shutdown").await;
    a2.context().exit(&a1.child_handle(), ExitReason::Kill);
    // The second signal was a kill: untrappable, a1 ends Killed.
    assert_eq!(exit_of(&a1).await, ExitReason::Killed);
    a2.child_handle().stop();
    assert_eq!(exit_of(&a2).await, ExitReason::Normal);
    let events = write("trapped_exit");
    has(
        &events,
        &[
            "a1.Trap:true",
            "a1.Signal:Shutdown",
            "a1.Dispatch",
            "a1.Signal:Kill",
            "a1.Die:Killed",
            "a2.Stop:Normal",
            "a2.Hook:run",
            "a2.Die:Normal",
        ],
    );
}

/// `ctx.exit` to a trapping actor queues an `Exit` that `exit_received`
/// handles; the actor keeps running until stopped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trapped_exit_then_stop() {
    let _serial = SERIAL.lock().await;
    lifecycle_trace::record_lifecycle();
    let log = Log::default();
    let a1 = Node::trapping(&log).start();
    a1.request(Sync()).await.unwrap();
    a1.child_handle().exit(ExitReason::Shutdown);
    wait_for(&log, "exit:Shutdown").await;
    a1.child_handle().stop();
    assert_eq!(exit_of(&a1).await, ExitReason::Normal);
    assert!(logged(&log).contains(&"stopped".to_string()));
    let events = write("trapped_exit_then_stop");
    has(
        &events,
        &[
            "a1.Trap:true",
            "a1.Signal:Shutdown",
            "a1.Dispatch",
            "a1.Stop:Normal",
            "a1.Hook:run",
            "a1.Die:Normal",
            "a1.Publish",
        ],
    );
}

/// Link, try_link and monitor against an actor past D: a trapping caller
/// gets `Exit{NoProc}`, `try_link` returns `Err` and keeps nothing, the
/// monitor delivers `Down{NoProc}`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn late_registrations_get_noproc() {
    let _serial = SERIAL.lock().await;
    lifecycle_trace::record_lifecycle();
    let log = Log::default();
    let a1 = Node::trapping(&log).start();
    let a2 = Node::new(&Log::default()).start();
    a2.child_handle().stop();
    assert_eq!(exit_of(&a2).await, ExitReason::Normal);
    a1.request(Link(a2.child_handle())).await.unwrap();
    assert!(!a1.request(TryLink(a2.child_handle())).await.unwrap());
    a1.request(Monitor(a2.child_handle())).await.unwrap();
    wait_for(&log, "exit:NoProc").await;
    wait_for(&log, "down:NoProc").await;
    a1.child_handle().stop();
    assert_eq!(exit_of(&a1).await, ExitReason::Normal);
    let events = write("late_registrations");
    has(
        &events,
        &[
            "a2.Die:Normal",
            "a2.Publish",
            "a1.Link:noproc",
            "a1.Deliver:NoProc",
            "a1.Link:try",
            "a1.Monitor:late",
            "a1.Deliver:NoProc",
            "a1.Die:Normal",
        ],
    );
}

/// A non-trapping caller of link to an actor past D exits `NoProc`
/// without `stopped()`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn late_link_ends_a_non_trapping_caller() {
    let _serial = SERIAL.lock().await;
    lifecycle_trace::record_lifecycle();
    let log = Log::default();
    let a1 = Node::new(&log).start();
    let a2 = Node::new(&Log::default()).start();
    a2.child_handle().stop();
    exit_of(&a2).await;
    a1.request(Link(a2.child_handle())).await.unwrap();
    assert_eq!(exit_of(&a1).await, ExitReason::NoProc);
    assert!(!logged(&log).contains(&"stopped".to_string()));
    let events = write("late_link_untrapped");
    has(
        &events,
        &[
            "a1.Link:noproc",
            "a1.Deliver:NoProc",
            "a1.Hook:skip",
            "a1.Die:NoProc",
        ],
    );
}

/// Unlink between the peer's D and its publication cancels the notice taken
/// at D (E6): no `Exit` reaches the trapping caller.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unlink_fences_a_notice_taken_at_death() {
    let _serial = SERIAL.lock().await;
    lifecycle_trace::record_lifecycle();
    let log = Log::default();
    let a1 = Node::trapping(&log).start();
    let (release, gate) = std::sync::mpsc::channel();
    let a2 = Node::holding_drop(gate).start_with_backend(Backend::Thread);
    a1.request(Link(a2.child_handle())).await.unwrap();
    a2.request(Sync()).await.unwrap();
    a2.context().stop_with_reason(ExitReason::Shutdown);
    // Past D (requests get NoProc), destruction held: not yet published.
    past_death(&a2).await;
    a1.request(Unlink(a2.child_handle())).await.unwrap();
    release.send(()).unwrap();
    assert_eq!(exit_of(&a2).await, ExitReason::Shutdown);
    a1.request(Sync()).await.unwrap();
    assert!(
        !logged(&log).iter().any(|e| e.starts_with("exit:")),
        "{:?}",
        logged(&log)
    );
    a1.child_handle().stop();
    exit_of(&a1).await;
    let events = write("unlink_fence");
    has(
        &events,
        &[
            "a1.Link:linked",
            "a2.Die:Shutdown",
            "a1.Unlink",
            "a2.Publish",
            "a1.Die:Normal",
        ],
    );
    assert!(!events.iter().any(|e| e.starts_with("a1.Deliver")));
}

/// Demonitor between the target's D and its publication: no `Down`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn demonitor_fences_a_down() {
    let _serial = SERIAL.lock().await;
    lifecycle_trace::record_lifecycle();
    let log = Log::default();
    let a1 = Node::new(&log).start();
    let (release, gate) = std::sync::mpsc::channel();
    let a2 = Node::holding_drop(gate).start_with_backend(Backend::Thread);
    let monitor = a1.request(Monitor(a2.child_handle())).await.unwrap();
    a2.child_handle().stop();
    past_death(&a2).await;
    a1.request(Demonitor(monitor)).await.unwrap();
    release.send(()).unwrap();
    exit_of(&a2).await;
    spawned_rt::tasks::sleep(Duration::from_millis(20)).await;
    a1.request(Sync()).await.unwrap();
    assert!(
        !logged(&log).iter().any(|e| e.starts_with("down:")),
        "{:?}",
        logged(&log)
    );
    a1.child_handle().stop();
    exit_of(&a1).await;
    let events = write("demonitor_fence");
    has(
        &events,
        &[
            "a1.Monitor:active",
            "a2.Die:Normal",
            "a1.Demonitor",
            "a2.Publish",
            "a1.Die:Normal",
        ],
    );
    assert!(!events.iter().any(|e| e.starts_with("a1.Deliver")));
}

async fn past_death(actor: &ActorRef<Node>) {
    spawned_rt::tasks::timeout(LIMIT, async {
        while !matches!(actor.request(Sync()).await, Err(ActorError::NoProc)) {
            spawned_rt::tasks::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("never reached its death point");
}

/// A monitor of a running actor delivers `Down` with the final reason after
/// publication.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn monitor_delivers_the_final_reason() {
    let _serial = SERIAL.lock().await;
    lifecycle_trace::record_lifecycle();
    let log = Log::default();
    let a1 = Node::new(&log).start();
    let a2 = Node::new(&Log::default()).start();
    a1.request(Monitor(a2.child_handle())).await.unwrap();
    a2.child_handle().stop_with_reason(ExitReason::Shutdown);
    wait_for(&log, "down:Shutdown").await;
    a1.child_handle().stop();
    exit_of(&a1).await;
    let events = write("monitor_down");
    has(
        &events,
        &[
            "a1.Monitor:active",
            "a2.Stop:Shutdown",
            "a2.Die:Shutdown",
            "a2.Publish",
            "a1.Deliver:Shutdown",
            "a1.Dispatch",
            "a1.Die:Normal",
        ],
    );
}

/// The `start_linked` parent's exit ends a trapping child normally: a
/// parent exit, `stopped()` runs, and the child ends with the parent's
/// reason (E3).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parent_exit_ends_a_trapping_child_through_stopped() {
    let _serial = SERIAL.lock().await;
    lifecycle_trace::record_lifecycle();
    let log = Log::default();
    let a1 = Node::new(&Log::default()).start();
    a1.request(Sync()).await.unwrap();
    let a2 = Node::trapping(&log).start_linked(&a1.context());
    a2.request(Sync()).await.unwrap();
    a1.child_handle().stop_with_reason(ExitReason::Shutdown);
    assert_eq!(exit_of(&a2).await, ExitReason::Shutdown);
    assert!(logged(&log).contains(&"stopped".to_string()));
    assert!(!logged(&log).iter().any(|e| e.starts_with("exit:")));
    let events = write("parent_exit");
    has(
        &events,
        &[
            "a2.Trap:true",
            "a1.Stop:Shutdown",
            "a1.Die:Shutdown",
            "a1.Publish",
            "a2.Deliver:Shutdown",
            "a2.Dispatch",
            "a2.Hook:run",
            "a2.Die:Shutdown",
        ],
    );
}

/// A linked actor's panic ends a non-trapping peer with `Panic`, without
/// `stopped()`; the panicking request is answered at D with `Panic`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn link_notice_ends_a_non_trapping_peer() {
    let _serial = SERIAL.lock().await;
    lifecycle_trace::record_lifecycle();
    let log = Log::default();
    let a1 = Node::new(&log).start();
    let a2 = Node::new(&Log::default()).start();
    a1.request(Link(a2.child_handle())).await.unwrap();
    assert!(matches!(
        a2.request(Boom()).await,
        Err(ActorError::ActorStopped(ExitReason::Panic(_)))
    ));
    assert!(matches!(exit_of(&a1).await, ExitReason::Panic(_)));
    assert!(!logged(&log).contains(&"stopped".to_string()));
    let events = write("linked_panic");
    has(
        &events,
        &[
            "a1.Link:linked",
            "a2.Dispatch",
            "a2.Panic:turn",
            "a2.Hook:run",
            "a2.Die:Panic",
            "a2.Publish",
            "a1.Deliver:Panic",
            "a1.Hook:skip",
            "a1.Die:Panic",
        ],
    );
}

/// The runtime drops a running actor: it ends `Killed`, no `stopped()` (G1).
#[test]
fn runtime_drop_ends_killed() {
    let _serial = SERIAL.blocking_lock();
    lifecycle_trace::record_lifecycle();
    let log = Log::default();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let a1 = runtime.block_on(async {
        let a1 = Node::new(&log).start();
        a1.request(Sync()).await.unwrap();
        a1
    });
    drop(runtime);
    assert_eq!(a1.child_handle().wait_exit_blocking(), ExitReason::Killed);
    assert!(!logged(&log).contains(&"stopped".to_string()));
    let events = write("runtime_drop");
    has(&events, &["a1.Dispatch", "a1.Drop:Killed", "a1.Publish"]);
}

/// Threads mode: a trapped exit, then a kill skipping `stopped()`.
#[test]
fn threads_trapped_exit_then_kill() {
    use spawned_concurrency::threads;
    let _serial = SERIAL.blocking_lock();
    lifecycle_trace::record_lifecycle();
    struct Native(Log);
    impl threads::Actor for Native {
        fn started(&mut self, ctx: &threads::Context<Self>) {
            ctx.trap_exit(true);
        }
        fn stopped(&mut self, _: &threads::Context<Self>) {
            self.0.lock().unwrap().push("stopped".into());
        }
        fn exit_received(&mut self, exit: Exit, _: &threads::Context<Self>) {
            self.0
                .lock()
                .unwrap()
                .push(format!("exit:{:?}", exit.reason));
        }
    }
    struct Ping;
    impl Message for Ping {
        type Result = ();
    }
    impl threads::Handler<Ping> for Native {
        fn handle(&mut self, _: Ping, reply: PendingReply<'_, ()>, _: &threads::Context<Self>) {
            reply.reply(());
        }
    }
    use threads::ActorStart as _;
    let log = Log::default();
    let a1 = Native(log.clone()).start();
    a1.request(Ping).unwrap();
    a1.child_handle().exit(ExitReason::Shutdown);
    a1.request(Ping).unwrap();
    assert_eq!(logged(&log), ["exit:Shutdown"]);
    a1.child_handle().kill();
    assert_eq!(a1.child_handle().wait_exit_blocking(), ExitReason::Killed);
    assert!(!logged(&log).contains(&"stopped".to_string()));
    let events = write("threads_trapped_exit_kill");
    has(
        &events,
        &[
            "a1.Trap:true",
            "a1.Signal:Shutdown",
            "a1.Dispatch",
            "a1.Signal:Kill",
            "a1.Hook:skip",
            "a1.Die:Killed",
            "a1.Publish",
        ],
    );
}
