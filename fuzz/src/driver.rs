//! Interprets a [`Scenario`] inside one simulated world and checks the
//! semantics contract. Everything here runs on the simulator's root task, in
//! input order; the only iteration over hashed collections is for lookups,
//! so a run is a function of the input, the seed and the strategy.

use crate::scenario::*;
use crate::world::{factory_call, now, Act, Do, SupInst, Worker, World};
use spawned_concurrency::{
    registry,
    supervision::{
        Backoff, ChildSpec, ChildState, FactoryContext, RestartMode, SupervisedChild,
        SupervisedSupervisor, Supervisor, SupervisorBuilder, SupervisorConfig, SupervisorError,
        SupervisorSpec, SupervisorStrategy,
    },
    system::{ActorSystem, ExecutionGroup, ShutdownReport},
    tasks::{send_after, send_interval, ActorRef, ActorStart, TimerHandle},
    ActorError, ActorId, ChildHandle, ExitReason,
};
use spawned_rt::tasks::{self as rt, sim, JoinHandle};
use std::{
    cell::RefCell,
    collections::{BTreeSet, HashMap},
    sync::Arc,
    time::Duration,
};

/// Generous deadline: every hook and handler sleep is bounded far below it,
/// so a shutdown or fence that misses it is a liveness failure.
const LONG: Duration = Duration::from_secs(600);
/// Virtual time a supervisor's initial start may take before it is cancelled.
const START_LIMIT: Duration = Duration::from_secs(30);
/// Virtual time teardown cleanup may take before leftovers count as leaks.
const LEAK_WAIT: Duration = Duration::from_secs(120);
const MAX_ACTORS: usize = 16;
const MAX_ROOTS: usize = 4;
const MAX_SYSTEMS: usize = 3;
const MAX_CHILDREN: usize = 4;
const MAX_TIMERS: usize = 16;
/// Monitors tracked (inside and outside); later monitor commands are no-ops.
const MAX_MONITORS: usize = 128;
const MAX_PENDING: usize = 16;
const REGISTERED_PREFIX: &str = "fz-reg-";

