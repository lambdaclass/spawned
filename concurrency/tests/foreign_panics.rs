//! Panics in code the framework calls but does not own: the global tracing
//! subscriber, the process panic hook, and reply payload destructors run when
//! the requester has gone (gave up, timed out) or while the holder unwinds.
//! Extends `panic_safety.rs`, which covers panics in user callbacks.
//!
//! Each case installs process-global state, so it runs as an ignored child
//! test in its own process (`support/drop_subprocess.rs`). The parent checks
//! that the child did not abort, that every wait was bounded, that the exit
//! reasons are sensible and that supervisor and system shutdown completed.
//!
//! Metrics recorder: N/A. The `metrics` feature is pull-only (fixed atomic
//! counters read by `metrics::snapshot()`/`render_prometheus()`); it exposes no
//! recorder, exporter or callback hook, so no foreign code runs inside it.
//!
//! See docs/TESTING.md.
// Not simulated: subprocess children on OS threads and dedicated runtimes.
#![cfg(all(feature = "async", not(spawned_sim)))]

#[path = "support/drop_subprocess.rs"]
mod subprocess;

use spawned_concurrency::{message::Message, ExitReason};
use std::{
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Condvar, Mutex,
    },
    time::Duration,
};

const LIMIT: Duration = Duration::from_secs(10);
const DONE: &str = "foreign panic cases complete";

fn is_child() -> bool {
    std::env::var_os("SPAWNED_MAILBOX_DROP_CHILD").is_some()
}
/// Strict children also check the documented contract for one area:
/// `callbacks` (the original handler panic is the exit reason and `stopped()`
/// still runs) or `restart` (the supervisor still restarts the child).
fn strict(area: &str) -> bool {
    std::env::var("SPAWNED_FOREIGN_STRICT").is_ok_and(|s| s == area)
}

// ---------------------------------------------------------------------------
// Foreign code: a subscriber that panics, a panic hook that panics, a bomb
// ---------------------------------------------------------------------------

static ARMED: AtomicBool = AtomicBool::new(false);
static EVENTS: AtomicUsize = AtomicUsize::new(0);
const SUBSCRIBER_PANIC: &str = "injected subscriber panic";

/// Panics on every event while armed.
struct Exploding;
impl tracing::Subscriber for Exploding {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, _: &tracing::Event<'_>) {
        EVENTS.fetch_add(1, Ordering::SeqCst);
        if ARMED.load(Ordering::SeqCst) {
            panic!("{SUBSCRIBER_PANIC}");
        }
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}
fn install_exploding_subscriber() {
    tracing::subscriber::set_global_default(Exploding).expect("one subscriber per child");
    ARMED.store(true, Ordering::SeqCst);
}

