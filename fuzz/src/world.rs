//! The worker actor every scenario runs, and the per-run log that its hooks,
//! handlers, destructor and the supervisor factories write. Checks that can
//! be decided at the moment an event happens are made here; violations are
//! recorded (never panicked inside an actor, which the framework would turn
//! into an ordinary actor panic) and reported by the driver at the end.

use spawned_concurrency::{
    message::Message,
    supervision::{Backoff, RestartMode, SupervisedChild, SupervisorStrategy},
    tasks::{Actor, Context, Handler, Recipient},
    ActorId, ChildHandle, Down, Exit, ExitReason, MonitorRef,
};
use spawned_rt::tasks::{self as rt, sim};
use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    sync::{
        atomic::{AtomicU16, AtomicU64, Ordering},
        Arc, Mutex, MutexGuard,
    },
    time::Duration,
};

use crate::fault::{inject_panic, Answer, BombRec};
use crate::scenario::{site, Hooks};

/// One oracle violation. `kind` is the stable panic message (no ids or
/// counters, so the tape shrinker can match it across runs); `detail` is
/// printed alongside.
#[derive(Debug, Clone)]
pub struct Violation {
    pub kind: &'static str,
    pub detail: String,
}

/// Per worker instance (one constructed `Worker` value).
#[derive(Debug, Default)]
pub struct Inst {
    pub id: Option<ActorId>,
    pub slot: Option<String>,
    pub started_begin: Option<u64>,
    pub started_end: Option<u64>,
    pub stopped_begin: Option<u64>,
    pub stopped_end: Option<u64>,
    pub dropped: Option<u64>,
    /// Some hook or handler of this instance panicked (injected).
    pub panicked: bool,
    pub started_panicked: bool,
    /// The state destructor panicked (injected): a cleanup failure, which
    /// preserves the published reason but can restart a Transient child.
    pub drop_panicked: bool,
    /// Handed to the framework: only then may its destructor panic (a value
    /// the harness drops itself is not the framework's to contain).
    pub armed: bool,
    pub last_lane0: u64,
    pub downs: Vec<(MonitorRef, ExitReason)>,
    pub exits: Vec<(ActorId, ExitReason)>,
    /// Sources whose `Down`/`Exit` this instance already handled.
    pub ended: HashSet<ActorId>,
    /// Virtual times of the same hooks, for the restart and backoff oracles
    /// (`ready_at` and `ended_at` of the supervisor's stability check are
    /// read in the same polls as `started_end` and `stopped_begin`).
    pub started_end_at: Option<Duration>,
    pub stopped_begin_at: Option<Duration>,
    pub dropped_at: Option<Duration>,
    /// Factory call that constructed this instance (supervised only).
    pub call: Option<usize>,
    /// Returned by `SupervisedChild::current()`: the supervisor published it,
    /// so it was ready (V4).
    pub published: bool,
    /// Taken in `started()`; the published reason is read after teardown.
    pub handle: Option<ChildHandle>,
    pub reason: Option<ExitReason>,
    /// A reply payload went off inside the actor's own `reply()` call (in a
    /// turn or `stopped()`): a panic of its own, so its reason is not
    /// predicted. (One that goes off on its task anywhere else is past D: a
    /// cleanup failure, L6.)
    pub relaxed: bool,
    /// The actor is inside its own `reply()` call (see `relaxed`).
    pub replying: bool,
    /// `ChildHandle::cleanup_failure()` after teardown.
    pub cleanup_failure: Option<String>,
}

#[derive(Debug, Clone)]
pub struct MonRec {
    pub watcher: ActorId,
    pub target: ChildHandle,
    pub mref: MonitorRef,
    pub demonitored: bool,
    /// Created by the driver (outside), so its time is known.
    pub outside: bool,
}

