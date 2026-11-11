//! Panic safety: a panic is injected into each user callback in turn (started,
//! send and request handlers, stopped, trapped exit_received, the Down handler,
//! a message whose Drop panics, a reply whose Drop panics after the caller left,
//! a queued message whose Drop panics at stop, and ChildSpec factories). For each,
//! in both modes and under supervision: the failing actor exits with the
//! documented reason, monitors and links fire exactly once, managed registry
//! entries are dropped, other actors keep working and shutdown completes.
//! See docs/TESTING.md.

// Tests that need OS threads are compiled out under spawned_sim.
#![cfg_attr(spawned_sim, allow(unused_imports, dead_code))]
#[path = "support/recorder.rs"]
mod recorder;

use recorder::Recorder;
use spawned_concurrency::{message::Message, ExitReason, MonitorRef};
use std::{
    sync::{Arc, Condvar, Mutex},
    time::Duration,
};

const LIMIT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fault {
    Started,
    SendHandler,
    RequestHandler,
    Stopped,
    ExitReceived,
    DownHandler,
    MessageDrop,
    ReplyDrop,
    QueuedMessageDrop,
}
const FAULTS: [Fault; 9] = [
    Fault::Started,
    Fault::SendHandler,
    Fault::RequestHandler,
    Fault::Stopped,
    Fault::ExitReceived,
    Fault::DownHandler,
    Fault::MessageDrop,
    Fault::ReplyDrop,
    Fault::QueuedMessageDrop,
];

/// The reason a victim must report, identical in both modes. A queued message
/// whose Drop panics while a stopping actor discards its mailbox is outside the
/// handler's `catch_unwind`, so the shutdown path publishes the trappable
/// framework panic reason.
fn expected(fault: Fault) -> ExitReason {
    let panic = |s: &str| ExitReason::Panic(s.to_string());
    match fault {
        Fault::Started => panic("panic in started(): injected started panic"),
        Fault::SendHandler | Fault::RequestHandler => {
            panic("panic in handler: injected handler panic")
        }
        Fault::Stopped => panic("panic in stopped(): injected stopped panic"),
        Fault::ExitReceived => panic("panic in handler: injected exit_received panic"),
        Fault::DownHandler => panic("panic in handler: injected Down panic"),
        Fault::MessageDrop => panic("panic in handler: injected message drop panic"),
        Fault::ReplyDrop => panic("panic in handler: injected reply drop panic"),
        // A destructor panic after the death point is a cleanup failure
        // (`ChildHandle::cleanup_failure`), never the exit reason.
        Fault::QueuedMessageDrop => ExitReason::Normal,
    }
}