thread_local! {
    /// Details of the last failing run's violations, for [`crate::fuzz_one`].
    pub static LAST_DETAILS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

struct Sys {
    system: Option<ActorSystem>,
    groups: Vec<ExecutionGroup>,
    /// `shutdown` was awaited: admission must be closed.
    shut: bool,
}

struct Standalone {
    aref: ActorRef<Worker>,
    instance: usize,
    sys: Option<usize>,
}

enum Slot {
    Worker(SupervisedChild<Worker>),
    Nested(SupervisedSupervisor),
}

struct Root {
    sup: Option<Supervisor>,
    sys: usize,
    /// Declared child paths, with the declared child count of nested slots.
    paths: Vec<(String, usize)>,
    slots: Vec<Slot>,
    /// Stop times stay inside every shutdown timeout, so reverse order is
    /// required (a missed timeout signals the remaining siblings early).
    ordered_ok: bool,
    done: bool,
    /// A stop or shutdown was already requested (a short `shutdown` that
    /// timed out signals every remaining child at once): no order check.
    closing: bool,
    /// For TLA+ trace export: strategy, budget and child restart modes, set
    /// only for flat trees whose children were all declared.
    #[allow(dead_code)]
    model: Option<(SupervisorStrategy, usize, Vec<RestartMode>)>,
}

/// Admission order of standalone managed actors and supervisor roots.
#[derive(Clone, Copy)]
enum Unit {
    Actor(usize),
    Root(usize),
}

struct LinkRec {
    a: ActorRef<Worker>,
    b: ActorRef<Worker>,
    at: u64,
    trap_a: bool,
    trap_b: bool,
    unlinked: bool,
}

type Reply = (u64, u64, Duration, Result<u64, ActorError>);

struct Driver {
    world: Arc<World>,
    seed: u64,
    op: u64,
    nonce: u64,
    lane: u64,
    systems: Vec<Sys>,
    actors: Vec<Standalone>,
    roots: Vec<Root>,
    units: Vec<(usize, Unit)>,
    timers: Vec<TimerHandle>,
    pending: Vec<JoinHandle<Reply>>,
    links: Vec<LinkRec>,
    /// Trap flag set by the driver and the op index of its last change.
    traps: HashMap<ActorId, (bool, u64)>,
    handles: HashMap<ActorId, ChildHandle>,
    first_exit: HashMap<ActorId, ExitReason>,
    /// Total handler sleep ever sent: a fence may wait behind all of it.
    queued_sleep: Duration,
}

/// Run one scenario in the current simulated world; panics with the first
/// violated oracle.
pub async fn run_body(scenario: Scenario, seed: u64) {
    let world = World::new();
    let baseline = sim::stats().tasks;
    let mut driver = Driver::new(world.clone(), seed);
    if std::env::var_os("SPAWNED_FUZZ_WATCH").is_some() {
        // Debugging aid (changes the schedule): report progress while the
        // driver is blocked.
        drop(rt::spawn(async {
            let mut last = 0;
            loop {
                let stats = sim::stats();
                if stats.steps >= last + 200_000 {
                    last = stats.steps;
                    eprintln!(
                        "[watch] {:?} poll {} tasks {} ready {}",
                        stats.now, stats.steps, stats.tasks, stats.ready
                    );
                }
                sim::yield_now().await;
            }
        }));
    }
    for op in scenario.ops.iter().take(MAX_OPS) {
        driver.op += 1;
        trace(|| format!("op {}: {op:?}", driver.op));
        driver.apply(op).await;
    }
    trace(|| "teardown".into());
    driver.teardown().await;
    trace(|| "leak checks".into());
    drop(driver);
    leaks(&world, baseline).await;
    budget_checks(&world);
    let violations = std::mem::take(&mut world.lock().violations);
    if let Some(first) = violations.first() {
        LAST_DETAILS.with(|d| {
            *d.borrow_mut() = violations
                .iter()
                .map(|v| format!("{}: {}", v.kind, v.detail))
                .collect()
        });
        panic!("spawned-fuzz oracle: {}", first.kind);
    }
}

/// `SPAWNED_FUZZ_TRACE=1`: print driver progress with virtual time and polls.
fn trace(what: impl FnOnce() -> String) {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *ON.get_or_init(|| std::env::var_os("SPAWNED_FUZZ_TRACE").is_some()) {
        let stats = sim::stats();
        eprintln!("[{:?} poll {}] {}", stats.now, stats.steps, what());
    }
}

/// After teardown: every worker state dropped, no registration left, and
/// the simulator's task table back to where the run began.
async fn leaks(world: &World, baseline: usize) {
    let clean = |w: &World| {
        let live = w.lock().live_total;
        let names = registry::registered()
            .into_iter()
            .filter(|n| n.starts_with(REGISTERED_PREFIX))
            .count();
        (live, names, sim::stats().tasks)
    };
    // Yield first (cleanup that is merely scheduled), then let virtual time
    // pass: a dropped system's shutdown runs with a 5 s grace and hooks sleep
    // up to 2.75 s each. LEAK_WAIT bounds the virtual time allowed.
    let start = now();
    let mut i = 0u64;
    while now() - start < LEAK_WAIT {
        let (live, names, tasks) = clean(world);
        if live == 0 && names == 0 && tasks <= baseline {
            trace(|| format!("released after {:?}", now() - start));
            return;
        }
        i += 1;
        if i.is_multiple_of(100) {
            rt::sleep(Duration::from_millis(100)).await;
        } else {
            sim::yield_now().await;
        }
    }
    let (live, names, tasks) = clean(world);
    if live != 0 {
        world.violate(
            "M5/S3: worker state not released after shutdown",
            format!("{live} live"),
        );
    }
    if names != 0 {
        world.violate(
            "S5: registration left after shutdown",
            format!("{names} names"),
        );
    }
    if tasks > baseline {
        world.violate(
            "S3: simulated tasks left after shutdown",
            format!("{tasks} tasks, {baseline} at start"),
        );
    }
}

fn reason(r: Reason) -> ExitReason {
    match r {
        Reason::Normal => ExitReason::Normal,
        Reason::Shutdown => ExitReason::Shutdown,
        Reason::Kill => ExitReason::Kill,
        Reason::Panic => ExitReason::Panic("fuzz: injected stop".into()),
    }
}

fn restart_mode(v: u8) -> RestartMode {
    match v % 3 {
        0 => RestartMode::Permanent,
        1 => RestartMode::Transient,
        _ => RestartMode::Temporary,
    }
}

fn config(p: &Policy, seed: u64) -> SupervisorConfig {
    let initial = Duration::from_millis(p.backoff as u64);
    SupervisorConfig {
        strategy: match p.strategy % 3 {
            0 => SupervisorStrategy::OneForOne,
            1 => SupervisorStrategy::OneForAll,
            _ => SupervisorStrategy::RestForOne,
        },
        max_restarts: (p.max_restarts % 7) as usize,
        // At least 50 ms, so a fast restart loop exhausts the budget
        // instead of restarting forever as virtual time creeps.
        restart_window: Duration::from_millis(50 + 2 * p.window as u64),
        backoff: Backoff {
            initial,
            maximum: initial * 4,
            stable_after: Duration::from_millis(10 * p.stable as u64 + 1),
            jitter: p.jitter,
            // Never the default (wall-clock) jitter seed.
            seed: Some(seed),
        },
        ..SupervisorConfig::default()
    }
}

fn cfg_strategy(p: &Policy) -> SupervisorStrategy {
    config(p, 0).strategy
}

fn new_sup_inst(world: &World, root: usize, cfg: &SupervisorConfig) -> usize {
    let mut s = world.lock();
    s.sups.push(SupInst {
        root,
        max_restarts: cfg.max_restarts,
        window: cfg.restart_window,
        cohort: !matches!(cfg.strategy, SupervisorStrategy::OneForOne),
    });
    s.sups.len() - 1
}

fn worker_spec(
    world: Arc<World>,
    inst: usize,
    path: String,
    def: WorkerDef,
    group: &ExecutionGroup,
) -> ChildSpec<Worker> {
    let temporary = def.restart % 3 == 2;
    let name = path.clone();
    ChildSpec::new(name, group, move |fctx: FactoryContext| {
        let world = world.clone();
        let path = path.clone();
        async move {
            factory_call(&world, inst, &path, temporary);
            let bit = 1u8 << (fctx.generation() % 8);
            if def.fail_mask & bit != 0 {
                return Err("fuzz: injected factory error".to_string());
            }
            if def.panic_mask & bit != 0 {
                panic!("fuzz: injected factory panic");
            }
            let hooks = if def.hook_mask & bit != 0 {
                def.hooks
            } else {
                Hooks::default()
            };
            let worker = Worker::new(&world, Some(path), hooks);
            Worker::arm(&world, worker.instance());
            Ok(worker)
        }
    })
    .restart(restart_mode(def.restart))
    .startup_timeout(Duration::from_millis(limit_ms(def.startup_timeout)))
    .shutdown_timeout(Duration::from_millis(limit_ms(def.shutdown_timeout)))
}

/// Shutdown order is checked only when every stop fits its timeout.
fn worker_stops_in_time(def: &WorkerDef) -> bool {
    def.hooks.stopped_sleep_ms() < limit_ms(def.shutdown_timeout)
}

impl Driver {
    fn new(world: Arc<World>, seed: u64) -> Self {
        let mut driver = Self {
            world,
            seed,
            op: 0,
            nonce: 0,
            lane: 1_000_000,
            systems: Vec::new(),
            actors: Vec::new(),
            roots: Vec::new(),
            units: Vec::new(),
            timers: Vec::new(),
            pending: Vec::new(),
            links: Vec::new(),
            traps: HashMap::new(),
            handles: HashMap::new(),
            first_exit: HashMap::new(),
            queued_sleep: Duration::ZERO,
        };
        driver.new_system();
        driver
    }

    fn violate(&self, kind: &'static str, detail: String) {
        self.world.violate(kind, detail);
    }

    fn next_nonce(&mut self) -> u64 {
        self.nonce += 1;
        self.nonce
    }

    fn new_system(&mut self) {
        if self.systems.len() >= MAX_SYSTEMS {
            return;
        }
        let i = self.systems.len();
        let Ok(system) = ActorSystem::new() else {
            self.violate("S1: ActorSystem::new failed", String::new());
            return;
        };
        let groups = (0..2)
            .filter_map(|j| system.async_group(format!("fz-s{i}-g{j}"), 1 + j).ok())
            .collect();
        self.systems.push(Sys {
            system: Some(system),
            groups,
            shut: false,
        });
    }

    fn all_groups(&self) -> Vec<(usize, ExecutionGroup)> {
        self.systems
            .iter()
            .enumerate()
            .flat_map(|(i, s)| s.groups.iter().map(move |g| (i, g.clone())))
            .collect()
    }

    /// Every actor an operand can name: standalone actors (alive or not),
    /// then the current generation of each supervised worker.
    fn targets(&mut self) -> Vec<ActorRef<Worker>> {
        let mut out: Vec<ActorRef<Worker>> = self.actors.iter().map(|a| a.aref.clone()).collect();
        for root in &self.roots {
            for slot in &root.slots {
                if let Slot::Worker(child) = slot {
                    out.extend(child.current().ok());
                }
            }
        }
        let nested: Vec<SupervisedChild<Worker>> =
            self.world.lock().nested.values().cloned().collect();
        out.extend(nested.iter().filter_map(|c| c.current().ok()));
        for a in &out {
            self.handles
                .entry(a.id())
                .or_insert_with(|| a.child_handle());
        }
        out
    }

    fn pick(&mut self, index: u8) -> Option<ActorRef<Worker>> {
        let targets = self.targets();
        (!targets.is_empty()).then(|| targets[index as usize % targets.len()].clone())
    }

    fn may_stop(&self, id: ActorId) {
        self.world.lock().may_stop.insert(id);
    }

    /// Resolve a command's operands against the current population.
    fn act(&mut self, cmd: Cmd, to: &ActorRef<Worker>) -> Act {
        match cmd {
            Cmd::Noop => Act::Noop,
            Cmd::Panic => {
                self.may_stop(to.id());
                Act::Panic
            }
            Cmd::Sleep(v) => {
                let d = Duration::from_millis(20 * v as u64);
                self.queued_sleep += d;
                Act::Sleep(d)
            }
            Cmd::StopSelf(r) => {
                self.may_stop(to.id());
                Act::StopSelf(reason(r))
            }
            Cmd::Note(i) => match self.pick(i) {
                Some(peer) => Act::Note(peer.recipient()),
                None => Act::Noop,
            },
            Cmd::Link(i) => match self.pick(i) {
                Some(peer) => Act::Link(peer.child_handle()),
                None => Act::Noop,
            },
            Cmd::Unlink(i) => match self.pick(i) {
                Some(peer) => Act::Unlink(peer.child_handle()),
                None => Act::Noop,
            },
            Cmd::Monitor(i) => match self.pick(i) {
                Some(peer) if self.world.lock().monitors.len() < MAX_MONITORS => {
                    Act::Monitor(peer.child_handle())
                }
                _ => Act::Noop,
            },
            Cmd::Trap(on) => Act::Trap(on),
            Cmd::Helper(v) => Act::Helper(v),
            Cmd::Register(v) => Act::Register(format!("{REGISTERED_PREFIX}{}", v % 4)),
            Cmd::Stop(i, r) => match self.pick(i) {
                Some(peer) => {
                    self.may_stop(peer.id());
                    Act::Stop(peer.child_handle(), reason(r))
                }
                None => Act::Noop,
            },
        }
    }

    fn check_reply(
        &self,
        nonce: u64,
        timeout_ms: u64,
        elapsed: Duration,
        reply: Result<u64, ActorError>,
    ) {
        match reply {
            Ok(n) => {
                if n != nonce {
                    self.violate(
                        "R2: a reply reached the wrong request",
                        format!("{n} for {nonce}"),
                    );
                }
                if !self.world.lock().handled.contains_key(&nonce) {
                    self.violate(
                        "R1: Ok reply without the handler running",
                        format!("{nonce}"),
                    );
                }
            }
            Err(ActorError::RequestTimeout) => {
                if elapsed < Duration::from_millis(timeout_ms) {
                    self.violate(
                        "R3: request timed out before its timeout",
                        format!("{elapsed:?} < {timeout_ms} ms"),
                    );
                }
            }
            // Answered at the death point, or sent after it (0.8: NoProc).
            Err(ActorError::ActorStopped(_) | ActorError::NoProc) => {}
            #[allow(unreachable_patterns)]
            Err(other) => self.violate("R1: unexpected request error", format!("{other}")),
        }
    }

    async fn collect_pending(&mut self) {
        for handle in std::mem::take(&mut self.pending) {
            match rt::timeout(LONG * 2, handle).await {
                Ok(Ok((nonce, ms, elapsed, reply))) => self.check_reply(nonce, ms, elapsed, reply),
                Ok(Err(e)) => self.violate("R1: a request task failed", format!("{e}")),
                Err(_) => self.violate("R1: a request never resolved", String::new()),
            }
        }
    }

    async fn apply(&mut self, op: &Op) {
        match *op {
            Op::Spawn { how, hooks } => self.spawn(how, hooks),
            Op::Send {
                to,
                cmd,
                via_recipient,
            } => {
                let Some(aref) = self.pick(to) else { return };
                let act = self.act(cmd, &aref);
                let msg = Do {
                    nonce: self.next_nonce(),
                    lane: 0,
                    act,
                };
                let _ = if via_recipient {
                    aref.recipient::<Do>().send(msg)
                } else {
                    aref.send(msg)
                };
            }
            Op::Request {
                to,
                cmd,
                timeout,
                detached,
            } => {
                let Some(aref) = self.pick(to) else { return };
                let act = self.act(cmd, &aref);
                let nonce = self.next_nonce();
                let lane = if detached {
                    self.lane += 1;
                    self.lane
                } else {
                    0
                };
                let ms = timeout.map_or(5_000, |t| t as u64);
                let msg = Do { nonce, lane, act };
                let request = async move {
                    let start = now();
                    let reply = match timeout {
                        Some(_) => {
                            aref.request_with_timeout(msg, Duration::from_millis(ms))
                                .await
                        }
                        None => aref.request(msg).await,
                    };
                    (nonce, ms, now() - start, reply)
                };
                if detached {
                    if self.pending.len() >= MAX_PENDING {
                        self.collect_pending().await;
                    }
                    self.pending.push(rt::spawn(request));
                } else {
                    let (nonce, ms, elapsed, reply) = request.await;
                    self.check_reply(nonce, ms, elapsed, reply);
                }
            }
            Op::Link { a, b } => {
                let (Some(pa), Some(pb)) = (self.pick(a), self.pick(b)) else {
                    return;
                };
                self.may_stop(pa.id());
                self.may_stop(pb.id());
                // A context whose actor already exited, or whose exit began,
                // cannot link (0.8, as in OTP: an exiting process cannot call
                // link): the call is inert, so nothing is predicted for the
                // pair. (`exit_pending` also counts a queued parent exit, which
                // does not make the link inert: the check is then skipped,
                // conservatively.)
                let linker_exited =
                    pa.child_handle().exit_reason().is_some() || pa.context().exit_pending();
                pa.context().link(&pb.child_handle());
                let trap = |d: &Self, id| d.traps.get(&id).is_some_and(|t| t.0);
                self.links.push(LinkRec {
                    trap_a: trap(self, pa.id()),
                    trap_b: trap(self, pb.id()),
                    a: pa,
                    b: pb,
                    at: self.op,
                    unlinked: linker_exited,
                });
            }
            Op::Unlink { a, b } => {
                let (Some(pa), Some(pb)) = (self.pick(a), self.pick(b)) else {
                    return;
                };
                pa.context().unlink(&pb.child_handle());
                for l in &mut self.links {
                    let ids = (l.a.id(), l.b.id());
                    if ids == (pa.id(), pb.id()) || ids == (pb.id(), pa.id()) {
                        l.unlinked = true;
                    }
                }
            }
            Op::Monitor { watcher, target } => {
                let (Some(w), Some(t)) = (self.pick(watcher), self.pick(target)) else {
                    return;
                };
                if self.world.lock().monitors.len() >= MAX_MONITORS {
                    return;
                }
                let mref = w.context().monitor(&t.child_handle());
                self.world.lock().monitors.push(crate::world::MonRec {
                    watcher: w.id(),
                    target: t.child_handle(),
                    mref,
                    demonitored: false,
                    outside: true,
                });
            }
            Op::Demonitor { which } => {
                let record = {
                    let s = self.world.lock();
                    (!s.monitors.is_empty()).then(|| {
                        (
                            which as usize % s.monitors.len(),
                            s.monitors[which as usize % s.monitors.len()].clone(),
                        )
                    })
                };
                let Some((index, m)) = record else { return };
                let watcher = self.targets().into_iter().find(|a| a.id() == m.watcher);
                if let Some(w) = watcher {
                    w.context().demonitor(m.mref);
                    self.world.lock().monitors[index].demonitored = true;
                }
            }
            Op::TrapExit { who, on } => {
                let Some(aref) = self.pick(who) else { return };
                aref.context().trap_exit(on);
                self.traps.insert(aref.id(), (on, self.op));
            }
            Op::Stop { who, reason: r } => {
                let Some(aref) = self.pick(who) else { return };
                self.may_stop(aref.id());
                let handle = aref.child_handle();
                match r {
                    Reason::Normal => handle.stop(),
                    Reason::Shutdown => handle.shutdown(),
                    r => handle.stop_with_reason(reason(r)),
                }
            }
            Op::Timer {
                to,
                ms,
                cmd,
                interval,
            } => {
                if self.timers.len() >= MAX_TIMERS {
                    return;
                }
                let Some(aref) = self.pick(to) else { return };
                // A repeating sleep would queue unbounded work (one tick per
                // millisecond, each sleeping seconds): fences could never
                // catch up. One-shot timers may sleep.
                // Nor do repeating links, monitors or registrations: they
                // would grow the harness's tables by one per millisecond.
                let cmd = match cmd {
                    Cmd::Sleep(_)
                    | Cmd::Monitor(_)
                    | Cmd::Link(_)
                    | Cmd::Unlink(_)
                    | Cmd::Register(_)
                    | Cmd::Helper(_)
                        if interval =>
                    {
                        Cmd::Noop
                    }
                    cmd => cmd,
                };
                let act = self.act(cmd, &aref);
                let msg = Do {
                    nonce: 0,
                    lane: 0,
                    act,
                };
                let period = Duration::from_millis(1 + ms as u64 % 5_000);
                let timer = if interval {
                    send_interval(period, aref.context(), msg)
                } else {
                    send_after(period, aref.context(), msg)
                };
                self.timers.push(timer);
            }
            Op::CancelTimer { which } => {
                if !self.timers.is_empty() {
                    let i = which as usize % self.timers.len();
                    self.timers[i].cancellation_token.cancel();
                }
            }
            Op::Advance { ms } => rt::sleep(Duration::from_millis(ms as u64 % 5_000)).await,
            Op::Yield { n } => {
                for _ in 0..n % 64 {
                    sim::yield_now().await;
                }
            }
            Op::Supervisor(ref spec) => self.supervisor(spec).await,
            Op::CrashChild { sup, child, how } => self.crash_child(sup, child, how),
            Op::StopSupervisor { sup, mode, ms } => self.stop_supervisor(sup, mode, ms).await,
            Op::NewSystem => self.new_system(),
            Op::ShutdownSystem { sys, mode, ms } => self.shutdown_system(sys, mode, ms).await,
            Op::DropSystem { sys } => {
                if self.systems.is_empty() {
                    return;
                }
                let i = sys as usize % self.systems.len();
                if self.systems[i].system.take().is_some() {
                    self.system_stopping(i);
                }
            }
            Op::DropReply { to, cmd } => {
                let Some(aref) = self.pick(to) else { return };
                let act = self.act(cmd, &aref);
                let msg = Do {
                    nonce: self.next_nonce(),
                    lane: 0,
                    act,
                };
                drop(aref.request_raw(msg));
            }
            Op::WaitGeneration { sup, child, ms } => self.wait_generation(sup, child, ms).await,
            Op::Fence => {
                if self.quiesce().await {
                    self.eventual().await;
                }
            }
        }
    }

    fn spawn(&mut self, how: SpawnHow, hooks: Hooks) {
        if self.actors.len() >= MAX_ACTORS {
            return;
        }
        let worker = Worker::new(&self.world, None, hooks);
        let instance = worker.instance();
        let (aref, sys) = match how {
            SpawnHow::Unmanaged => (worker.start(), None),
            SpawnHow::Managed { group } => {
                let groups = self.all_groups();
                if groups.is_empty() {
                    return;
                }
                let (sys, group) = &groups[group as usize % groups.len()];
                match worker.start_on(group) {
                    Ok(aref) => {
                        if self.systems[*sys].shut {
                            self.violate(
                                "S1: start_on admitted an actor after system shutdown",
                                format!("system {sys}"),
                            );
                        }
                        (aref, Some(*sys))
                    }
                    Err(_) => return,
                }
            }
            SpawnHow::Linked { parent } => {
                if self.actors.is_empty() {
                    (worker.start(), None)
                } else {
                    let parent = self.actors[parent as usize % self.actors.len()]
                        .aref
                        .clone();
                    let aref = worker.start_linked(&parent.context());
                    self.may_stop(parent.id());
                    self.may_stop(aref.id());
                    self.links.push(LinkRec {
                        trap_a: self.traps.get(&parent.id()).is_some_and(|t| t.0),
                        trap_b: false,
                        a: parent,
                        b: aref.clone(),
                        at: self.op,
                        unlinked: false,
                    });
                    (aref, None)
                }
            }
        };
        Worker::arm(&self.world, instance);
        if hooks.started_panic() {
            self.may_stop(aref.id());
        }
        self.handles.insert(aref.id(), aref.child_handle());
        if let Some(sys) = sys {
            if self.systems[sys].shut || self.systems[sys].system.is_none() {
                self.may_stop(aref.id());
            }
            self.units.push((sys, Unit::Actor(self.actors.len())));
        }
        self.actors.push(Standalone {
            aref,
            instance,
            sys,
        });
    }

    async fn supervisor(&mut self, spec: &SupSpec) {
        if self.roots.len() >= MAX_ROOTS || self.systems.is_empty() {
            return;
        }
        let sys = spec.system as usize % self.systems.len();
        let Some(system) = self.systems[sys].system.as_ref() else {
            return;
        };
        let root = self.roots.len();
        let cfg = config(&spec.policy, self.seed);
        let inst = new_sup_inst(&self.world, root, &cfg);
        let Ok(mut builder) = system.supervisor(cfg) else {
            return;
        };
        let groups: Vec<ExecutionGroup> = self.all_groups().into_iter().map(|(_, g)| g).collect();
        if groups.is_empty() {
            return;
        }
        let mut paths = Vec::new();
        let mut slots = Vec::new();
        let mut ordered_ok = true;
        let mut modes = Some(Vec::new());
        for (i, def) in spec.children.iter().take(MAX_CHILDREN).enumerate() {
            let path = format!("r{root}/c{i}");
            match def {
                ChildDef::Worker(w) => {
                    ordered_ok &= worker_stops_in_time(w);
                    let group = &groups[w.group as usize % groups.len()];
                    let spec = worker_spec(self.world.clone(), inst, path.clone(), *w, group);
                    if let Ok(child) = builder.add(spec) {
                        slots.push(Slot::Worker(child));
                        paths.push((path, 0));
                        if let Some(m) = &mut modes {
                            m.push(restart_mode(w.restart));
                        }
                    } else {
                        modes = None;
                    }
                }
                ChildDef::Nested {
                    restart,
                    group,
                    policy,
                    children,
                } => {
                    modes = None;
                    let children: Vec<WorkerDef> =
                        children.iter().take(MAX_CHILDREN).copied().collect();
                    // The nested slot's own stop (all its children, in turn)
                    // must fit its 5 s shutdown timeout too.
                    let total: u64 = children.iter().map(|c| c.hooks.stopped_sleep_ms()).sum();
                    ordered_ok &= children.iter().all(worker_stops_in_time) && total < 5_000;
                    let count = children.len();
                    let nested_cfg = config(policy, self.seed);
                    let configure = nested_configure(
                        self.world.clone(),
                        root,
                        inst,
                        path.clone(),
                        *restart % 3 == 2,
                        children,
                        groups.clone(),
                        nested_cfg.clone(),
                    );
                    let group = &groups[*group as usize % groups.len()];
                    let spec = SupervisorSpec::new(path.clone(), group, nested_cfg, configure)
                        .restart(restart_mode(*restart));
                    if let Ok(child) = builder.add_supervisor(spec) {
                        slots.push(Slot::Nested(child));
                        paths.push((path, count));
                    }
                }
            }
        }
        let model = modes.filter(|m| !m.is_empty()).map(|m| {
            (
                cfg_strategy(&spec.policy),
                (spec.policy.max_restarts % 7) as usize,
                m,
            )
        });
        self.units.push((sys, Unit::Root(root)));
        trace(|| format!("supervisor r{root}: start"));
        // A tree whose subtree restarts slower than its budget refills
        // retries forever (a legal configuration); cancelling the start
        // future rolls it back (V7) and keeps the run bounded.
        let sup = match rt::timeout(START_LIMIT, builder.start()).await {
            Ok(result) => result.ok(),
            Err(_) => None,
        };
        trace(|| format!("supervisor r{root}: started {}", sup.is_some()));
        self.roots.push(Root {
            sup,
            sys,
            paths,
            slots,
            ordered_ok,
            done: false,
            closing: false,
            model,
        });
        if self.systems[sys].shut {
            self.world.lock().roots_done.resize(self.roots.len(), false);
        }
    }

    fn crash_child(&mut self, sup: u8, child: u8, how: ChildCrash) {
        if self.roots.is_empty() {
            return;
        }
        let root = sup as usize % self.roots.len();
        let prefix = format!("r{root}/");
        let mut workers: Vec<SupervisedChild<Worker>> = Vec::new();
        let mut nested: Vec<SupervisedSupervisor> = Vec::new();
        for slot in &self.roots[root].slots {
            match slot {
                Slot::Worker(c) => workers.push(c.clone()),
                Slot::Nested(n) => nested.push(n.clone()),
            }
        }
        workers.extend(
            self.world
                .lock()
                .nested
                .iter()
                .filter(|(k, _)| k.starts_with(&prefix))
                .map(|(_, c)| c.clone()),
        );
        let total = workers.len() + nested.len();
        if total == 0 {
            return;
        }
        let i = child as usize % total;
        if i >= workers.len() || how == ChildCrash::StopNested {
            if let Some(n) = nested.get(i.saturating_sub(workers.len()) % nested.len().max(1)) {
                if let Ok(s) = n.current() {
                    s.stop();
                }
            }
            return;
        }
        let Ok(aref) = workers[i].current() else {
            return;
        };
        if !aref.child_handle().is_alive() {
            self.violate(
                "V4: current() returned a generation that already exited",
                format!("{:?}", workers[i].snapshot().state),
            );
        }
        match how {
            ChildCrash::Panic => {
                let nonce = self.next_nonce();
                let _ = aref.send(Do {
                    nonce,
                    lane: 0,
                    act: Act::Panic,
                });
            }
            ChildCrash::Stop(r) => aref.child_handle().stop_with_reason(reason(r)),
            ChildCrash::StopNested => {}
        }
    }

    async fn wait_generation(&mut self, sup: u8, child: u8, ms: u16) {
        if self.roots.is_empty() {
            return;
        }
        let root = sup as usize % self.roots.len();
        let prefix = format!("r{root}/");
        let mut workers: Vec<SupervisedChild<Worker>> = self.roots[root]
            .slots
            .iter()
            .filter_map(|s| match s {
                Slot::Worker(c) => Some(c.clone()),
                Slot::Nested(_) => None,
            })
            .collect();
        workers.extend(
            self.world
                .lock()
                .nested
                .iter()
                .filter(|(k, _)| k.starts_with(&prefix))
                .map(|(_, c)| c.clone()),
        );
        if workers.is_empty() {
            return;
        }
        let c = &workers[child as usize % workers.len()];
        let after = c.snapshot().generation;
        let wait = c.wait_for_generation(after);
        if let Ok(Ok(aref)) = rt::timeout(Duration::from_millis(ms as u64 % 10_000), wait).await {
            let generation = c.snapshot().generation;
            if generation <= after {
                self.violate(
                    "V4: wait_for_generation returned an older generation",
                    format!("{generation} <= {after}"),
                );
            }
            if !aref.child_handle().is_alive() {
                self.violate(
                    "V4: wait_for_generation returned a generation that already exited",
                    String::new(),
                );
            }
        }
    }

    /// Check a supervisor whose shutdown reported complete (V9).
    fn root_done(
        &mut self,
        root: usize,
        children: &[spawned_concurrency::supervision::ChildSnapshot],
    ) {
        for c in children {
            if !matches!(c.state, ChildState::Stopped | ChildState::Failed) {
                self.violate(
                    "V9: a finished supervisor reports a child neither Stopped nor Failed",
                    format!("r{root} {} {:?}", c.name, c.state),
                );
            }
        }
        let prefix = format!("r{root}/");
        let live: i64 = {
            let s = self.world.lock();
            s.live
                .iter()
                .filter(|(k, _)| k.starts_with(&prefix))
                .map(|(_, v)| *v)
                .sum()
        };
        if live != 0 {
            self.violate(
                "V9: a finished supervisor left a live generation",
                format!("r{root}: {live}"),
            );
        }
        self.roots[root].done = true;
        #[cfg(feature = "lifecycle-trace")]
        self.export_trace(root);
        let mut s = self.world.lock();
        if s.roots_done.len() <= root {
            s.roots_done.resize(root + 1, false);
        }
        s.roots_done[root] = true;
    }

    /// Write a finished flat tree's lifecycle trace for
    /// `verification/tla/trace_check.py` (to `SPAWNED_FUZZ_TLA_DIR`).
    #[cfg(feature = "lifecycle-trace")]
    fn export_trace(&self, root: usize) {
        use spawned_concurrency::lifecycle_trace;
        let Some(sup) = &self.roots[root].sup else {
            return;
        };
        let events = lifecycle_trace::take(sup.trace_id());
        let (Some(dir), Some((strategy, max_restarts, modes))) = (
            std::env::var_os("SPAWNED_FUZZ_TLA_DIR"),
            self.roots[root].model.as_ref(),
        ) else {
            return;
        };
        if !events.iter().any(|e| e.action == "SupJoin") {
            return;
        }
        // Cohorts.tla models only the cohort actions (as cohort_trace.rs).
        let events: Vec<_> = if *strategy == SupervisorStrategy::OneForOne {
            events
        } else {
            events
                .into_iter()
                .filter(|e| {
                    matches!(
                        e.action,
                        "CRequest"
                            | "CSelect"
                            | "CAssign"
                            | "CStop"
                            | "CClean"
                            | "CResume"
                            | "CBegin"
                            | "CReady"
                            | "CJoined"
                            | "CRetire"
                            | "CAck"
                            | "CEnd"
                            | "CInitialReady"
                            | "CClose"
                            | "CDone"
                    )
                })
                .collect()
        };
        let modes: Vec<String> = modes.iter().map(|m| format!("\"{m:?}\"")).collect();
        let events: Vec<String> = events.iter().map(|e| e.to_json()).collect();
        let name = format!("fuzz-{}-r{root}", self.seed);
        let header = match strategy {
            SupervisorStrategy::OneForOne => format!("\"max_restarts\":{max_restarts}"),
            other => format!("\"schema\":\"cohort-v1\",\"strategy\":\"{other:?}\""),
        };
        let json = format!(
            "{{{header},\"name\":\"{name}\",\"n\":{},\"modes\":[{}],\"events\":[\n{}\n]}}\n",
            modes.len(),
            modes.join(","),
            events.join(",\n")
        );
        let dir = std::path::PathBuf::from(dir);
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(dir.join(format!("{name}.json")), json);
    }

    async fn stop_supervisor(&mut self, sup: u8, mode: u8, ms: u16) {
        if self.roots.is_empty() {
            return;
        }
        let root = sup as usize % self.roots.len();
        if self.roots[root].done {
            return;
        }
        let Some(s) = self.roots[root].sup.clone() else {
            return;
        };
        let was_closing = std::mem::replace(&mut self.roots[root].closing, true);
        match mode % 3 {
            0 => s.stop(),
            1 => {
                let report = s.shutdown(Duration::from_millis(ms as u64)).await;
                if report.complete {
                    self.root_done(root, &report.children);
                }
            }
            _ => {
                let settled = self.quiesce().await;
                let running = self.running();
                let report = s.shutdown(LONG).await;
                if !report.complete {
                    self.violate(
                        "V9: supervisor shutdown did not complete",
                        format!("r{root}"),
                    );
                    return;
                }
                self.root_done(root, &report.children);
                if settled && self.roots[root].ordered_ok && !was_closing {
                    self.check_tree_order(root, &running);
                }
            }
        }
    }

    /// Mark a system's actors as stoppable: its shutdown began.
    fn system_stopping(&mut self, sys: usize) {
        let ids: Vec<ActorId> = self
            .actors
            .iter()
            .filter(|a| a.sys == Some(sys))
            .map(|a| a.aref.id())
            .collect();
        for id in ids {
            self.may_stop(id);
        }
    }

    async fn shutdown_system(&mut self, sys: u8, mode: u8, ms: u16) {
        if self.systems.is_empty() {
            return;
        }
        let i = sys as usize % self.systems.len();
        let Some(system) = self.systems[i].system.as_ref() else {
            return;
        };
        let _ = system;
        let (settled, running) = if mode % 3 == 2 {
            let settled = self.quiesce().await;
            (settled, self.running())
        } else {
            (false, BTreeSet::new())
        };
        let Some(system) = self.systems[i].system.take() else {
            return;
        };
        self.system_stopping(i);
        let deadline = if mode % 3 == 1 {
            Duration::from_millis(ms as u64)
        } else {
            LONG
        };
        let report = system.shutdown(deadline).await;
        self.systems[i].system = Some(system);
        self.systems[i].shut = true;
        if mode % 3 != 1 && !report.complete {
            self.violate(
                "S2: system shutdown did not complete",
                format!("system {i}"),
            );
        }
        if report.complete {
            self.system_done(i, &report);
            if settled
                && self
                    .roots
                    .iter()
                    .filter(|r| r.sys == i)
                    .all(|r| r.ordered_ok && !r.closing)
            {
                self.check_system_order(i, &running);
            }
        }
    }

    fn system_done(&mut self, sys: usize, report: &ShutdownReport) {
        if !report.coordinator_failures.is_empty() {
            self.violate(
                "S4: coordinator failure without an injected fault",
                report.coordinator_failures.join("; "),
            );
        }
        let actors: Vec<(bool, usize)> = self
            .actors
            .iter()
            .filter(|a| a.sys == Some(sys))
            .map(|a| (a.aref.child_handle().is_alive(), a.instance))
            .collect();
        for (alive, instance) in actors {
            let dropped = self.world.lock().insts[instance].dropped.is_some();
            if alive || !dropped {
                self.violate(
                    "S3: a complete system shutdown left a managed actor running",
                    format!("w{instance} alive={alive} dropped={dropped}"),
                );
            }
        }
        let roots: Vec<usize> = (0..self.roots.len())
            .filter(|&r| self.roots[r].sys == sys && !self.roots[r].done)
            .collect();
        for root in roots {
            let children: Vec<_> = self.roots[root]
                .slots
                .iter()
                .map(|s| match s {
                    Slot::Worker(c) => c.snapshot(),
                    Slot::Nested(n) => n.snapshot(),
                })
                .collect();
            self.root_done(root, &children);
        }
    }

    /// Instances currently running (started, not stopping, not dropped).
    fn running(&self) -> BTreeSet<usize> {
        let s = self.world.lock();
        s.insts
            .iter()
            .enumerate()
            .filter(|(_, i)| {
                i.started_end.is_some() && i.stopped_begin.is_none() && i.dropped.is_none()
            })
            .map(|(n, _)| n)
            .collect()
    }

    fn members(&self, running: &BTreeSet<usize>, prefix: &str) -> Vec<usize> {
        let s = self.world.lock();
        let nested = format!("{prefix}/");
        running
            .iter()
            .copied()
            .filter(|&i| {
                s.insts[i]
                    .slot
                    .as_deref()
                    .is_some_and(|slot| slot == prefix || slot.starts_with(&nested))
            })
            .collect()
    }

    /// Units in start order: each later unit must be fully finished (state
    /// dropped) before any member of an earlier unit begins `stopped()`.
    fn check_order(&self, kind: &'static str, units: &[Vec<usize>]) {
        // A linked member can be stopped by a later unit's exit, out of the
        // coordinator's order (and before the units in between): skip it.
        let linked: std::collections::HashSet<ActorId> = {
            let s = self.world.lock();
            self.links
                .iter()
                .flat_map(|l| [l.a.id(), l.b.id()])
                .chain(s.uncertain.iter().copied())
                .collect()
        };
        let units: Vec<Vec<usize>> = {
            let s = self.world.lock();
            units
                .iter()
                .map(|u| {
                    let linked = u
                        .iter()
                        .any(|&i| s.insts[i].id.is_some_and(|id| linked.contains(&id)));
                    if linked {
                        Vec::new()
                    } else {
                        u.clone()
                    }
                })
                .collect()
        };
        let found = {
            let s = self.world.lock();
            let first_stop =
                |u: &Vec<usize>| u.iter().filter_map(|&i| s.insts[i].stopped_begin).min();
            // None while any member's state is still alive (S3 reports that).
            let last_drop = |u: &Vec<usize>| -> Option<u64> {
                u.iter()
                    .map(|&i| s.insts[i].dropped)
                    .collect::<Option<Vec<u64>>>()
                    .and_then(|d| d.into_iter().max())
            };
            let mut found = None;
            'outer: for i in 0..units.len() {
                for j in i + 1..units.len() {
                    if let (Some(stop), Some(drop)) = (first_stop(&units[i]), last_drop(&units[j]))
                    {
                        if stop < drop {
                            let describe = |u: &Vec<usize>| -> String {
                                u.iter()
                                    .map(|&m| {
                                        let x = &s.insts[m];
                                        format!(
                                            "w{m}({}: stop {:?} drop {:?})",
                                            x.slot.as_deref().unwrap_or("-"),
                                            x.stopped_begin,
                                            x.dropped
                                        )
                                    })
                                    .collect::<Vec<_>>()
                                    .join(" ")
                            };
                            found = Some(format!(
                                "unit {i} [{}] began stopping before unit {j} [{}] finished",
                                describe(&units[i]),
                                describe(&units[j])
                            ));
                            break 'outer;
                        }
                    }
                }
            }
            found
        };
        if let Some(detail) = found {
            self.violate(kind, detail);
        }
    }

    fn check_tree_order(&self, root: usize, running: &BTreeSet<usize>) {
        let units: Vec<Vec<usize>> = self.roots[root]
            .paths
            .iter()
            .map(|(p, _)| self.members(running, p))
            .collect();
        self.check_order(
            "V9: supervisor children did not stop in reverse declaration order",
            &units,
        );
        for (path, count) in &self.roots[root].paths {
            if *count > 0 {
                let units: Vec<Vec<usize>> = (0..*count)
                    .map(|j| self.members(running, &format!("{path}/c{j}")))
                    .collect();
                self.check_order(
                    "V9: supervisor children did not stop in reverse declaration order",
                    &units,
                );
            }
        }
    }

    fn check_system_order(&self, sys: usize, running: &BTreeSet<usize>) {
        let units: Vec<Vec<usize>> = self
            .units
            .iter()
            .filter(|(s, _)| *s == sys)
            .map(|(_, u)| match *u {
                Unit::Actor(a) => {
                    let i = self.actors[a].instance;
                    if running.contains(&i) {
                        vec![i]
                    } else {
                        Vec::new()
                    }
                }
                Unit::Root(r) => self.members(running, &format!("r{r}")),
            })
            .collect();
        self.check_order(
            "S2: system shutdown did not stop targets in reverse admission order",
            &units,
        );
        for root in 0..self.roots.len() {
            if self.roots[root].sys == sys {
                self.check_tree_order(root, running);
            }
        }
    }

    /// Fence one actor: true if it is alive and answered (so it handled
    /// everything queued before).
    async fn fence(&mut self, aref: &ActorRef<Worker>) -> bool {
        if !aref.child_handle().is_alive() {
            return false;
        }
        let nonce = self.next_nonce();
        let msg = Do {
            nonce,
            lane: 0,
            act: Act::Noop,
        };
        match aref
            .request_with_timeout(msg, LONG + self.queued_sleep)
            .await
        {
            Ok(n) => {
                if n != nonce {
                    self.violate(
                        "R2: a reply reached the wrong request",
                        format!("{n} for {nonce}"),
                    );
                }
                true
            }
            Err(ActorError::ActorStopped(_) | ActorError::NoProc) => {
                if rt::timeout(LONG, aref.wait_exit()).await.is_err() {
                    self.violate(
                        "L7: a stopping actor never published its exit",
                        String::new(),
                    );
                }
                false
            }
            Err(_) => {
                self.violate(
                    "liveness: an actor did not answer a fence behind all queued work",
                    String::new(),
                );
                false
            }
        }
    }

    /// Supervisors not mid-transition (no child starting, backing off or
    /// stopping) within 15 s of virtual time.
    async fn settle_supervisors(&mut self) -> bool {
        for _ in 0..300 {
            let mut busy = false;
            for root in &self.roots {
                for slot in &root.slots {
                    let state = match slot {
                        Slot::Worker(c) => c.snapshot().state,
                        Slot::Nested(n) => n.snapshot().state,
                    };
                    busy |= !matches!(
                        state,
                        ChildState::Running | ChildState::Stopped | ChildState::Failed
                    );
                }
            }
            let nested: Vec<SupervisedChild<Worker>> =
                self.world.lock().nested.values().cloned().collect();
            for c in &nested {
                busy |= !matches!(
                    c.snapshot().state,
                    ChildState::Running | ChildState::Stopped | ChildState::Failed
                );
            }
            if !busy {
                return true;
            }
            rt::sleep(Duration::from_millis(50)).await;
        }
        false
    }

    /// Cancel timers, collect detached requests, and fence every actor
    /// until nothing is in flight. False if supervisors did not settle.
    async fn quiesce(&mut self) -> bool {
        trace(|| "quiesce".into());
        for t in self.timers.drain(..) {
            t.cancellation_token.cancel();
        }
        self.collect_pending().await;
        for _ in 0..2 {
            for aref in self.targets() {
                self.fence(&aref).await;
            }
            // Sleep rather than yield: under the canonical schedule (and
            // PCT) a yielding driver can be picked again at once and starve
            // everyone else, while a timer fires only once nothing is ready.
            rt::sleep(Duration::from_millis(1)).await;
            if !self.settle_supervisors().await {
                return false;
            }
        }
        for aref in self.targets() {
            self.fence(&aref).await;
        }
        true
    }

    fn find(&mut self, id: ActorId) -> Option<ActorRef<Worker>> {
        self.targets().into_iter().find(|a| a.id() == id)
    }

    /// Properties that hold once everything settled: Down and Exit delivery
    /// (X1, X3, X5), link propagation, exit reasons (L4) and no actor
    /// stopping without a cause.
    async fn eventual(&mut self) {
        let monitors = self.world.lock().monitors.clone();
        for m in monitors {
            let Some(published) = m.target.exit_reason() else {
                continue;
            };
            let Some(watcher) = self.world.lock().instance_of(m.watcher) else {
                continue;
            };
            let received = |d: &Self| -> Vec<ExitReason> {
                d.world.lock().insts[watcher]
                    .downs
                    .iter()
                    .filter(|(r, _)| *r == m.mref)
                    .map(|(_, reason)| reason.clone())
                    .collect()
            };
            for r in received(self) {
                // 0.8 (X1): a monitor installed after the target's death
                // point gets `NoProc`; one installed before, its reason.
                if r != published && r != ExitReason::NoProc {
                    self.violate(
                        "X1: Down carried a different reason than the target published",
                        format!("{r:?} vs {published:?}"),
                    );
                }
            }
            if m.demonitored || !received(self).is_empty() {
                continue;
            }
            let Some(waref) = self.find(m.watcher) else {
                continue;
            };
            let mut alive = false;
            for _ in 0..5 {
                alive = self.fence(&waref).await;
                if !alive || !received(self).is_empty() {
                    break;
                }
                rt::sleep(Duration::from_millis(10)).await;
            }
            if alive && received(self).is_empty() {
                self.violate(
                    "X1: no Down for an exited target",
                    format!("watcher w{watcher}, outside={}", m.outside),
                );
            }
        }

        let links: Vec<(ActorRef<Worker>, ActorRef<Worker>, bool, u64)> = self
            .links
            .iter()
            .filter(|l| !l.unlinked)
            .flat_map(|l| {
                [
                    (l.a.clone(), l.b.clone(), l.trap_a, l.at),
                    (l.b.clone(), l.a.clone(), l.trap_b, l.at),
                ]
            })
            .collect();
        for (w, p, trap, at) in links {
            let (wid, pid) = (w.id(), p.id());
            if wid == pid {
                continue;
            }
            let pair_unlinked = self.links.iter().any(|l| {
                l.unlinked
                    && ((l.a.id(), l.b.id()) == (wid, pid) || (l.a.id(), l.b.id()) == (pid, wid))
            });
            let calls = self
                .links
                .iter()
                .filter(|l| {
                    (l.a.id(), l.b.id()) == (wid, pid) || (l.a.id(), l.b.id()) == (pid, wid)
                })
                .count();
            let uncertain = {
                let s = self.world.lock();
                s.uncertain.contains(&wid) || s.uncertain.contains(&pid)
            };
            let toggled = self
                .traps
                .get(&wid)
                .is_some_and(|(_, changed)| *changed > at);
            if pair_unlinked || uncertain || toggled {
                continue;
            }
            let Some(published) = p.child_handle().exit_reason() else {
                continue;
            };
            let exits = |d: &Self| -> Vec<ExitReason> {
                let s = d.world.lock();
                s.instance_of(wid)
                    .map(|i| {
                        s.insts[i]
                            .exits
                            .iter()
                            .filter(|(from, _)| *from == pid)
                            .map(|(_, r)| r.clone())
                            .collect()
                    })
                    .unwrap_or_default()
            };
            for r in exits(self) {
                if r != published {
                    self.violate(
                        "X3: Exit carried a different reason than the peer published",
                        format!("{r:?} vs {published:?}"),
                    );
                }
            }
            let mut alive = false;
            for _ in 0..5 {
                alive = self.fence(&w).await;
                if !alive || !trap || !exits(self).is_empty() {
                    break;
                }
                rt::sleep(Duration::from_millis(10)).await;
            }
            if !alive {
                continue;
            }
            if trap {
                let n = exits(self).len();
                if n == 0 {
                    self.violate(
                        "X3: a trapping linked actor never received Exit",
                        String::new(),
                    );
                } else if n > calls {
                    self.violate(
                        "X5: Exit delivered more often than links were made",
                        format!("{n} > {calls}"),
                    );
                }
            } else if published != ExitReason::Normal {
                self.violate(
                    "X3: a non-trapping linked actor survived its peer's abnormal exit",
                    format!("peer {published:?}"),
                );
            }
        }

        let standalone: Vec<(ActorId, usize, Option<ExitReason>)> = self
            .actors
            .iter()
            .map(|a| (a.aref.id(), a.instance, a.aref.child_handle().exit_reason()))
            .collect();
        for (id, instance, exit) in standalone {
            let (may_stop, panicked) = {
                let s = self.world.lock();
                (s.may_stop.contains(&id), s.insts[instance].panicked)
            };
            let Some(exit) = exit else { continue };
            if !may_stop {
                self.violate(
                    "M: an actor stopped without any cause",
                    format!("w{instance} {exit:?}"),
                );
            }
            if panicked && !exit.is_abnormal() {
                self.violate(
                    "L4: an actor that panicked exited with a non-abnormal reason",
                    format!("w{instance} {exit:?}"),
                );
            }
            match self.first_exit.get(&id) {
                Some(first) if *first != exit => self.violate(
                    "L4: a published exit reason changed",
                    format!("w{instance} {first:?} then {exit:?}"),
                ),
                Some(_) => {}
                None => {
                    self.first_exit.insert(id, exit);
                }
            }
        }
    }

    async fn teardown(&mut self) {
        if self.quiesce().await {
            self.eventual().await;
        }
        for root in 0..self.roots.len() {
            if self.roots[root].done {
                continue;
            }
            let Some(sup) = self.roots[root].sup.clone() else {
                continue;
            };
            trace(|| format!("teardown: shutdown r{root}"));
            let report = sup.shutdown(LONG).await;
            if report.complete {
                self.root_done(root, &report.children);
            } else {
                self.violate(
                    "V9: supervisor shutdown did not complete",
                    format!("r{root}"),
                );
            }
        }
        for i in 0..self.systems.len() {
            let Some(system) = self.systems[i].system.take() else {
                continue;
            };
            self.system_stopping(i);
            trace(|| format!("teardown: shutdown system {i}"));
            let report = system.shutdown(LONG).await;
            self.systems[i].shut = true;
            if report.complete {
                self.system_done(i, &report);
            } else {
                self.violate(
                    "S2: system shutdown did not complete",
                    format!("system {i}"),
                );
            }
        }
        for a in &self.actors {
            if a.sys.is_none() {
                a.aref.child_handle().stop();
            }
        }
        let actors: Vec<(ActorRef<Worker>, usize)> = self
            .actors
            .iter()
            .map(|a| (a.aref.clone(), a.instance))
            .collect();
        for (aref, instance) in actors {
            if rt::timeout(LONG, aref.wait_exit()).await.is_err() {
                self.violate(
                    "L3: an actor did not exit after stop",
                    format!("w{instance}"),
                );
            }
        }
        self.collect_pending().await;
        self.world.lock().nested.clear();
    }
}