/// A supervisor instance: a root, or one generation of a nested subtree.
#[derive(Debug, Clone)]
pub struct SupInst {
    pub root: usize,
    pub max_restarts: usize,
    pub window: Duration,
    /// OneForAll or RestForOne: a recovery round rebuilds several children
    /// on one restart grant (0.8, V5).
    pub cohort: bool,
    pub strategy: SupervisorStrategy,
    pub backoff: Backoff,
    /// A root whose declared children are all workers (the restart oracles
    /// only judge these; nested subtrees are checked by V1, V3 and V5).
    pub flat: bool,
    /// Children declared (slot paths `r{root}/c{i}`).
    pub children: usize,
    /// Virtual time of the first stop, shutdown or system close the driver
    /// requested before teardown.
    pub closed_at: Option<Duration>,
    /// Teardown found the tree settled (no child starting, backing off or
    /// stopping) at this tick, before shutting it down.
    pub settled: Option<u64>,
    /// `Supervisor::failure()` when teardown began.
    pub failure: Option<String>,
}

/// How a factory call ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The call has not returned (or its future was dropped).
    Pending,
    Error,
    Panic,
    Instance(usize),
}

/// One factory invocation (worker factory or nested `configure` callback).
#[derive(Debug, Clone)]
pub struct Call {
    pub inst: usize,
    pub slot: String,
    /// Declaration index of the slot in its supervisor.
    pub index: usize,
    pub at: Duration,
    pub seq: u64,
    pub mode: RestartMode,
    pub outcome: Outcome,
}

/// A `send_after` or `send_interval` the driver created (bug 21's oracle).
#[derive(Debug, Clone)]
pub struct TimerRec {
    pub created: Duration,
    pub period: Duration,
    pub interval: bool,
    pub cancelled: Option<Duration>,
    /// Timer messages a handler ran.
    pub handled: u64,
    /// The actor whose `Context` created it (T2: its ticks end at its stop).
    pub target: ChildHandle,
    /// Copies of the message the timer task made: `send_interval` clones it
    /// at the top of every round, so each after the first follows a send.
    pub clones: u64,
}

#[derive(Default)]
pub struct State {
    pub seq: u64,
    pub insts: Vec<Inst>,
    pub by_id: HashMap<ActorId, usize>,
    /// Request nonce -> instance that handled it.
    pub handled: HashMap<u64, usize>,
    pub monitors: Vec<MonRec>,
    /// Actors whose trap flag or links changed inside a handler: their
    /// propagation is not predicted.
    pub uncertain: HashSet<ActorId>,
    /// Actors something may have stopped (direct stop, injected panic,
    /// link, system shutdown).
    pub may_stop: HashSet<ActorId>,
    /// How many separate causes could have stopped each actor (every
    /// `stoppable` call): X8 predicts the reason only when there was one.
    pub causes: HashMap<ActorId, u32>,
    pub live: BTreeMap<String, i64>,
    pub live_total: i64,
    pub sups: Vec<SupInst>,
    pub calls: Vec<Call>,
    /// Root supervisors reported done (complete shutdown).
    pub roots_done: Vec<bool>,
    /// Latest `SupervisedChild` of each nested worker slot.
    pub nested: BTreeMap<String, SupervisedChild<Worker>>,
    pub timers: Vec<TimerRec>,
    pub violations: Vec<Violation>,
    /// Every injected-fault payload (M6, L6).
    pub bombs: Vec<BombRec>,
    /// `Fx` requests whose reply was deferred: `ReplyDropped` is a valid answer.
    pub deferred: HashSet<u64>,
    /// Simulated task -> worker instance.
    pub tasks: HashMap<sim::Id, usize>,
}

/// Injection-site statistics of one run (`examples/sites.rs`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Sites {
    /// Sites armed at some point of the run (`scenario::site` bits).
    pub armed: u16,
    /// Per site bit: payloads created armed (payload sites only).
    pub payloads_armed: [u64; 16],
    /// Per site bit: faults that went off.
    pub fired: [u64; 16],
    /// Subscriber panics (`TRACING`).
    pub subscriber_panics: u64,
}

