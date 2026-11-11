//! The worker actor every scenario runs, and the per-run log that its hooks,
//! handlers, destructor and the supervisor factories write. Checks that can
//! be decided at the moment an event happens are made here; violations are
//! recorded (never panicked inside an actor, which the framework would turn
//! into an ordinary actor panic) and reported by the driver at the end.

use spawned_concurrency::{
    message::Message,
    supervision::SupervisedChild,
    tasks::{Actor, Context, Handler, Recipient},
    ActorId, ChildHandle, Down, Exit, ExitReason, MonitorRef,
};
use spawned_rt::tasks::{self as rt, sim};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use crate::scenario::Hooks;

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
    /// The state destructor panicked (injected): a secondary failure that may
    /// replace a clean requested reason (X8 skips such instances).
    pub drop_panicked: bool,
    /// Handed to the framework: only then may its destructor panic (a value
    /// the harness drops itself is not the framework's to contain).
    pub armed: bool,
    pub last_lane0: u64,
    pub downs: Vec<(MonitorRef, ExitReason)>,
    pub exits: Vec<(ActorId, ExitReason)>,
    /// Sources whose `Down`/`Exit` this instance already handled.
    pub ended: HashSet<ActorId>,
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
}

/// One factory invocation (worker factory or nested `configure` callback).
#[derive(Debug, Clone)]
pub struct Call {
    pub inst: usize,
    pub slot: String,
    pub at: Duration,
    pub temporary: bool,
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
    pub violations: Vec<Violation>,
}

pub struct World {
    state: Mutex<State>,
}

impl World {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State::default()),
        })
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
    fn tick(&mut self) -> u64 {
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
    pub act: Act,
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
    world: Arc<World>,
    instance: usize,
    hooks: Hooks,
    slot: Option<String>,
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
            panic!("fuzz: injected started() panic");
        }
        let mut s = self.world.lock();
        let seq = s.tick();
        s.insts[self.instance].started_end = Some(seq);
    }

    async fn stopped(&mut self, _ctx: &Context<Self>) {
        {
            let mut s = self.world.lock();
            let seq = s.tick();
            let inst = &mut s.insts[self.instance];
            let (twice, after_started_panic) =
                (inst.stopped_begin.is_some(), inst.started_panicked);
            inst.stopped_begin = Some(seq);
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
        if self.hooks.stopped_panic() {
            self.world.lock().insts[self.instance].panicked = true;
            panic!("fuzz: injected stopped() panic");
        }
        let mut s = self.world.lock();
        let seq = s.tick();
        s.insts[self.instance].stopped_end = Some(seq);
    }

    async fn exit_received(&mut self, exit: Exit, _ctx: &Context<Self>) {
        let mut s = self.world.lock();
        self.turn(&mut s, "exit_received");
        let inst = &mut s.insts[self.instance];
        inst.ended.insert(exit.from);
        inst.exits.push((exit.from, exit.reason));
    }
}

impl Worker {
    /// Hook order for any turn (L1/L2): after `started()` returned, before
    /// `stopped()` began.
    fn turn(&self, s: &mut State, what: &str) {
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
    async fn handle(&mut self, msg: Do, ctx: &Context<Self>) -> u64 {
        {
            let mut s = self.world.lock();
            self.turn(&mut s, "Do");
            if msg.nonce != 0 {
                if let Some(other) = s.handled.insert(msg.nonce, self.instance) {
                    let detail = format!("nonce {} by w{other} and w{}", msg.nonce, self.instance);
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
                panic!("fuzz: injected handler panic");
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
    async fn handle(&mut self, down: Down, ctx: &Context<Self>) {
        let mut s = self.world.lock();
        self.turn(&mut s, "Down");
        let me = ctx.id();
        let target = s
            .monitors
            .iter()
            .find(|m| m.mref == down.monitor_ref)
            .map(|m| (m.watcher, m.target.id()));
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
}

impl Handler<Note> for Worker {
    async fn handle(&mut self, note: Note, _ctx: &Context<Self>) {
        let mut s = self.world.lock();
        self.turn(&mut s, "Note");
        if s.insts[self.instance].ended.contains(&note.from) {
            let detail = format!("from {} at w{}", s.name(note.from), self.instance);
            s.violate("X7: a message arrived after the sender's Down/Exit", detail);
        }
    }
}

/// Virtual time now.
pub fn now() -> Duration {
    sim::now()
}

/// Record a factory call for `slot` under supervisor instance `inst` and
/// check that no earlier generation of the slot is still alive (V3).
pub fn factory_call(world: &World, inst: usize, slot: &str, temporary: bool) {
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
    s.calls.push(Call {
        inst,
        slot: slot.to_string(),
        at,
        temporary,
    });
}