/// Restart budget (V5) and Temporary children (V1) from the factory log.
fn budget_checks(world: &World) {
    let mut s = world.lock();
    let calls = s.calls.clone();
    let mut found = Vec::new();
    let mut seen: HashMap<(usize, String), usize> = HashMap::new();
    let mut retries: Vec<Vec<Duration>> = vec![Vec::new(); s.sups.len()];
    for c in &calls {
        let n = seen.entry((c.inst, c.slot.clone())).or_default();
        *n += 1;
        if *n > 1 {
            if c.temporary {
                found.push((
                    "V1: a Temporary child was constructed twice",
                    c.slot.clone(),
                ));
            }
            retries[c.inst].push(c.at);
        }
    }
    for (inst, times) in retries.iter().enumerate() {
        let SupInst {
            max_restarts,
            window,
            cohort,
            ..
        } = s.sups[inst].clone();
        // V5 charges restart actions: a OneForOne retry is one, a cohort
        // round is one however many children it rebuilds (at most all of
        // them), so the factory log bounds a cohort supervisor's rebuilds by
        // `max_restarts` times its children.
        let children = calls
            .iter()
            .filter(|c| c.inst == inst)
            .map(|c| &c.slot)
            .collect::<std::collections::HashSet<_>>()
            .len()
            .max(1);
        let bound = if cohort {
            max_restarts * children
        } else {
            max_restarts
        };
        for (i, t) in times.iter().enumerate() {
            let in_window = times[..=i].iter().filter(|u| *t - **u < window).count();
            if in_window > bound {
                found.push((
                    "V5: more restarts than the budget within one window",
                    format!("instance {inst}: {in_window} > {bound} in {window:?}"),
                ));
                break;
            }
        }
    }
    for (kind, detail) in found {
        s.violate(kind, detail);
    }
}

/// Builds a nested subtree generation: records the slot's factory call
/// (parent budget, V3 over the old subtree) and declares fresh children
/// under a new supervisor instance.
#[allow(clippy::too_many_arguments)]
fn nested_configure(
    world: Arc<World>,
    root: usize,
    parent_inst: usize,
    path: String,
    temporary: bool,
    children: Vec<WorkerDef>,
    groups: Vec<ExecutionGroup>,
    cfg: SupervisorConfig,
) -> impl Fn(&mut SupervisorBuilder) -> Result<(), SupervisorError> + Send + Sync + 'static {
    move |builder: &mut SupervisorBuilder| {
        factory_call(&world, parent_inst, &path, temporary);
        let inst = new_sup_inst(&world, root, &cfg);
        for (j, w) in children.iter().enumerate() {
            let p = format!("{path}/c{j}");
            let group = &groups[w.group as usize % groups.len()];
            let child = builder.add(worker_spec(world.clone(), inst, p.clone(), *w, group))?;
            world.lock().nested.insert(p, child);
        }
        Ok(())
    }
}