pub struct World {
    state: Mutex<State>,
    /// Sites armed now (`scenario::site`).
    armed: AtomicU16,
    /// Sites armed at some point of the run (stats).
    ever_armed: AtomicU16,
    /// The driver's task and its request tasks: the caller's context, where
    /// no injected fault goes off.
    callers: Mutex<HashSet<sim::Id>>,
    /// Tasks whose handler is inside its own call into the framework
    /// (`ctx.pipe*`, `ctx.spawn_helper`), with the nesting depth: a payload
    /// the call refuses is dropped there, in the caller's context.
    calling: Mutex<HashMap<sim::Id, u32>>,
    /// Callback faults that went off, per site bit (stats). Atomics, so the
    /// subscriber never takes the state lock (a `tracing` event emitted
    /// while it is held would deadlock on it).
    site_fires: [AtomicU64; 16],
    subscriber_panics: AtomicU64,
}

impl World {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State::default()),
            armed: AtomicU16::new(0),
            ever_armed: AtomicU16::new(0),
            callers: Mutex::new(HashSet::new()),
            calling: Mutex::new(HashMap::new()),
            site_fires: Default::default(),
            subscriber_panics: AtomicU64::new(0),
        })
    }
    pub fn armed(&self) -> u16 {
        self.armed.load(Ordering::Relaxed)
    }
    pub fn arm(&self, sites: u16) {
        self.armed.store(sites, Ordering::Relaxed);
        self.ever_armed.fetch_or(sites, Ordering::Relaxed);
    }
    /// Count a fault that went off (stats).
    pub fn count(&self, site: u16) {
        let bit = site.trailing_zeros() as usize;
        if bit < 16 {
            self.site_fires[bit].fetch_add(1, Ordering::Relaxed);
        }
        if site == crate::scenario::site::TRACING {
            self.subscriber_panics.fetch_add(1, Ordering::Relaxed);
        }
    }
    /// The run's injection-site statistics.
    pub fn sites(&self) -> Sites {
        let mut sites = Sites {
            armed: self.ever_armed.load(Ordering::Relaxed),
            subscriber_panics: self.subscriber_panics.load(Ordering::Relaxed),
            ..Sites::default()
        };
        for (bit, n) in self.site_fires.iter().enumerate() {
            sites.fired[bit] = n.load(Ordering::Relaxed);
        }
        for b in &self.lock().bombs {
            let bit = b.site.trailing_zeros() as usize;
            if bit < 16 {
                sites.payloads_armed[bit] += b.armed as u64;
                sites.fired[bit] += b.fired.is_some() as u64;
            }
        }
        sites
    }
    /// Mark `task` as the caller's context (the driver, a request task).
    pub fn add_caller(&self, task: Option<sim::Id>) {
        if let Some(task) = task {
            self.callers
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(task);
        }
    }
    /// Run `f`, a handler's call into the framework, as caller context of
    /// the current task (reentrant: a nested call keeps the outer one's).
    pub fn calling<T>(&self, f: impl FnOnce() -> T) -> T {
        struct Reset<'a>(&'a World, Option<sim::Id>);
        impl Drop for Reset<'_> {
            fn drop(&mut self) {
                let Some(task) = self.1 else { return };
                let mut calling = self.0.calling.lock().unwrap_or_else(|p| p.into_inner());
                if let Some(depth) = calling.get_mut(&task) {
                    *depth -= 1;
                    if *depth == 0 {
                        calling.remove(&task);
                    }
                }
            }
        }
        let task = crate::fault::task();
        if let Some(task) = task {
            *self
                .calling
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .entry(task)
                .or_default() += 1;
        }
        let _reset = Reset(self, task);
        f()
    }
    /// No injected fault goes off here: the caller's context, or outside
    /// the simulation (no task the harness models).
    pub fn in_caller(&self) -> bool {
        let Some(task) = crate::fault::task() else {
            return true;
        };
        self.calling
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains_key(&task)
            || self
                .callers
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .contains(&task)
    }
    /// The worker instance whose task runs now.
    pub fn task_owner(&self) -> Option<usize> {
        let task = crate::fault::task()?;
        self.lock().tasks.get(&task).copied()
    }
    pub fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }
    pub fn violate(&self, kind: &'static str, detail: String) {
        self.lock().violate(kind, detail);
    }
}