#[derive(Debug)]
pub struct Bomb(Option<&'static str>);
impl Drop for Bomb {
    fn drop(&mut self) {
        if let Some(msg) = self.0.take() {
            panic!("{msg}");
        }
    }
}

#[derive(Default)]
struct Gate {
    open: Mutex<bool>,
    changed: Condvar,
}
impl Gate {
    fn release(&self) {
        *self.open.lock().unwrap_or_else(|p| p.into_inner()) = true;
        self.changed.notify_all();
    }
    fn wait(&self) {
        let open = self.open.lock().unwrap_or_else(|p| p.into_inner());
        let (_open, timeout) = self
            .changed
            .wait_timeout_while(open, LIMIT, |open| !*open)
            .unwrap_or_else(|p| p.into_inner());
        assert!(!timeout.timed_out(), "gate never released");
    }
}

// ---------------------------------------------------------------------------
// Messages and actors shared by both modes
// ---------------------------------------------------------------------------

pub struct Crash;
impl Message for Crash {
    type Result = ();
}
pub struct Ping;
impl Message for Ping {
    type Result = u64;
}
/// Replies with an armed bomb after the gate opens.
pub struct Gift;
impl Message for Gift {
    type Result = Bomb;
}
/// Keep an armed reply in actor state, then optionally panic.
pub struct Stash;
impl Message for Stash {
    type Result = ();
}

pub struct Victim {
    stopped: Arc<AtomicBool>,
    panic_in_started: bool,
    panic_in_stopped: bool,
    gate: Arc<Gate>,
    stash: Vec<Bomb>,
    generation: u64,
}
impl Victim {
    fn new(stopped: &Arc<AtomicBool>, panic_in_stopped: bool) -> Self {
        Self {
            stopped: stopped.clone(),
            panic_in_started: false,
            panic_in_stopped,
            gate: Arc::new(Gate::default()),
            stash: Vec::new(),
            generation: 0,
        }
    }
    fn on_started(&self) {
        if self.panic_in_started {
            panic!("injected started panic");
        }
    }
    fn on_stopped(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        if self.panic_in_stopped {
            panic!("injected stopped panic");
        }
    }
    fn on_crash(&self) {
        if self.generation <= 1 {
            panic!("injected handler panic");
        }
    }
}

const REPLY_PANIC: &str = "injected reply drop panic";
fn handler_panic() -> ExitReason {
    ExitReason::Panic("panic in handler: injected handler panic".into())
}

mod task_actors {
    use super::*;
    use spawned_concurrency::tasks::{Actor, Context, Handler};
    pub struct TaskVictim(pub Victim);
    impl Actor for TaskVictim {
        async fn started(&mut self, _: &Context<Self>) {
            self.0.on_started();
        }
        async fn stopped(&mut self, _: &Context<Self>) {
            self.0.on_stopped();
        }
    }
    impl Handler<Crash> for TaskVictim {
        async fn handle(
            &mut self,
            _: Crash,
            reply: spawned_concurrency::reply::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            self.0.on_crash();
            reply.reply(());
        }
    }
    impl Handler<Ping> for TaskVictim {
        async fn handle(
            &mut self,
            _: Ping,
            reply: spawned_concurrency::reply::PendingReply<'_, u64>,
            _: &Context<Self>,
        ) {
            reply.reply(7);
        }
    }
    impl Handler<Gift> for TaskVictim {
        async fn handle(
            &mut self,
            _: Gift,
            reply: spawned_concurrency::reply::PendingReply<'_, Bomb>,
            _: &Context<Self>,
        ) {
            let __reply_value: Bomb = {
                self.0.gate.wait();
                Bomb(Some(REPLY_PANIC))
            };
            reply.reply(__reply_value);
        }
    }
    impl Handler<Stash> for TaskVictim {
        async fn handle(
            &mut self,
            _: Stash,
            reply: spawned_concurrency::reply::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            self.0.stash.push(Bomb(Some(REPLY_PANIC)));
            reply.reply(());
        }
    }
}

mod thread_actors {
    use super::*;
    use spawned_concurrency::threads::{Actor, Context, Handler};
    pub struct ThreadVictim(pub Victim);
    impl Actor for ThreadVictim {
        fn started(&mut self, _: &Context<Self>) {
            self.0.on_started();
        }
        fn stopped(&mut self, _: &Context<Self>) {
            self.0.on_stopped();
        }
    }
    impl Handler<Crash> for ThreadVictim {
        fn handle(
            &mut self,
            _: Crash,
            reply: spawned_concurrency::reply::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            self.0.on_crash();
            reply.reply(());
        }
    }
    impl Handler<Ping> for ThreadVictim {
        fn handle(
            &mut self,
            _: Ping,
            reply: spawned_concurrency::reply::PendingReply<'_, u64>,
            _: &Context<Self>,
        ) {
            reply.reply(7);
        }
    }
    impl Handler<Gift> for ThreadVictim {
        fn handle(
            &mut self,
            _: Gift,
            reply: spawned_concurrency::reply::PendingReply<'_, Bomb>,
            _: &Context<Self>,
        ) {
            let __reply_value: Bomb = {
                self.0.gate.wait();
                Bomb(Some(REPLY_PANIC))
            };
            reply.reply(__reply_value);
        }
    }
    impl Handler<Stash> for ThreadVictim {
        fn handle(
            &mut self,
            _: Stash,
            reply: spawned_concurrency::reply::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            self.0.stash.push(Bomb(Some(REPLY_PANIC)));
            reply.reply(());
        }
    }
}

// ---------------------------------------------------------------------------
// Placement and bounded waits
// ---------------------------------------------------------------------------

mod env {
    use super::task_actors::TaskVictim;
    use super::*;
    use spawned_concurrency::{
        supervision::{Backoff, ChildSpec, SupervisorConfig},
        system::{ActorSystem, ExecutionGroup},
        tasks::{ActorRef, ActorStart, Backend},
        ChildHandle,
    };

    #[derive(Debug, Clone, Copy, PartialEq)]
    pub enum Place {
        Ambient,
        GroupAsync,
        GroupThread,
        Native,
    }
    pub const PLACES: [Place; 4] = [
        Place::Ambient,
        Place::GroupAsync,
        Place::GroupThread,
        Place::Native,
    ];

    pub struct Env {
        pub rt: tokio::runtime::Runtime,
        pub system: ActorSystem,
        group: Option<ExecutionGroup>,
        pub place: Place,
    }
    pub enum Started {
        Task(ActorRef<TaskVictim>),
        Native(spawned_concurrency::threads::ActorRef<super::thread_actors::ThreadVictim>),
    }
    impl Started {
        pub fn child(&self) -> ChildHandle {
            match self {
                Started::Task(a) => a.child_handle(),
                Started::Native(a) => a.child_handle(),
            }
        }
        pub fn crash(&self) {
            match self {
                Started::Task(a) => a.send(Crash).unwrap(),
                Started::Native(a) => a.send(Crash).unwrap(),
            }
        }
        pub fn stash(&self, env: &Env) {
            match self {
                Started::Task(a) => env.bounded(a.request(Stash)).unwrap(),
                Started::Native(a) => a.request_with_timeout(Stash, LIMIT).unwrap(),
            }
        }
        pub fn ping(&self, env: &Env) -> u64 {
            match self {
                Started::Task(a) => env.bounded(a.request(Ping)).unwrap(),
                Started::Native(a) => a.request_with_timeout(Ping, LIMIT).unwrap(),
            }
        }
        /// The requester times out before the actor replies with a bomb.
        pub fn timed_out_gift(&self, env: &Env, gate: &Gate) {
            let short = Duration::from_millis(20);
            let timed_out = match self {
                Started::Task(a) => env.bounded(a.request_with_timeout(Gift, short)).is_err(),
                Started::Native(a) => a.request_with_timeout(Gift, short).is_err(),
            };
            assert!(
                timed_out,
                "{:?}: gift arrived before the timeout",
                env.place
            );
            gate.release();
        }
        /// The requesting task is aborted while awaiting the reply.
        pub fn abandoned_gift(&self, env: &Env, gate: &Gate) {
            match self {
                Started::Task(a) => {
                    let a = a.clone();
                    let task = env.rt.spawn(async move { a.request(Gift).await });
                    std::thread::sleep(Duration::from_millis(20));
                    task.abort();
                    let _ = env.bounded(task);
                }
                Started::Native(a) => drop(a.request_raw(Gift).unwrap()),
            }
            gate.release();
        }
    }

    impl Env {
        pub fn new(place: Place) -> Self {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap();
            let system = ActorSystem::new().unwrap();
            let group = match place {
                Place::GroupAsync => Some(system.async_group("foreign", 2).unwrap()),
                Place::GroupThread => Some(system.thread_group("foreign", 8).unwrap()),
                _ => None,
            };
            Self {
                rt,
                system,
                group,
                place,
            }
        }
        pub fn start(&self, victim: Victim) -> Started {
            let _entered = self.rt.enter();
            match self.place {
                Place::Ambient => {
                    Started::Task(TaskVictim(victim).start_with_backend(Backend::Async))
                }
                Place::Native => Started::Native(spawned_concurrency::threads::ActorStart::start(
                    super::thread_actors::ThreadVictim(victim),
                )),
                _ => {
                    let actor = self
                        .group
                        .as_ref()
                        .unwrap()
                        .start(TaskVictim(victim))
                        .unwrap();
                    let _ = self.bounded(actor.ready());
                    Started::Task(actor)
                }
            }
        }
        pub fn bounded<F: std::future::Future>(&self, f: F) -> F::Output {
            self.rt
                .block_on(async move { tokio::time::timeout(LIMIT, f).await })
                .unwrap_or_else(|_| panic!("{:?}: wait timed out", self.place))
        }
        pub fn wait_exit(&self, child: &ChildHandle) -> ExitReason {
            self.bounded(child.wait_exit_async())
        }
        /// Supervisor restart of a child whose first generation crashes.
        /// Returns whether the replacement came up, after a bounded shutdown.
        pub fn supervised_restart(&self, stopped: &Arc<AtomicBool>) -> bool {
            let Some(group) = self.group.clone() else {
                return true;
            };
            let config = SupervisorConfig {
                max_restarts: 10,
                backoff: Backoff {
                    initial: Duration::ZERO,
                    maximum: Duration::ZERO,
                    jitter: false,
                    seed: Some(1),
                    ..Backoff::default()
                },
                ..SupervisorConfig::default()
            };
            let mut tree = self.system.supervisor(config).unwrap();
            let stopped = stopped.clone();
            let child = tree
                .add(ChildSpec::new("foreign", &group, move |ctx| {
                    let mut victim = Victim::new(&stopped, false);
                    victim.generation = ctx.generation();
                    async move { Ok::<_, String>(TaskVictim(victim)) }
                }))
                .unwrap();
            let supervisor = self.bounded(tree.start()).unwrap();
            child.current().unwrap().send(Crash).unwrap();
            let restarted = self
                .rt
                .block_on(async {
                    tokio::time::timeout(Duration::from_secs(2), child.wait_for_generation(1)).await
                })
                .ok()
                .and_then(Result::ok)
                .map(|next| matches!(self.bounded(next.request(Ping)), Ok(7)))
                .unwrap_or(false);
            let report = self.bounded(supervisor.shutdown(LIMIT));
            assert!(report.complete, "{:?}: supervisor shutdown", self.place);
            restarted
        }
        pub fn finish(self) {
            let report = self.bounded(self.system.shutdown(LIMIT));
            assert!(
                report.complete,
                "{:?}: system shutdown incomplete",
                self.place
            );
            self.rt.shutdown_timeout(LIMIT);
        }
    }
}
use env::{Env, PLACES};

// ---------------------------------------------------------------------------
// Child cases
// ---------------------------------------------------------------------------

/// Normal lifecycle, a handler panic, a handler panic followed by a stopped()
/// panic, and a supervised restart, all while the subscriber panics on every
/// event.
#[test]
#[ignore = "subprocess helper exercised by enabled parent"]
fn subscriber_child() {
    if !is_child() {
        return;
    }
    install_exploding_subscriber();
    let (mut callback_gaps, mut restart_gaps) = (Vec::new(), Vec::new());
    for place in PLACES {
        let env = Env::new(place);
        // Normal stop.
        let stopped = Arc::new(AtomicBool::new(false));
        let actor = env.start(Victim::new(&stopped, false));
        assert_eq!(actor.ping(&env), 7);
        actor.child().stop();
        assert_eq!(env.wait_exit(&actor.child()), ExitReason::Normal);
        assert!(
            stopped.load(Ordering::SeqCst),
            "{place:?}: stopped() skipped"
        );
        // Startup and shutdown-only panics must also retain their own reason
        // when the diagnostic subscriber panics. Failed startup retains the
        // existing contract that stopped() is not called.
        for startup_panic in [true, false] {
            let stopped = Arc::new(AtomicBool::new(false));
            let mut victim = Victim::new(&stopped, !startup_panic);
            victim.panic_in_started = startup_panic;
            let actor = env.start(victim);
            if !startup_panic {
                assert_eq!(actor.ping(&env), 7);
                actor.child().stop();
            }
            let reason = env.wait_exit(&actor.child());
            let expected = ExitReason::Panic(if startup_panic {
                "panic in started(): injected started panic".into()
            } else {
                "panic in stopped(): injected stopped panic".into()
            });
            if reason != expected || stopped.load(Ordering::SeqCst) == startup_panic {
                callback_gaps.push(format!(
                    "{place:?} startup_panic={startup_panic}: {reason:?}"
                ));
            }
        }
        // Handler panic, then (optionally) a stopped() panic during cleanup.
        for panic_in_stopped in [false, true] {
            let stopped = Arc::new(AtomicBool::new(false));
            let actor = env.start(Victim::new(&stopped, panic_in_stopped));
            actor.crash();
            let reason = env.wait_exit(&actor.child());
            assert!(
                matches!(&reason, ExitReason::Panic(_)),
                "{place:?}: {reason:?}"
            );
            let ran = stopped.load(Ordering::SeqCst);
            println!(
                "{place:?} panic_in_stopped={panic_in_stopped} reason={reason:?} stopped_ran={ran}"
            );
            // v0.8: a panic in stopped() replaces an earlier abnormal reason
            // (as a crash in OTP's terminate), unless a kill was received.
            let expected = if panic_in_stopped {
                ExitReason::Panic("panic in stopped(): injected stopped panic".into())
            } else {
                handler_panic()
            };
            if reason != expected || !ran {
                callback_gaps.push(format!(
                    "{place:?} stopped_panic={panic_in_stopped}: {reason:?}, stopped() ran: {ran}"
                ));
            }
        }
        let stopped = Arc::new(AtomicBool::new(false));
        if !env.supervised_restart(&stopped) {
            restart_gaps.push(format!("{place:?}: supervisor did not restart the child"));
        }
        env.finish();
    }
    assert!(
        EVENTS.load(Ordering::SeqCst) > 0,
        "no framework event reached the subscriber"
    );
    if strict("callbacks") {
        assert!(callback_gaps.is_empty(), "{callback_gaps:#?}");
    }
    if strict("restart") {
        assert!(restart_gaps.is_empty(), "{restart_gaps:#?}");
    }
    println!("{DONE}");
}

/// A panic hook that panics, during normal lifecycles (never invoked) and
/// then for one handler panic (Rust aborts: a panic inside the hook is a
/// "panic while processing panic", whoever owns the panicking code).
#[test]
#[ignore = "subprocess helper exercised by enabled parent"]
fn panic_hook_child() {
    if !is_child() {
        return;
    }
    std::panic::set_hook(Box::new(|_| panic!("injected panic hook panic")));
    for place in PLACES {
        let env = Env::new(place);
        let stopped = Arc::new(AtomicBool::new(false));
        let actor = env.start(Victim::new(&stopped, false));
        assert_eq!(actor.ping(&env), 7);
        actor.child().shutdown();
        assert_eq!(env.wait_exit(&actor.child()), ExitReason::Shutdown);
        env.finish();
    }
    println!("{DONE}");
    if std::env::var_os("SPAWNED_HOOK_UNWIND").is_some() {
        let env = Env::new(env::Place::GroupAsync);
        let stopped = Arc::new(AtomicBool::new(false));
        let actor = env.start(Victim::new(&stopped, false));
        actor.crash();
        let _ = env.wait_exit(&actor.child());
        println!("survived a panicking panic hook");
    }
}

/// Plain Rust, no framework: the same hook aborts the process on any panic.
#[test]
#[ignore = "subprocess helper exercised by enabled parent"]
fn panic_hook_std_child() {
    if !is_child() {
        return;
    }
    std::panic::set_hook(Box::new(|_| panic!("injected panic hook panic")));
    let _ = std::panic::catch_unwind(|| panic!("plain panic"));
    println!("survived a panicking panic hook");
}

/// Reply payloads whose Drop panics, dropped by the framework because the
/// requester timed out or gave up, or held in actor state when the actor
/// stops normally or unwinds from a handler panic.
#[test]
#[ignore = "subprocess helper exercised by enabled parent"]
fn reply_drop_child() {
    if !is_child() {
        return;
    }
    let reply_panic = ExitReason::Panic(format!("panic in handler: {REPLY_PANIC}"));
    for place in PLACES {
        let env = Env::new(place);
        // Gave up and timed out: the late reply is dropped in the replier,
        // inside its handler turn, as in panic_safety's ReplyDrop fault.
        for timed_out in [true, false] {
            let stopped = Arc::new(AtomicBool::new(false));
            let victim = Victim::new(&stopped, false);
            let gate = victim.gate.clone();
            let actor = env.start(victim);
            if timed_out {
                actor.timed_out_gift(&env, &gate);
            } else {
                actor.abandoned_gift(&env, &gate);
            }
            let reason = env.wait_exit(&actor.child());
            assert_eq!(reason, reply_panic, "{place:?} timed_out={timed_out}");
            assert!(
                stopped.load(Ordering::SeqCst),
                "{place:?}: stopped() skipped"
            );
        }
        // A held reply whose Drop panics while state is destroyed (after the
        // death point): a cleanup failure, never the reason.
        for unwinding in [false, true] {
            let stopped = Arc::new(AtomicBool::new(false));
            let mut victim = Victim::new(&stopped, false);
            victim.generation = 1;
            let actor = env.start(victim);
            actor.stash(&env);
            if unwinding {
                actor.crash();
            } else {
                actor.child().stop();
            }
            let reason = env.wait_exit(&actor.child());
            let expected = if unwinding {
                handler_panic()
            } else {
                ExitReason::Normal
            };
            assert_eq!(reason, expected, "{place:?} unwinding={unwinding}");
            assert_eq!(
                actor.child().cleanup_failure(),
                Some(format!("actor state destructor: {REPLY_PANIC}")),
                "{place:?} unwinding={unwinding}"
            );
        }
        // The process and the remaining actors stay healthy.
        let stopped = Arc::new(AtomicBool::new(false));
        let healthy = env.start(Victim::new(&stopped, false));
        assert_eq!(healthy.ping(&env), 7);
        healthy.child().stop();
        assert_eq!(env.wait_exit(&healthy.child()), ExitReason::Normal);
        env.finish();
    }
    println!("{DONE}");
}

// ---------------------------------------------------------------------------
// Parents
// ---------------------------------------------------------------------------

#[test]
fn panicking_subscriber_does_not_abort_hang_or_block_shutdown() {
    subprocess::run("subscriber_child", DONE);
}

#[test]
fn panicking_subscriber_keeps_handler_reason_and_runs_stopped() {
    subprocess::run_with(
        "subscriber_child",
        DONE,
        &[("SPAWNED_FOREIGN_STRICT", "callbacks".into())],
    );
}

#[test]
fn panicking_subscriber_does_not_prevent_restart() {
    subprocess::run_with(
        "subscriber_child",
        DONE,
        &[("SPAWNED_FOREIGN_STRICT", "restart".into())],
    );
}

#[test]
fn panicking_panic_hook_is_never_invoked_by_normal_lifecycles() {
    subprocess::run("panic_hook_child", DONE);
}

/// Documents Rust's rule rather than a framework property: once the hook
/// panics, std aborts, with or without spawned on the stack.
#[test]
fn panicking_panic_hook_aborts_like_plain_rust() {
    for (test, env) in [
        ("panic_hook_std_child", vec![]),
        (
            "panic_hook_child",
            vec![("SPAWNED_HOOK_UNWIND", "1".to_string())],
        ),
    ] {
        let outcome = subprocess::spawn(test, &env, Duration::from_secs(120))
            .unwrap_or_else(|| panic!("{test} hung instead of aborting"));
        assert!(!outcome.status.success(), "{test}: {}", outcome.output);
        assert!(
            !outcome.output.contains("survived"),
            "{test}: {}",
            outcome.output
        );
        assert!(
            outcome.errors.contains("panicked while processing panic")
                || outcome.errors.contains("aborting"),
            "{test} failed differently: {}",
            outcome.errors
        );
    }
}

#[test]
fn panicking_reply_drops_are_contained() {
    subprocess::run("reply_drop_child", DONE);
}