/// Panics on drop when armed.
#[derive(Debug)]
struct Bomb(Option<&'static str>);
impl Drop for Bomb {
    fn drop(&mut self) {
        if let Some(msg) = self.0.take() {
            panic!("{msg}");
        }
    }
}

/// A one-shot latch that a handler can block on briefly.
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

#[derive(Debug, Clone, PartialEq)]
enum Event {
    Down(MonitorRef, ExitReason),
    Exit(spawned_concurrency::ActorId, ExitReason),
}
type Events = Arc<Recorder<Event>>;

struct Poke;
impl Message for Poke {
    type Result = ();
}
struct Ask;
impl Message for Ask {
    type Result = u64;
}
struct Payload(#[allow(dead_code)] Bomb);
impl Message for Payload {
    type Result = ();
}
struct Gift;
impl Message for Gift {
    type Result = Bomb;
}
struct Hold;
impl Message for Hold {
    type Result = ();
}
enum Setup {
    Trap,
    Link(spawned_concurrency::ChildHandle),
    Watch(spawned_concurrency::ChildHandle),
}
impl Message for Setup {
    type Result = Option<MonitorRef>;
}

/// Fault configuration shared by both modes.
struct Victim {
    fault: Option<Fault>,
    gate: Arc<Gate>,
    asks: u64,
    #[cfg_attr(not(feature = "async"), allow(dead_code))]
    name: Option<String>,
}
impl Victim {
    fn new(fault: Option<Fault>, gate: &Arc<Gate>) -> Self {
        Self {
            fault,
            gate: gate.clone(),
            asks: 0,
            name: None,
        }
    }
    fn fault(&self, fault: Fault, msg: &str) {
        if self.fault == Some(fault) {
            panic!("{msg}");
        }
    }
    fn ask(&mut self) -> u64 {
        self.fault(Fault::RequestHandler, "injected handler panic");
        self.asks += 1;
        self.asks
    }
    fn gift(&self) -> Bomb {
        self.gate.wait();
        Bomb((self.fault == Some(Fault::ReplyDrop)).then_some("injected reply drop panic"))
    }
}

/// Healthy observer: traps exits, records Down and Exit events.
struct Observer {
    events: Events,
}

#[cfg(feature = "async")]
mod tasks_actors {
    use super::*;
    use spawned_concurrency::{
        tasks::{Actor, Context, Handler},
        Down, Exit,
    };
    pub struct TaskVictim(pub Victim);
    impl Actor for TaskVictim {
        async fn started(&mut self, ctx: &Context<Self>) {
            if let Some(name) = &self.0.name {
                ctx.register_name(name).unwrap();
            }
            self.0.fault(Fault::Started, "injected started panic");
        }
        async fn stopped(&mut self, _: &Context<Self>) {
            self.0.fault(Fault::Stopped, "injected stopped panic");
        }
        async fn exit_received(&mut self, _: Exit, _: &Context<Self>) {
            self.0
                .fault(Fault::ExitReceived, "injected exit_received panic");
        }
    }
    impl Handler<Poke> for TaskVictim {
        async fn handle(
            &mut self,
            _: Poke,
            reply: spawned_concurrency::reply::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            self.0.fault(Fault::SendHandler, "injected handler panic");
            reply.reply(());
        }
    }
    impl Handler<Ask> for TaskVictim {
        async fn handle(
            &mut self,
            _: Ask,
            reply: spawned_concurrency::reply::PendingReply<'_, u64>,
            _: &Context<Self>,
        ) {
            reply.reply(self.0.ask());
        }
    }
    impl Handler<Payload> for TaskVictim {
        async fn handle(
            &mut self,
            _: Payload,
            reply: spawned_concurrency::reply::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            reply.reply(());
        }
    }
    impl Handler<Gift> for TaskVictim {
        async fn handle(
            &mut self,
            _: Gift,
            reply: spawned_concurrency::reply::PendingReply<'_, Bomb>,
            _: &Context<Self>,
        ) {
            reply.reply(self.0.gift());
        }
    }
    impl Handler<Hold> for TaskVictim {
        async fn handle(
            &mut self,
            _: Hold,
            reply: spawned_concurrency::reply::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            self.0.gate.wait();
            reply.reply(());
        }
    }
    impl Handler<Setup> for TaskVictim {
        async fn handle(
            &mut self,
            setup: Setup,
            reply: spawned_concurrency::reply::PendingReply<'_, Option<MonitorRef>>,
            ctx: &Context<Self>,
        ) {
            let __reply_value: Option<MonitorRef> = async {
                match setup {
                    Setup::Trap => ctx.trap_exit(true),
                    Setup::Link(target) => ctx.link(&target),
                    Setup::Watch(target) => return Some(ctx.monitor(&target)),
                }
                None
            }
            .await;
            reply.reply(__reply_value);
        }
    }
    impl Handler<Down> for TaskVictim {
        async fn handle(
            &mut self,
            _: Down,
            reply: spawned_concurrency::reply::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            self.0.fault(Fault::DownHandler, "injected Down panic");
            reply.reply(());
        }
    }

    pub struct TaskObserver(pub Observer);
    impl Actor for TaskObserver {
        async fn started(&mut self, ctx: &Context<Self>) {
            ctx.trap_exit(true);
        }
        async fn exit_received(&mut self, exit: Exit, _: &Context<Self>) {
            self.0.events.push(Event::Exit(exit.from, exit.reason));
        }
    }
    impl Handler<Down> for TaskObserver {
        async fn handle(
            &mut self,
            down: Down,
            reply: spawned_concurrency::reply::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            self.0
                .events
                .push(Event::Down(down.monitor_ref, down.reason));
            reply.reply(());
        }
    }
    impl Handler<Setup> for TaskObserver {
        async fn handle(
            &mut self,
            setup: Setup,
            reply: spawned_concurrency::reply::PendingReply<'_, Option<MonitorRef>>,
            ctx: &Context<Self>,
        ) {
            let __reply_value: Option<MonitorRef> = async {
                match setup {
                    Setup::Trap => ctx.trap_exit(true),
                    Setup::Link(target) => ctx.link(&target),
                    Setup::Watch(target) => return Some(ctx.monitor(&target)),
                }
                None
            }
            .await;
            reply.reply(__reply_value);
        }
    }
    impl Handler<Ask> for TaskObserver {
        async fn handle(
            &mut self,
            _: Ask,
            reply: spawned_concurrency::reply::PendingReply<'_, u64>,
            _: &Context<Self>,
        ) {
            reply.reply(7);
        }
    }
}

mod thread_actors {
    use super::*;
    use spawned_concurrency::{
        threads::{Actor, Context, Handler},
        Down, Exit,
    };
    pub struct ThreadVictim(pub Victim);
    impl Actor for ThreadVictim {
        fn started(&mut self, _: &Context<Self>) {
            self.0.fault(Fault::Started, "injected started panic");
        }
        fn stopped(&mut self, _: &Context<Self>) {
            self.0.fault(Fault::Stopped, "injected stopped panic");
        }
        fn exit_received(&mut self, _: Exit, _: &Context<Self>) {
            self.0
                .fault(Fault::ExitReceived, "injected exit_received panic");
        }
    }
    impl Handler<Poke> for ThreadVictim {
        fn handle(
            &mut self,
            _: Poke,
            reply: spawned_concurrency::reply::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            self.0.fault(Fault::SendHandler, "injected handler panic");
            reply.reply(());
        }
    }
    impl Handler<Ask> for ThreadVictim {
        fn handle(
            &mut self,
            _: Ask,
            reply: spawned_concurrency::reply::PendingReply<'_, u64>,
            _: &Context<Self>,
        ) {
            reply.reply(self.0.ask());
        }
    }
    impl Handler<Payload> for ThreadVictim {
        fn handle(
            &mut self,
            _: Payload,
            reply: spawned_concurrency::reply::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            reply.reply(());
        }
    }
    impl Handler<Gift> for ThreadVictim {
        fn handle(
            &mut self,
            _: Gift,
            reply: spawned_concurrency::reply::PendingReply<'_, Bomb>,
            _: &Context<Self>,
        ) {
            reply.reply(self.0.gift());
        }
    }
    impl Handler<Hold> for ThreadVictim {
        fn handle(
            &mut self,
            _: Hold,
            reply: spawned_concurrency::reply::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            self.0.gate.wait();
            reply.reply(());
        }
    }
    impl Handler<Setup> for ThreadVictim {
        fn handle(
            &mut self,
            setup: Setup,
            reply: spawned_concurrency::reply::PendingReply<'_, Option<MonitorRef>>,
            ctx: &Context<Self>,
        ) {
            let __reply_value: Option<MonitorRef> = (|| {
                match setup {
                    Setup::Trap => ctx.trap_exit(true),
                    Setup::Link(target) => ctx.link(&target),
                    Setup::Watch(target) => return Some(ctx.monitor(&target)),
                }
                None
            })();
            reply.reply(__reply_value);
        }
    }
    impl Handler<Down> for ThreadVictim {
        fn handle(
            &mut self,
            _: Down,
            reply: spawned_concurrency::reply::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            self.0.fault(Fault::DownHandler, "injected Down panic");
            reply.reply(());
        }
    }

    pub struct ThreadObserver(pub Observer);
    impl Actor for ThreadObserver {
        fn started(&mut self, ctx: &Context<Self>) {
            ctx.trap_exit(true);
        }
        fn exit_received(&mut self, exit: Exit, _: &Context<Self>) {
            self.0.events.push(Event::Exit(exit.from, exit.reason));
        }
    }
    impl Handler<Down> for ThreadObserver {
        fn handle(
            &mut self,
            down: Down,
            reply: spawned_concurrency::reply::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            self.0
                .events
                .push(Event::Down(down.monitor_ref, down.reason));
            reply.reply(());
        }
    }
    impl Handler<Setup> for ThreadObserver {
        fn handle(
            &mut self,
            setup: Setup,
            reply: spawned_concurrency::reply::PendingReply<'_, Option<MonitorRef>>,
            ctx: &Context<Self>,
        ) {
            let __reply_value: Option<MonitorRef> = (|| {
                match setup {
                    Setup::Trap => ctx.trap_exit(true),
                    Setup::Link(target) => ctx.link(&target),
                    Setup::Watch(target) => return Some(ctx.monitor(&target)),
                }
                None
            })();
            reply.reply(__reply_value);
        }
    }
    impl Handler<Ask> for ThreadObserver {
        fn handle(
            &mut self,
            _: Ask,
            reply: spawned_concurrency::reply::PendingReply<'_, u64>,
            _: &Context<Self>,
        ) {
            reply.reply(7);
        }
    }
}

/// Mode-independent view of the actors under test. Requests return
/// `Err(())` when the reply channel closes without a reply.
trait Harness {
    fn victim_child(&self) -> spawned_concurrency::ChildHandle;
    fn victim_send_poke(&self);
    fn victim_send_payload(&self, bomb: Bomb);
    fn victim_send_hold(&self);
    fn victim_ask(&self) -> Result<u64, ()>;
    fn victim_abandoned_gift(&self);
    fn victim_setup(&self, setup: Setup) -> Option<MonitorRef>;
    fn spawn_trigger(&self) -> spawned_concurrency::ChildHandle;
    fn observer_setup(&self, setup: Setup) -> Option<MonitorRef>;
    fn observer_ask(&self) -> u64;
    fn observer_child(&self) -> spawned_concurrency::ChildHandle;
    fn wait_exit(&self, child: &spawned_concurrency::ChildHandle) -> ExitReason;
}

/// Inject `fault` and check every post-condition. Returns the victim's reason.
fn exercise(h: &dyn Harness, fault: Fault, gate: &Gate, events: &Events) -> ExitReason {
    let victim = h.victim_child();
    let monitor = h
        .observer_setup(Setup::Watch(victim.clone()))
        .expect("monitor ref");
    h.observer_setup(Setup::Link(victim.clone()));
    match fault {
        Fault::Started => {}
        Fault::SendHandler => h.victim_send_poke(),
        Fault::RequestHandler => assert_eq!(h.victim_ask(), Err(()), "reply after panic"),
        Fault::Stopped => victim.stop(),
        Fault::ExitReceived => {
            let trigger = h.spawn_trigger();
            h.victim_setup(Setup::Trap);
            h.victim_setup(Setup::Link(trigger.clone()));
            trigger.shutdown();
        }
        Fault::DownHandler => {
            let trigger = h.spawn_trigger();
            h.victim_setup(Setup::Watch(trigger.clone()));
            trigger.stop();
        }
        Fault::MessageDrop => h.victim_send_payload(Bomb(Some("injected message drop panic"))),
        Fault::ReplyDrop => {
            // The caller leaves before the handler replies; the reply drops in the actor.
            h.victim_abandoned_gift();
            gate.release();
        }
        Fault::QueuedMessageDrop => {
            // Busy actor, queued armed message, stop: the message is discarded.
            h.victim_send_hold();
            h.victim_send_payload(Bomb(Some("injected queued drop panic")));
            victim.stop();
            gate.release();
        }
    }
    let reason = h.wait_exit(&victim);
    assert_eq!(reason, expected(fault), "{fault:?}");
    // Exactly one Down and one Exit, both carrying the published reason.
    let both = |e: &[Event]| e.len() >= 2;
    events.wait_until(
        LIMIT,
        &format!("{fault:?} Down and Exit, reason {reason:?}"),
        both,
    );
    assert_eq!(h.observer_ask(), 7, "{fault:?}: observer stopped working");
    let seen = events.snapshot();
    // A victim that panics in started() can pass its death point before the
    // observer's monitor or link is registered: that registration gets NoProc.
    let carries =
        |r: &ExitReason| *r == reason || (fault == Fault::Started && *r == ExitReason::NoProc);
    assert_eq!(
        seen.iter()
            .filter(|e| matches!(e, Event::Down(m, r) if *m == monitor && carries(r)))
            .count(),
        1,
        "{fault:?}: {seen:?}"
    );
    assert_eq!(
        seen.iter()
            .filter(|e| matches!(e, Event::Exit(id, r) if *id == victim.id() && carries(r)))
            .count(),
        1,
        "{fault:?}: {seen:?}"
    );
    if fault == Fault::QueuedMessageDrop {
        assert_eq!(
            victim.cleanup_failure().as_deref(),
            Some("injected queued drop panic"),
            "{fault:?}"
        );
    }
    assert_eq!(seen.len(), 2, "{fault:?}: extra notification {seen:?}");
    reason
}

// ---------------------------------------------------------------------------
// Threads mode
// ---------------------------------------------------------------------------

mod threads_mode {
    use super::thread_actors::*;
    use super::*;
    use spawned_concurrency::{
        threads::{ActorRef, ActorStart},
        ChildHandle,
    };

    struct ThreadsHarness {
        victim: ActorRef<ThreadVictim>,
        observer: ActorRef<ThreadObserver>,
        triggers: Mutex<Vec<ChildHandle>>,
    }
    impl Harness for ThreadsHarness {
        fn victim_child(&self) -> ChildHandle {
            self.victim.child_handle()
        }
        fn victim_send_poke(&self) {
            // Through a type-erased Recipient: the conversion adds no failure path.
            self.victim.recipient::<Poke>().send(Poke).unwrap();
        }
        fn victim_send_payload(&self, bomb: Bomb) {
            self.victim.send(Payload(bomb)).unwrap();
        }
        fn victim_send_hold(&self) {
            self.victim.send(Hold).unwrap();
        }
        fn victim_ask(&self) -> Result<u64, ()> {
            self.victim.request_with_timeout(Ask, LIMIT).map_err(|_| ())
        }
        fn victim_abandoned_gift(&self) {
            drop(
                self.victim
                    .send_request(Gift, spawned_concurrency::DEFAULT_REQUEST_TIMEOUT)
                    .unwrap(),
            );
        }
        fn victim_setup(&self, setup: Setup) -> Option<MonitorRef> {
            self.victim.request_with_timeout(setup, LIMIT).unwrap()
        }
        fn spawn_trigger(&self) -> ChildHandle {
            let gate = Arc::new(Gate::default());
            let trigger = ThreadVictim(Victim::new(None, &gate))
                .start()
                .child_handle();
            self.triggers.lock().unwrap().push(trigger.clone());
            trigger
        }
        fn observer_setup(&self, setup: Setup) -> Option<MonitorRef> {
            self.observer.request_with_timeout(setup, LIMIT).unwrap()
        }
        fn observer_ask(&self) -> u64 {
            self.observer.request_with_timeout(Ask, LIMIT).unwrap()
        }
        fn observer_child(&self) -> ChildHandle {
            self.observer.child_handle()
        }
        fn wait_exit(&self, child: &ChildHandle) -> ExitReason {
            wait_native(child)
        }
    }
    fn wait_native(child: &ChildHandle) -> ExitReason {
        let (tx, rx) = std::sync::mpsc::channel();
        let waiter = child.clone();
        std::thread::spawn(move || {
            let _ = tx.send(waiter.wait_exit_blocking());
        });
        rx.recv_timeout(LIMIT).expect("actor did not exit")
    }

    #[test]
    fn every_callback_panic_is_contained_in_threads_mode() {
        for fault in FAULTS {
            let gate = Arc::new(Gate::default());
            let events: Events = Recorder::new();
            let observer = ThreadObserver(Observer {
                events: events.clone(),
            })
            .start();
            let victim = ThreadVictim(Victim::new(Some(fault), &gate)).start();
            let h = ThreadsHarness {
                victim,
                observer,
                triggers: Mutex::new(Vec::new()),
            };
            exercise(&h, fault, &gate, &events);
            for trigger in h.triggers.lock().unwrap().iter() {
                trigger.stop();
                wait_native(trigger);
            }
            h.observer_child().stop();
            assert_eq!(wait_native(&h.observer_child()), ExitReason::Normal);
        }
    }
}

// ---------------------------------------------------------------------------
// Tasks mode, unmanaged backends and execution groups
// ---------------------------------------------------------------------------

#[cfg(feature = "async")]
mod tasks_mode {
    use super::tasks_actors::*;
    use super::*;
    use spawned_concurrency::{
        registry,
        supervision::{Backoff, ChildSpec, SupervisorConfig},
        system::{ActorSystem, ExecutionGroup},
        tasks::{ActorRef, ActorStart, Backend},
        ChildHandle,
    };

    #[derive(Debug, Clone, Copy, PartialEq)]
    enum Place {
        Async,
        Blocking,
        Thread,
        GroupAsync,
        GroupThread,
    }
    const PLACES: [Place; 5] = [
        Place::Async,
        Place::Blocking,
        Place::Thread,
        Place::GroupAsync,
        Place::GroupThread,
    ];

    struct Env {
        rt: tokio::runtime::Runtime,
        system: Option<ActorSystem>,
        group: Option<ExecutionGroup>,
        place: Place,
    }
    impl Env {
        fn new(place: Place) -> Self {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap();
            let (system, group) = match place {
                Place::GroupAsync | Place::GroupThread => {
                    let system = ActorSystem::new().unwrap();
                    let group = if place == Place::GroupAsync {
                        system.async_group("panic", 2)
                    } else {
                        system.thread_group("panic", 16)
                    }
                    .unwrap();
                    (Some(system), Some(group))
                }
                _ => (None, None),
            };
            Self {
                rt,
                system,
                group,
                place,
            }
        }
        fn start<A: spawned_concurrency::tasks::Actor>(&self, actor: A) -> ActorRef<A> {
            let _entered = self.rt.enter();
            match self.place {
                Place::Async => actor.start_with_backend(Backend::Async),
                Place::Blocking => actor.start_with_backend(Backend::Blocking),
                Place::Thread => actor.start_with_backend(Backend::Thread),
                _ => self.group.as_ref().unwrap().start(actor).unwrap(),
            }
        }
        fn bounded<F: std::future::Future>(&self, f: F) -> F::Output {
            self.rt
                .block_on(async move { tokio::time::timeout(LIMIT, f).await })
                .unwrap_or_else(|_| panic!("{:?}: wait timed out", self.place))
        }
        fn finish(self) {
            if let Some(system) = &self.system {
                let report = self.bounded(system.shutdown(LIMIT));
                assert!(report.complete, "{:?}: shutdown incomplete", self.place);
            }
            self.rt.shutdown_timeout(LIMIT);
        }
    }

    struct TasksHarness<'a> {
        env: &'a Env,
        victim: ActorRef<TaskVictim>,
        observer: ActorRef<TaskObserver>,
        triggers: Mutex<Vec<ChildHandle>>,
    }
    impl Harness for TasksHarness<'_> {
        fn victim_child(&self) -> ChildHandle {
            self.victim.child_handle()
        }
        fn victim_send_poke(&self) {
            self.victim.recipient::<Poke>().send(Poke).unwrap();
        }
        fn victim_send_payload(&self, bomb: Bomb) {
            self.victim.send(Payload(bomb)).unwrap();
        }
        fn victim_send_hold(&self) {
            self.victim.send(Hold).unwrap();
        }
        fn victim_ask(&self) -> Result<u64, ()> {
            self.env
                .bounded(self.victim.request_with_timeout(Ask, LIMIT))
                .map_err(|_| ())
        }
        fn victim_abandoned_gift(&self) {
            drop(
                self.victim
                    .send_request(Gift, spawned_concurrency::DEFAULT_REQUEST_TIMEOUT)
                    .unwrap(),
            );
        }
        fn victim_setup(&self, setup: Setup) -> Option<MonitorRef> {
            self.env
                .bounded(self.victim.request_with_timeout(setup, LIMIT))
                .unwrap()
        }
        fn spawn_trigger(&self) -> ChildHandle {
            let gate = Arc::new(Gate::default());
            let trigger = self
                .env
                .start(TaskVictim(Victim::new(None, &gate)))
                .child_handle();
            self.triggers.lock().unwrap().push(trigger.clone());
            trigger
        }
        fn observer_setup(&self, setup: Setup) -> Option<MonitorRef> {
            self.env
                .bounded(self.observer.request_with_timeout(setup, LIMIT))
                .unwrap()
        }
        fn observer_ask(&self) -> u64 {
            self.env
                .bounded(self.observer.request_with_timeout(Ask, LIMIT))
                .unwrap()
        }
        fn observer_child(&self) -> ChildHandle {
            self.observer.child_handle()
        }
        fn wait_exit(&self, child: &ChildHandle) -> ExitReason {
            self.env.bounded(child.wait_exit_async())
        }
    }

    // Not simulated: builds its own Tokio runtime and mixes backends.
    #[cfg(not(spawned_sim))]
    #[test]
    fn every_callback_panic_is_contained_in_tasks_mode() {
        for place in PLACES {
            for fault in FAULTS {
                let env = Env::new(place);
                let gate = Arc::new(Gate::default());
                let events: Events = Recorder::new();
                let observer = env.start(TaskObserver(Observer {
                    events: events.clone(),
                }));
                let mut victim = Victim::new(Some(fault), &gate);
                let name = format!("panic-{place:?}-{fault:?}");
                let managed = env.group.is_some();
                if managed {
                    victim.name = Some(name.clone());
                }
                let victim = env.start(TaskVictim(victim));
                if managed {
                    // A managed actor stopped before its first poll skips started()
                    // and stopped() entirely (see cross_backend), so wait for startup.
                    let _ = env.bounded(victim.ready());
                }
                let h = TasksHarness {
                    env: &env,
                    victim,
                    observer,
                    triggers: Mutex::new(Vec::new()),
                };
                exercise(&h, fault, &gate, &events);
                if managed {
                    // The scope drops its registrations once the actor finishes.
                    let deadline = std::time::Instant::now() + LIMIT;
                    while registry::registered().contains(&name) {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "{place:?} {fault:?}: leaked registration"
                        );
                        std::thread::yield_now();
                    }
                }
                for trigger in h.triggers.lock().unwrap().iter() {
                    trigger.stop();
                    h.wait_exit(trigger);
                }
                h.observer_child().stop();
                assert_eq!(h.wait_exit(&h.observer_child()), ExitReason::Normal);
                drop(h);
                env.finish();
            }
        }
    }

    fn config() -> SupervisorConfig {
        SupervisorConfig {
            max_restarts: 10,
            backoff: Backoff {
                initial: Duration::ZERO,
                maximum: Duration::ZERO,
                jitter: false,
                seed: Some(1),
                ..Backoff::default()
            },
            ..SupervisorConfig::default()
        }
    }

    /// Under supervision the first generation fails in each callback; the
    /// Permanent child is rebuilt once and the tree stays healthy.
    // Not simulated: builds its own Tokio runtime and mixes backends.
    #[cfg(not(spawned_sim))]
    #[test]
    fn supervised_callback_panics_restart_once() {
        for place in [Place::GroupAsync, Place::GroupThread] {
            for fault in FAULTS {
                let env = Env::new(place);
                let group = env.group.clone().unwrap();
                let gate = Arc::new(Gate::default());
                let events: Events = Recorder::new();
                let observer = env.start(TaskObserver(Observer {
                    events: events.clone(),
                }));
                let mut tree = env.system.as_ref().unwrap().supervisor(config()).unwrap();
                let child_gate = gate.clone();
                let child = tree
                    .add(ChildSpec::new("victim", &group, move |ctx| {
                        // v0.8 (S1): a first start is not retried, so a
                        // started() panic is injected into the replacement.
                        let faulty = if fault == Fault::Started { 2 } else { 1 };
                        let fault = (ctx.generation() == faulty).then_some(fault);
                        let victim = TaskVictim(Victim::new(fault, &child_gate));
                        async move { Ok::<_, String>(victim) }
                    }))
                    .unwrap();
                let supervisor = env.bounded(tree.start()).unwrap();
                if fault == Fault::Started {
                    // The replacement never became ready; its retry did.
                    child.current().unwrap().child_handle().kill();
                    let next = env.bounded(child.wait_for_generation(2)).unwrap();
                    assert_eq!(env.bounded(next.request(Ask)).unwrap(), 1);
                    assert_eq!(child.snapshot().generation, 3);
                    assert_eq!(child.snapshot().last_exit, Some(expected(fault)));
                } else {
                    let first = child.current().unwrap();
                    let h = TasksHarness {
                        env: &env,
                        victim: first,
                        observer: observer.clone(),
                        triggers: Mutex::new(Vec::new()),
                    };
                    let reason = exercise(&h, fault, &gate, &events);
                    let next = env.bounded(child.wait_for_generation(1)).unwrap();
                    assert_eq!(env.bounded(next.request(Ask)).unwrap(), 1, "{fault:?}");
                    let snapshot = child.snapshot();
                    assert_eq!(snapshot.generation, 2, "{place:?} {fault:?}");
                    assert_eq!(snapshot.last_exit, Some(reason), "{place:?} {fault:?}");
                    for trigger in h.triggers.lock().unwrap().iter() {
                        trigger.stop();
                        h.wait_exit(trigger);
                    }
                }
                let report = env.bounded(supervisor.shutdown(LIMIT));
                assert!(report.complete, "{place:?} {fault:?}");
                assert_eq!(report.failure, None, "{place:?} {fault:?}");
                observer.child_handle().stop();
                env.bounded(observer.wait_exit());
                env.finish();
            }
        }
    }

    /// A ChildSpec factory panic, before or inside its future, is a failed
    /// construction attempt: retried, never fatal to the tree.
    // Not simulated: builds its own Tokio runtime and mixes backends.
    #[cfg(not(spawned_sim))]
    #[test]
    fn factory_panics_are_retried() {
        for place in [Place::GroupAsync, Place::GroupThread] {
            for in_future in [false, true] {
                let env = Env::new(place);
                let group = env.group.clone().unwrap();
                let gate = Arc::new(Gate::default());
                let mut tree = env.system.as_ref().unwrap().supervisor(config()).unwrap();
                let child = tree
                    .add(ChildSpec::new("factory", &group, move |ctx| {
                        // v0.8 (S1): a first start is not retried, so the
                        // panic is injected into the replacement.
                        let first = ctx.generation() == 2;
                        if first && !in_future {
                            panic!("injected factory panic");
                        }
                        let victim = TaskVictim(Victim::new(None, &gate));
                        async move {
                            if first {
                                panic!("injected factory future panic");
                            }
                            Ok::<_, String>(victim)
                        }
                    }))
                    .unwrap();
                let supervisor = env.bounded(tree.start()).unwrap();
                child.current().unwrap().child_handle().kill();
                let actor = env.bounded(child.wait_for_generation(2)).unwrap();
                assert_eq!(env.bounded(actor.request(Ask)).unwrap(), 1);
                let snapshot = child.snapshot();
                assert_eq!(snapshot.generation, 3, "{place:?} in_future={in_future}");
                assert!(snapshot
                    .last_error
                    .as_deref()
                    .is_some_and(|e| e.contains("injected factory")));
                let report = env.bounded(supervisor.shutdown(LIMIT));
                assert!(report.complete);
                assert_eq!(report.failure, None);
                env.finish();
            }
        }
    }
}