impl State {
    pub fn violate(&mut self, kind: &'static str, detail: String) {
        self.violations.push(Violation { kind, detail });
    }
    /// Record one more cause that may stop `id`.
    pub fn stoppable(&mut self, id: ActorId) {
        self.may_stop.insert(id);
        *self.causes.entry(id).or_default() += 1;
    }
    pub fn tick(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }
    pub fn instance_of(&self, id: ActorId) -> Option<usize> {
        self.by_id.get(&id).copied()
    }
    pub fn name(&self, id: ActorId) -> String {
        match self.instance_of(id) {
            Some(i) => format!("w{i}"),
            None => "w?".into(),
        }
    }
}

/// What a `Do` message makes the worker do.
#[derive(Clone)]
pub enum Act {
    Noop,
    Panic,
    Sleep(Duration),
    StopSelf(ExitReason),
    Note(Recipient<Note>),
    Link(ChildHandle),
    Unlink(ChildHandle),
    Monitor(ChildHandle),
    Trap(bool),
    Register(String),
    Stop(ChildHandle, ExitReason),
    Helper(u8),
}

/// A command with its request nonce (0: untracked, as for timer sends) and
/// sender lane (0: the driver task, whose sends are sequential).
#[derive(Clone)]
pub struct Do {
    pub nonce: u64,
    pub lane: u64,
    /// Set for timer messages.
    pub timer: Option<TimerTag>,
    pub act: Act,
}

/// Marks a timer message with its index in [`State::timers`]. Cloning it is
/// a tick round of a `send_interval` task, checked against T2 there.
pub struct TimerTag {
    pub world: Arc<World>,
    pub index: usize,
}

impl Clone for TimerTag {
    /// T2: an interval stops when its actor stops. The timer task clones the
    /// message at the top of every round; every clone after the first comes
    /// in the same poll as a send that found the actor's token not cancelled
    /// (the cancel arm is polled first), and that token is cancelled before
    /// the exit is published. So no second clone follows the publication.
    fn clone(&self) -> Self {
        let mut s = self.world.lock();
        let t = &mut s.timers[self.index];
        t.clones += 1;
        if t.clones > 1 && t.target.exit_reason().is_some() {
            let detail = format!(
                "timer {} (every {:?}): round {} after its actor exited",
                self.index, t.period, t.clones
            );
            s.violate(
                "T2: an interval timer ticked after its actor's exit was published",
                detail,
            );
        }
        drop(s);
        Self {
            world: self.world.clone(),
            index: self.index,
        }
    }
}
impl Message for Do {
    type Result = u64;
}

/// Sent by one worker to another; must never follow the sender's `Down` or
/// `Exit` at the receiver (X7).
#[derive(Clone)]
pub struct Note {
    pub from: ActorId,
}
impl Message for Note {
    type Result = ();
}

pub struct Worker {
    pub(crate) world: Arc<World>,
    pub(crate) instance: usize,
    hooks: Hooks,
    slot: Option<String>,
    /// Sites armed when it was constructed (handler panics: `DOWN_PANIC`,
    /// `EXIT_PANIC`, `NOTE_PANIC`, `PAYLOAD_DROP`).
    pub(crate) faults: u16,
    /// Deferred `Fx` replies, oldest first.
    pub(crate) deferred: VecDeque<(u64, spawned_concurrency::ReplyTo<Answer>)>,
}

impl Worker {
    /// Construct and count one live instance (per supervisor slot too).
    pub fn new(world: &Arc<World>, slot: Option<String>, hooks: Hooks) -> Self {
        let instance = {
            let mut s = world.lock();
            s.insts.push(Inst {
                slot: slot.clone(),
                ..Inst::default()
            });
            s.live_total += 1;
            if let Some(slot) = &slot {
                *s.live.entry(slot.clone()).or_default() += 1;
            }
            s.insts.len() - 1
        };
        Self {
            world: world.clone(),
            instance,
            hooks,
            slot,
            faults: world.armed(),
            deferred: VecDeque::new(),
        }
    }
    pub fn instance(&self) -> usize {
        self.instance
    }
    /// Mark the instance as owned by the framework (see [`Inst::armed`]).
    pub fn arm(world: &World, instance: usize) {
        world.lock().insts[instance].armed = true;
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let armed = {
            let mut s = self.world.lock();
            let seq = s.tick();
            s.insts[self.instance].dropped = Some(seq);
            s.insts[self.instance].dropped_at = Some(now());
            s.live_total -= 1;
            if let Some(slot) = &self.slot {
                *s.live.entry(slot.clone()).or_default() -= 1;
            }
            let armed = s.insts[self.instance].armed;
            if armed && self.hooks.drop_panic() && !std::thread::panicking() {
                s.insts[self.instance].drop_panicked = true;
            }
            armed
        };
        if armed && self.hooks.drop_panic() && !std::thread::panicking() {
            panic!("fuzz: injected state destructor panic");
        }
    }
}

impl Actor for Worker {
    async fn started(&mut self, ctx: &Context<Self>) {
        {
            let mut s = self.world.lock();
            let seq = s.tick();
            let id = ctx.id();
            s.by_id.insert(id, self.instance);
            let inst = &mut s.insts[self.instance];
            let twice = inst.started_begin.is_some();
            inst.id = Some(id);
            inst.started_begin = Some(seq);
            inst.handle = Some(ctx.actor_ref().child_handle());
            if let Some(task) = crate::fault::task() {
                s.tasks.insert(task, self.instance);
            }
            if twice {
                s.violate("L1: started() ran twice", format!("w{}", self.instance));
            }
        }
        let ms = self.hooks.started_sleep_ms();
        if ms > 0 {
            rt::sleep(Duration::from_millis(ms)).await;
        }
        if self.hooks.started_panic() {
            let mut s = self.world.lock();
            s.insts[self.instance].panicked = true;
            s.insts[self.instance].started_panicked = true;
            drop(s);
            inject_panic(&self.world, self.faults, "fuzz: injected started() panic");
        }
        let mut s = self.world.lock();
        let seq = s.tick();
        s.insts[self.instance].started_end = Some(seq);
        s.insts[self.instance].started_end_at = Some(now());
    }

    async fn stopped(&mut self, _ctx: &Context<Self>) {
        {
            let mut s = self.world.lock();
            let seq = s.tick();
            let inst = &mut s.insts[self.instance];
            let (twice, after_started_panic) =
                (inst.stopped_begin.is_some(), inst.started_panicked);
            inst.stopped_begin = Some(seq);
            inst.stopped_begin_at = Some(now());
            if twice {
                s.violate("L1: stopped() ran twice", format!("w{}", self.instance));
            }
            if after_started_panic {
                s.violate(
                    "L5: stopped() ran after a panic in started()",
                    format!("w{}", self.instance),
                );
            }
        }
        let ms = self.hooks.stopped_sleep_ms();
        if ms > 0 {
            rt::sleep(Duration::from_millis(ms)).await;
        }
        // Deferred replies may be answered from `stopped()` (R6).
        self.answer_all();
        if self.hooks.stopped_panic() {
            self.world.lock().insts[self.instance].panicked = true;
            inject_panic(&self.world, self.faults, "fuzz: injected stopped() panic");
        }
        let mut s = self.world.lock();
        let seq = s.tick();
        s.insts[self.instance].stopped_end = Some(seq);
    }

    async fn exit_received(&mut self, exit: Exit, ctx: &Context<Self>) {
        {
            let mut s = self.world.lock();
            self.turn(&mut s, "exit_received");
            let inst = &mut s.insts[self.instance];
            inst.ended.insert(exit.from);
            inst.exits.push((exit.from, exit.reason));
        }
        self.handler_fault(site::EXIT_PANIC, ctx, "fuzz: injected exit_received panic");
    }
}

impl Worker {
    /// A panic in this handler, if `site` was armed when the instance was
    /// constructed: a handler panic like any other (L4: `Panic`).
    fn handler_fault(&self, site: u16, ctx: &Context<Self>, what: &'static str) {
        if self.faults & site == 0 {
            return;
        }
        self.world.count(site);
        {
            let mut s = self.world.lock();
            s.insts[self.instance].panicked = true;
            s.stoppable(ctx.id());
        }
        inject_panic(&self.world, self.faults, what);
    }

    /// Hook order for any turn (L1/L2): after `started()` returned, before
    /// `stopped()` began.
    pub(crate) fn turn(&self, s: &mut State, what: &str) {
        let inst = &s.insts[self.instance];
        if inst.started_end.is_none() {
            let detail = format!("w{} {what}", self.instance);
            s.violate("L1: a handler ran before started() returned", detail);
        } else if inst.stopped_begin.is_some() {
            let detail = format!("w{} {what}", self.instance);
            s.violate("L1: a handler ran after stopped() began", detail);
        }
    }
}

impl Handler<Do> for Worker {
    async fn handle(
        &mut self,
        msg: Do,
        __reply: spawned_concurrency::PendingReply<'_, u64>,
        ctx: &Context<Self>,
    ) {
        let __value: u64 = async {
            {
                let mut s = self.world.lock();
                self.turn(&mut s, "Do");
                if let Some(t) = &msg.timer {
                    s.timers[t.index].handled += 1;
                }
                if msg.nonce != 0 {
                    if let Some(other) = s.handled.insert(msg.nonce, self.instance) {
                        let detail =
                            format!("nonce {} by w{other} and w{}", msg.nonce, self.instance);
                        s.violate("D2: a message was handled twice", detail);
                    }
                    if msg.lane == 0 {
                        let inst = &mut s.insts[self.instance];
                        let last = inst.last_lane0;
                        inst.last_lane0 = msg.nonce;
                        if msg.nonce <= last {
                            let detail = format!("w{}: {} after {last}", self.instance, msg.nonce);
                            s.violate("D1: per-sender FIFO broken", detail);
                        }
                    }
                }
            }
            match msg.act {
                Act::Noop => {}
                Act::Panic => {
                    self.world.lock().insts[self.instance].panicked = true;
                    inject_panic(&self.world, self.faults, "fuzz: injected handler panic");
                }
                Act::Sleep(d) => rt::sleep(d).await,
                Act::StopSelf(reason) => ctx.stop_with_reason(reason),
                Act::Note(to) => {
                    let _ = to.send(Note { from: ctx.id() });
                }
                Act::Link(peer) => {
                    self.uncertain(ctx.id(), Some(peer.id()));
                    ctx.link(&peer);
                }
                Act::Unlink(peer) => {
                    self.uncertain(ctx.id(), Some(peer.id()));
                    ctx.unlink(&peer);
                }
                Act::Monitor(target) => {
                    let mref = ctx.monitor(&target);
                    self.world.lock().monitors.push(MonRec {
                        watcher: ctx.id(),
                        target,
                        mref,
                        demonitored: false,
                        outside: false,
                    });
                }
                Act::Trap(on) => {
                    self.uncertain(ctx.id(), None);
                    ctx.trap_exit(on);
                }
                Act::Register(name) => {
                    let _ = ctx.register_name(&name);
                }
                Act::Helper(v) => {
                    let _ = ctx.spawn_helper(async move {
                        match v % 3 {
                            0 => std::future::pending::<()>().await,
                            1 => rt::sleep(Duration::from_millis(10 * v as u64)).await,
                            _ => panic!("fuzz: injected helper panic"),
                        }
                    });
                }
                Act::Stop(target, reason) => {
                    self.world.lock().stoppable(target.id());
                    target.stop_with_reason(reason);
                }
            }
            msg.nonce
        }
        .await;
        __reply.reply(__value);
    }
}

impl Worker {
    fn uncertain(&self, me: ActorId, peer: Option<ActorId>) {
        let mut s = self.world.lock();
        s.uncertain.insert(me);
        s.stoppable(me);
        if let Some(peer) = peer {
            s.uncertain.insert(peer);
            s.stoppable(peer);
        }
    }
}

impl Handler<Down> for Worker {
    async fn handle(
        &mut self,
        down: Down,
        __reply: spawned_concurrency::PendingReply<'_, ()>,
        ctx: &Context<Self>,
    ) {
        let __value: () = async {
            let mut s = self.world.lock();
            self.turn(&mut s, "Down");
            let me = ctx.id();
            let target = s
                .monitors
                .iter()
                .find(|m| m.mref == down.monitor_ref)
                .map(|m| (m.watcher, m.target.id()));
            // X2: `demonitor` flushes (OTP `[flush]`): once it returned, no
            // Down of the monitor is handled. A Down whose dispatch began
            // before it returned is handled in that same poll, before the
            // driver marks the record.
            if s.monitors
                .iter()
                .any(|m| m.mref == down.monitor_ref && m.demonitored)
            {
                s.violate(
                    "X2: a Down was handled after demonitor returned",
                    format!("w{}", self.instance),
                );
            }
            match target {
                Some((watcher, target)) if watcher == me => {
                    s.insts[self.instance].ended.insert(target);
                }
                Some(_) => s.violate(
                    "X1: Down delivered to an actor that did not create the monitor",
                    format!("w{}", self.instance),
                ),
                None => s.violate(
                    "X1: Down for an unknown monitor",
                    format!("w{}", self.instance),
                ),
            }
            let inst = &mut s.insts[self.instance];
            let duplicate = inst.downs.iter().any(|(r, _)| *r == down.monitor_ref);
            inst.downs.push((down.monitor_ref, down.reason));
            if duplicate {
                s.violate("X1: Down delivered twice", format!("w{}", self.instance));
            }
        }
        .await;
        self.handler_fault(site::DOWN_PANIC, ctx, "fuzz: injected Down handler panic");
        __reply.reply(__value);
    }
}

impl Handler<Note> for Worker {
    async fn handle(
        &mut self,
        note: Note,
        __reply: spawned_concurrency::PendingReply<'_, ()>,
        _ctx: &Context<Self>,
    ) {
        let __value: () = async {
            let mut s = self.world.lock();
            self.turn(&mut s, "Note");
            if s.insts[self.instance].ended.contains(&note.from) {
                let detail = format!("from {} at w{}", s.name(note.from), self.instance);
                s.violate("X7: a message arrived after the sender's Down/Exit", detail);
            }
        }
        .await;
        self.handler_fault(site::NOTE_PANIC, _ctx, "fuzz: injected Note handler panic");
        __reply.reply(__value);
    }
}

/// Virtual time now.
pub fn now() -> Duration {
    sim::now()
}

/// Record a factory call for `slot` under supervisor instance `inst` and
/// check that no earlier generation of the slot is still alive (V3).
pub fn factory_call(world: &World, inst: usize, slot: &str, mode: RestartMode) -> usize {
    let mut s = world.lock();
    let live = s.live.get(slot).copied().unwrap_or(0);
    let prefix = format!("{slot}/");
    let nested_live: i64 = s
        .live
        .range(prefix.clone()..)
        .take_while(|(k, _)| k.starts_with(&prefix))
        .map(|(_, v)| *v)
        .sum();
    if live != 0 || nested_live != 0 {
        s.violate(
            "V3: a replacement was constructed while an earlier generation was alive",
            format!("slot {slot}: {live} live, {nested_live} live below"),
        );
    }
    let root = s.sups[inst].root;
    if s.roots_done.get(root).copied().unwrap_or(false) {
        s.violate(
            "V9: a factory ran after its supervisor tree finished",
            format!("slot {slot}"),
        );
    }
    let at = now();
    let seq = s.tick();
    let index = slot
        .rsplit_once("/c")
        .and_then(|(_, i)| i.parse().ok())
        .unwrap_or(0);
    s.calls.push(Call {
        inst,
        slot: slot.to_string(),
        index,
        at,
        seq,
        mode,
        outcome: Outcome::Pending,
    });
    s.calls.len() - 1
}

/// Record how factory call `call` ended.
pub fn factory_outcome(world: &World, call: usize, outcome: Outcome) {
    let mut s = world.lock();
    s.calls[call].outcome = outcome;
    if let Outcome::Instance(i) = outcome {
        s.insts[i].call = Some(call);
    }
}
