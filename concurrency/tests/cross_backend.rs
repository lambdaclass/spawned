//! Cross-backend metamorphic workloads. Each workload is defined once and run
//! on every execution placement; observable outcomes must be identical.
//!
//! Compared across placements: reply values, per-sender message order, exit
//! reasons (including panic text), Down/Exit delivery counts and reasons,
//! timer delivery sets, restart generations and factory calls, and shutdown
//! order. Intentionally allowed to differ: timing, thread identity, ActorId and
//! MonitorRef values (mapped to logical indices before comparison),
//! cross-sender interleaving at one actor, and timer arrival order between
//! distinct deadlines. See docs/TESTING.md.
#![cfg(feature = "async")]
// Tests that need OS threads are compiled out under spawned_sim.
#![cfg_attr(spawned_sim, allow(unused_imports, dead_code))]
#[path = "support/recorder.rs"]
mod recorder;

use proptest::prelude::*;
use recorder::Recorder;
use spawned_concurrency::{
    message::Message,
    supervision::{Backoff, ChildSpec, SupervisedChild, SupervisorConfig, SupervisorStrategy},
    system::{ActorSystem, ExecutionGroup},
    ActorError, ActorId, ChildHandle, Down, Exit, ExitReason, MonitorRef,
};
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

const LIMIT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq)]
enum Obs {
    Down {
        observer: u32,
        monitor: MonitorRef,
        reason: ExitReason,
    },
    Exit {
        observer: u32,
        from: ActorId,
        reason: ExitReason,
    },
    Stopped(u32),
    Tick {
        node: u32,
        n: u32,
    },
    Pulse {
        node: u32,
    },
}
type Log = Arc<Recorder<Obs>>;

#[derive(Debug)]
enum Cmd {
    Echo(u64),
    Chain(u64),
    FanOut(u64),
    Work { sender: u32, seq: u32 },
    Log,
    Watch(ChildHandle),
    Link(ChildHandle),
    Trap,
    Boom,
    Stop,
    Timers { after: u32, pulses: u32 },
}
#[derive(Debug, Clone, PartialEq)]
enum Reply {
    Unit,
    Value(u64),
    Values(Vec<u64>),
    Log(Vec<(u32, u32)>),
    Monitor(MonitorRef),
    Failed,
}
impl Message for Cmd {
    type Result = Reply;
}
#[derive(Debug)]
struct Tick(u32);
impl Message for Tick {
    type Result = ();
}
#[derive(Debug, Clone)]
struct Pulse;
impl Message for Pulse {
    type Result = ();
}

/// Mode-independent node state. Handlers differ only in how they wait.
struct State {
    index: u32,
    log: Log,
    handled: u64,
    work: Vec<(u32, u32)>,
    ticks_wanted: u32,
    ticks_seen: u32,
    pulses_wanted: u32,
    pulses_seen: u32,
}
impl State {
    fn new(index: u32, log: Log) -> Self {
        Self {
            index,
            log,
            handled: 0,
            work: Vec::new(),
            ticks_wanted: 0,
            ticks_seen: 0,
            pulses_wanted: 0,
            pulses_seen: 0,
        }
    }
    fn echo(&mut self, v: u64) -> Reply {
        self.handled += 1;
        Reply::Value(v.wrapping_mul(1_000) + u64::from(self.index) * 10 + self.handled)
    }
    fn leaf(&self, v: u64) -> Reply {
        Reply::Value(v.wrapping_mul(3) + u64::from(self.index))
    }
    fn timers_done(&self) -> bool {
        self.ticks_seen == self.ticks_wanted && self.pulses_seen == self.pulses_wanted
    }
}
fn chain_arg(v: u64, index: u32) -> u64 {
    v.wrapping_mul(2).wrapping_add(u64::from(index) + 1)
}
const TICK_STEP: Duration = Duration::from_millis(1);

mod tasks_node {
    use super::*;
    use spawned_concurrency::tasks::{
        send_after, send_interval, Actor, ActorRef, Context, Handler, TimerHandle,
    };
    pub struct Node {
        pub state: State,
        pub peers: Vec<ActorRef<Node>>,
        pub interval: Option<TimerHandle>,
    }
    impl Actor for Node {
        async fn stopped(&mut self, _: &Context<Self>) {
            self.state.log.push(Obs::Stopped(self.state.index));
        }
        async fn exit_received(&mut self, exit: Exit, _: &Context<Self>) {
            self.state.log.push(Obs::Exit {
                observer: self.state.index,
                from: exit.from,
                reason: exit.reason,
            });
        }
    }
    impl Handler<Cmd> for Node {
        async fn handle(&mut self, cmd: Cmd, ctx: &Context<Self>) -> Reply {
            match cmd {
                Cmd::Echo(v) => self.state.echo(v),
                Cmd::Chain(v) => match self.peers.first() {
                    None => self.state.leaf(v),
                    Some(next) => next
                        .request_with_timeout(Cmd::Chain(chain_arg(v, self.state.index)), LIMIT)
                        .await
                        .unwrap_or(Reply::Failed),
                },
                Cmd::FanOut(v) => {
                    let calls =
                        self.peers.iter().enumerate().map(|(i, peer)| {
                            peer.request_with_timeout(Cmd::Echo(v + i as u64), LIMIT)
                        });
                    let mut values = Vec::new();
                    for reply in futures::future::join_all(calls).await {
                        match reply {
                            Ok(Reply::Value(v)) => values.push(v),
                            _ => return Reply::Failed,
                        }
                    }
                    Reply::Values(values)
                }
                Cmd::Work { sender, seq } => {
                    self.state.work.push((sender, seq));
                    Reply::Unit
                }
                Cmd::Log => Reply::Log(self.state.work.clone()),
                Cmd::Watch(target) => Reply::Monitor(ctx.monitor(&target)),
                Cmd::Link(target) => {
                    ctx.link(&target);
                    Reply::Unit
                }
                Cmd::Trap => {
                    ctx.trap_exit(true);
                    Reply::Unit
                }
                Cmd::Boom => panic!("cross-backend injected panic"),
                Cmd::Stop => {
                    ctx.stop();
                    Reply::Unit
                }
                Cmd::Timers { after, pulses } => {
                    self.state.ticks_wanted = after;
                    self.state.pulses_wanted = pulses;
                    for n in 0..after {
                        send_after(TICK_STEP * (n + 1), ctx.clone(), Tick(n));
                    }
                    if pulses > 0 {
                        self.interval = Some(send_interval(TICK_STEP, ctx.clone(), Pulse));
                    }
                    if self.state.timers_done() {
                        ctx.stop();
                    }
                    Reply::Unit
                }
            }
        }
    }
    impl Handler<Down> for Node {
        async fn handle(&mut self, down: Down, _: &Context<Self>) {
            self.state.log.push(Obs::Down {
                observer: self.state.index,
                monitor: down.monitor_ref,
                reason: down.reason,
            });
        }
    }
    impl Handler<Tick> for Node {
        async fn handle(&mut self, Tick(n): Tick, ctx: &Context<Self>) {
            self.state.ticks_seen += 1;
            self.state.log.push(Obs::Tick {
                node: self.state.index,
                n,
            });
            if self.state.timers_done() {
                ctx.stop();
            }
        }
    }
    impl Handler<Pulse> for Node {
        async fn handle(&mut self, _: Pulse, ctx: &Context<Self>) {
            if self.state.pulses_seen == self.state.pulses_wanted {
                return; // A tick already in flight when the interval was cancelled.
            }
            self.state.pulses_seen += 1;
            self.state.log.push(Obs::Pulse {
                node: self.state.index,
            });
            if self.state.pulses_seen == self.state.pulses_wanted {
                if let Some(timer) = self.interval.take() {
                    timer.cancellation_token.cancel();
                }
            }
            if self.state.timers_done() {
                ctx.stop();
            }
        }
    }
}

mod threads_node {
    use super::*;
    use spawned_concurrency::threads::{
        send_after, send_interval, Actor, ActorRef, Context, Handler, TimerHandle,
    };
    pub struct Node {
        pub state: State,
        pub peers: Vec<ActorRef<Node>>,
        pub interval: Option<TimerHandle>,
    }
    impl Actor for Node {
        fn stopped(&mut self, _: &Context<Self>) {
            self.state.log.push(Obs::Stopped(self.state.index));
        }
        fn exit_received(&mut self, exit: Exit, _: &Context<Self>) {
            self.state.log.push(Obs::Exit {
                observer: self.state.index,
                from: exit.from,
                reason: exit.reason,
            });
        }
    }
    impl Handler<Cmd> for Node {
        fn handle(&mut self, cmd: Cmd, ctx: &Context<Self>) -> Reply {
            match cmd {
                Cmd::Echo(v) => self.state.echo(v),
                Cmd::Chain(v) => match self.peers.first() {
                    None => self.state.leaf(v),
                    Some(next) => next
                        .request_with_timeout(Cmd::Chain(chain_arg(v, self.state.index)), LIMIT)
                        .unwrap_or(Reply::Failed),
                },
                Cmd::FanOut(v) => {
                    // Fan out every request before collecting any reply.
                    let pending: Vec<_> = self
                        .peers
                        .iter()
                        .enumerate()
                        .map(|(i, peer)| peer.request_raw(Cmd::Echo(v + i as u64)))
                        .collect();
                    let mut values = Vec::new();
                    for rx in pending {
                        match rx.map(|rx| rx.recv_timeout(LIMIT)) {
                            Ok(Ok(Reply::Value(v))) => values.push(v),
                            _ => return Reply::Failed,
                        }
                    }
                    Reply::Values(values)
                }
                Cmd::Work { sender, seq } => {
                    self.state.work.push((sender, seq));
                    Reply::Unit
                }
                Cmd::Log => Reply::Log(self.state.work.clone()),
                Cmd::Watch(target) => Reply::Monitor(ctx.monitor(&target)),
                Cmd::Link(target) => {
                    ctx.link(&target);
                    Reply::Unit
                }
                Cmd::Trap => {
                    ctx.trap_exit(true);
                    Reply::Unit
                }
                Cmd::Boom => panic!("cross-backend injected panic"),
                Cmd::Stop => {
                    ctx.stop();
                    Reply::Unit
                }
                Cmd::Timers { after, pulses } => {
                    self.state.ticks_wanted = after;
                    self.state.pulses_wanted = pulses;
                    for n in 0..after {
                        send_after(TICK_STEP * (n + 1), ctx.clone(), Tick(n));
                    }
                    if pulses > 0 {
                        self.interval = Some(send_interval(TICK_STEP, ctx.clone(), Pulse));
                    }
                    if self.state.timers_done() {
                        ctx.stop();
                    }
                    Reply::Unit
                }
            }
        }
    }
    impl Handler<Down> for Node {
        fn handle(&mut self, down: Down, _: &Context<Self>) {
            self.state.log.push(Obs::Down {
                observer: self.state.index,
                monitor: down.monitor_ref,
                reason: down.reason,
            });
        }
    }
    impl Handler<Tick> for Node {
        fn handle(&mut self, Tick(n): Tick, ctx: &Context<Self>) {
            self.state.ticks_seen += 1;
            self.state.log.push(Obs::Tick {
                node: self.state.index,
                n,
            });
            if self.state.timers_done() {
                ctx.stop();
            }
        }
    }
    impl Handler<Pulse> for Node {
        fn handle(&mut self, _: Pulse, ctx: &Context<Self>) {
            if self.state.pulses_seen == self.state.pulses_wanted {
                return;
            }
            self.state.pulses_seen += 1;
            self.state.log.push(Obs::Pulse {
                node: self.state.index,
            });
            if self.state.pulses_seen == self.state.pulses_wanted {
                if let Some(timer) = self.interval.take() {
                    timer.cancellation_token.cancel();
                }
            }
            if self.state.timers_done() {
                ctx.stop();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Placements
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Placement {
    TasksAsync,
    TasksBlocking,
    TasksThread,
    Threads,
    GroupAsync,
    GroupThread,
    /// Linux only: hard CPU affinity is unsupported elsewhere.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    GroupPinned,
}
impl Placement {
    fn managed(self) -> bool {
        matches!(
            self,
            Self::GroupAsync | Self::GroupThread | Self::GroupPinned
        )
    }
}
/// Under `--cfg spawned_sim` only the async placements exist: the others
/// need OS threads, which the single-thread simulator cannot schedule.
#[cfg(spawned_sim)]
fn all_placements() -> Vec<Placement> {
    vec![Placement::TasksAsync, Placement::GroupAsync]
}
#[cfg(spawned_sim)]
fn group_placements() -> Vec<Placement> {
    vec![Placement::GroupAsync]
}
#[cfg(not(spawned_sim))]
fn all_placements() -> Vec<Placement> {
    let mut all = vec![
        Placement::TasksAsync,
        Placement::TasksBlocking,
        Placement::TasksThread,
        Placement::Threads,
    ];
    all.extend(group_placements());
    all
}
#[cfg(not(spawned_sim))]
fn group_placements() -> Vec<Placement> {
    #[allow(unused_mut)]
    let mut groups = vec![Placement::GroupAsync, Placement::GroupThread];
    #[cfg(target_os = "linux")]
    groups.push(Placement::GroupPinned);
    groups
}

#[derive(Clone)]
enum Ref {
    Tasks(
        spawned_concurrency::tasks::ActorRef<tasks_node::Node>,
        spawned_rt::tasks::Handle,
    ),
    Threads(spawned_concurrency::threads::ActorRef<threads_node::Node>),
}
impl Ref {
    fn send(&self, cmd: Cmd) -> Result<(), ActorError> {
        match self {
            Self::Tasks(a, _) => a.send(cmd),
            Self::Threads(a) => a.send(cmd),
        }
    }
    fn request(&self, cmd: Cmd) -> Result<Reply, ActorError> {
        match self {
            Self::Tasks(a, rt) => rt.block_on(a.request_with_timeout(cmd, LIMIT)),
            Self::Threads(a) => a.request_with_timeout(cmd, LIMIT),
        }
    }
    fn child(&self) -> ChildHandle {
        match self {
            Self::Tasks(a, _) => a.child_handle(),
            Self::Threads(a) => a.child_handle(),
        }
    }
}

struct Env {
    placement: Placement,
    rt: Option<spawned_rt::tasks::Runtime>,
    system: Option<ActorSystem>,
    group: Option<ExecutionGroup>,
    started: Vec<ChildHandle>,
}
impl Env {
    /// `seed` picks the simulated schedule under `--cfg spawned_sim`; Tokio
    /// ignores it.
    fn new(placement: Placement, #[allow(unused_variables)] seed: u64) -> Self {
        #[cfg(not(spawned_sim))]
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        #[cfg(spawned_sim)]
        let rt = spawned_rt::tasks::Runtime::with_seed(seed);
        let (system, group) = if placement.managed() {
            let system = ActorSystem::new().unwrap();
            let group = match placement {
                Placement::GroupAsync => system.async_group("xb-async", 2),
                Placement::GroupThread => system.thread_group("xb-thread", 64),
                _ => {
                    let cpus = spawned_rt::tasks::affinity::available_cpus().unwrap();
                    system.pinned_group("xb-pinned", cpus.into_iter().take(2).collect())
                }
            }
            .unwrap();
            (Some(system), Some(group))
        } else {
            (None, None)
        };
        Self {
            placement,
            rt: Some(rt),
            system,
            group,
            started: Vec::new(),
        }
    }
    fn handle(&self) -> spawned_rt::tasks::Handle {
        self.rt.as_ref().unwrap().handle().clone()
    }
    fn block_on<F: std::future::Future>(&self, future: F) -> F::Output {
        self.rt.as_ref().unwrap().block_on(future)
    }
    fn bounded<F: std::future::Future>(&self, future: F) -> Result<F::Output, String> {
        self.block_on(async move { spawned_rt::tasks::timeout(LIMIT, future).await })
            .map_err(|elapsed| elapsed.to_string())
    }
    fn start(&mut self, index: u32, peers: Vec<Ref>, log: &Log) -> Ref {
        use spawned_concurrency::tasks::{ActorStart as _, Backend};
        let state = State::new(index, log.clone());
        let handle = self.handle();
        let started = if self.placement == Placement::Threads {
            use spawned_concurrency::threads::ActorStart as _;
            let peers = peers
                .into_iter()
                .map(|p| match p {
                    Ref::Threads(a) => a,
                    Ref::Tasks(..) => unreachable!("mixed modes"),
                })
                .collect();
            Ref::Threads(
                threads_node::Node {
                    state,
                    peers,
                    interval: None,
                }
                .start(),
            )
        } else {
            let peers = peers
                .into_iter()
                .map(|p| match p {
                    Ref::Tasks(a, _) => a,
                    Ref::Threads(_) => unreachable!("mixed modes"),
                })
                .collect();
            let node = tasks_node::Node {
                state,
                peers,
                interval: None,
            };
            let _entered = handle.enter();
            let actor = match self.placement {
                Placement::TasksAsync => node.start_with_backend(Backend::Async),
                Placement::TasksBlocking => node.start_with_backend(Backend::Blocking),
                Placement::TasksThread => node.start_with_backend(Backend::Thread),
                _ => {
                    let actor = self.group.as_ref().unwrap().start(node).unwrap();
                    self.bounded(actor.ready())
                        .expect("managed actor readiness timed out")
                        .unwrap();
                    actor
                }
            };
            Ref::Tasks(actor, handle)
        };
        self.started.push(started.child());
        started
    }
    fn wait_exit(&self, child: &ChildHandle) -> ExitReason {
        self.bounded(child.wait_exit_async())
            .unwrap_or_else(|_| panic!("{:?}: actor did not exit", self.placement))
    }
    /// Stop everything and require a bounded, complete shutdown.
    fn finish(mut self) {
        for child in std::mem::take(&mut self.started) {
            child.stop();
            self.wait_exit(&child);
        }
        if let Some(system) = self.system.take() {
            let report = self.block_on(system.shutdown(LIMIT));
            assert!(report.complete, "{:?}: system shutdown", self.placement);
        }
        #[cfg(not(spawned_sim))]
        self.rt.take().unwrap().shutdown_timeout(LIMIT);
    }
}
impl Drop for Env {
    fn drop(&mut self) {
        // Assertion failures still stop actors, so a failing case cannot wedge.
        for child in &self.started {
            child.stop();
        }
        #[cfg(not(spawned_sim))]
        if let Some(rt) = self.rt.take() {
            rt.shutdown_background();
        }
    }
}

/// Run `workload` on every placement and require identical outcomes.
fn metamorphic<T: PartialEq + std::fmt::Debug>(
    placements: Vec<Placement>,
    mut workload: impl FnMut(&mut Env) -> T,
) -> T {
    let mut baseline: Option<(Placement, u64, T)> = None;
    for placement in placements {
        for seed in seeds() {
            let mut env = Env::new(placement, seed);
            let outcome = workload(&mut env);
            env.finish();
            match &baseline {
                None => baseline = Some((placement, seed, outcome)),
                Some((first, first_seed, expected)) => assert_eq!(
                    &outcome, expected,
                    "{placement:?} (seed {seed}) diverged from {first:?} (seed {first_seed})"
                ),
            }
        }
    }
    baseline.unwrap().2
}

/// One run per placement on Tokio. Under `--cfg spawned_sim`, where only two
/// async placements exist, each also runs over simulated schedules
/// `0..SPAWNED_SIM_SEEDS` (default 4), so the comparison covers schedules
/// as well as placements.
fn seeds() -> std::ops::Range<u64> {
    if cfg!(spawned_sim) {
        0..std::env::var("SPAWNED_SIM_SEEDS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(4)
    } else {
        0..1
    }
}

// ---------------------------------------------------------------------------
// Workloads
// ---------------------------------------------------------------------------

fn chain_workload(env: &mut Env, len: u32, values: &[u64]) -> Vec<Reply> {
    let log = Recorder::new();
    let mut next: Option<Ref> = None;
    for index in (0..len).rev() {
        next = Some(env.start(index, next.into_iter().collect(), &log));
    }
    let head = next.unwrap();
    values
        .iter()
        .map(|&v| head.request(Cmd::Chain(v)).unwrap())
        .collect()
}
fn chain_oracle(len: u32, v: u64) -> Reply {
    let mut v = v;
    for index in 0..len - 1 {
        v = chain_arg(v, index);
    }
    Reply::Value(v.wrapping_mul(3) + u64::from(len - 1))
}

#[derive(Debug, PartialEq)]
struct FanOutcome {
    fanned: Vec<Reply>,
    per_sender: BTreeMap<u32, Vec<u32>>,
    total: usize,
}
fn fan_workload(env: &mut Env, leaves: u32, values: &[u64], producers: &[u8]) -> FanOutcome {
    let log = Recorder::new();
    let peers: Vec<_> = (1..=leaves).map(|i| env.start(i, vec![], &log)).collect();
    let hub = env.start(0, peers, &log);
    let fanned = values
        .iter()
        .map(|&v| hub.request(Cmd::FanOut(v)).unwrap())
        .collect();
    // Fan-in: concurrent producers, a mix of sends and request fences.
    std::thread::scope(|scope| {
        for (sender, &count) in producers.iter().enumerate() {
            let hub = hub.clone();
            scope.spawn(move || {
                for seq in 0..u32::from(count) {
                    let work = Cmd::Work {
                        sender: sender as u32,
                        seq,
                    };
                    if seq % 3 == 2 {
                        assert_eq!(hub.request(work).unwrap(), Reply::Unit);
                    } else {
                        hub.send(work).unwrap();
                    }
                }
            });
        }
    });
    let Reply::Log(work) = hub.request(Cmd::Log).unwrap() else {
        panic!("wrong reply")
    };
    let mut per_sender: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    for (sender, seq) in &work {
        per_sender.entry(*sender).or_default().push(*seq);
    }
    FanOutcome {
        fanned,
        per_sender,
        total: work.len(),
    }
}

#[derive(Debug, Clone, Copy)]
enum Ending {
    Stop,
    Panic,
    Shutdown,
}
fn end(env: &Env, actor: &Ref, ending: Ending) -> ExitReason {
    let child = actor.child();
    match ending {
        Ending::Stop => assert_eq!(actor.request(Cmd::Stop).unwrap(), Reply::Unit),
        Ending::Panic => assert!(matches!(
            actor.request(Cmd::Boom),
            Err(ActorError::ActorStopped)
        )),
        Ending::Shutdown => child.shutdown(),
    }
    env.wait_exit(&child)
}
fn expected_reason(ending: Ending) -> ExitReason {
    match ending {
        Ending::Stop => ExitReason::Normal,
        Ending::Shutdown => ExitReason::Shutdown,
        Ending::Panic => ExitReason::Panic("panic in handler: cross-backend injected panic".into()),
    }
}

#[derive(Debug, PartialEq)]
struct LinkOutcome {
    child_reasons: Vec<ExitReason>,
    /// (child index, reason) per trapped Exit, sorted by child index.
    trapped: Vec<(usize, ExitReason)>,
    bystander: ExitReason,
}
fn link_workload(env: &mut Env, endings: &[Ending]) -> LinkOutcome {
    let log = Recorder::new();
    let trapper = env.start(0, vec![], &log);
    let bystander = env.start(1, vec![], &log);
    assert_eq!(trapper.request(Cmd::Trap).unwrap(), Reply::Unit);
    let children: Vec<_> = (0..endings.len())
        .map(|i| env.start(10 + i as u32, vec![], &log))
        .collect();
    for child in &children {
        assert_eq!(
            trapper.request(Cmd::Link(child.child())).unwrap(),
            Reply::Unit
        );
    }
    assert_eq!(
        bystander.request(Cmd::Link(children[0].child())).unwrap(),
        Reply::Unit
    );
    let ids: Vec<ActorId> = children.iter().map(|c| c.child().id()).collect();
    let child_reasons: Vec<_> = children
        .iter()
        .zip(endings)
        .map(|(child, &ending)| end(env, child, ending))
        .collect();
    // Exit envelopes are enqueued before the child's completion is published,
    // so a later request to the trapper is a delivery fence.
    let _ = trapper.request(Cmd::Echo(0)).unwrap();
    let mut trapped: Vec<_> = log
        .snapshot()
        .into_iter()
        .filter_map(|obs| match obs {
            Obs::Exit {
                observer: 0,
                from,
                reason,
            } => Some((ids.iter().position(|id| *id == from).unwrap(), reason)),
            _ => None,
        })
        .collect();
    trapped.sort_by_key(|(i, _)| *i);
    let bystander_child = bystander.child();
    let bystander_reason = if child_reasons[0] != ExitReason::Normal {
        env.wait_exit(&bystander_child)
    } else {
        // Only a Normal exit is ignored by an untrapped link (Erlang semantics).
        assert_eq!(bystander.request(Cmd::Echo(0)).unwrap(), Reply::Value(11));
        bystander_child.stop();
        env.wait_exit(&bystander_child)
    };
    trapper.child().stop();
    assert_eq!(env.wait_exit(&trapper.child()), ExitReason::Normal);
    let late = log
        .snapshot()
        .iter()
        .filter(|o| matches!(o, Obs::Exit { observer: 0, .. }))
        .count();
    assert_eq!(late, trapped.len(), "duplicate or late trapped Exit");
    LinkOutcome {
        child_reasons,
        trapped,
        bystander: bystander_reason,
    }
}

#[derive(Debug, PartialEq)]
struct MonitorOutcome {
    /// Down reasons per watched target, in watch order (last one watched after death).
    downs: Vec<Vec<ExitReason>>,
}
fn monitor_workload(env: &mut Env, endings: &[Ending], double: &[bool]) -> MonitorOutcome {
    let log = Recorder::new();
    let observer = env.start(0, vec![], &log);
    let targets: Vec<_> = (0..endings.len())
        .map(|i| env.start(10 + i as u32, vec![], &log))
        .collect();
    let watch = |target: &Ref| match observer.request(Cmd::Watch(target.child())).unwrap() {
        Reply::Monitor(id) => id,
        other => panic!("wrong reply {other:?}"),
    };
    let mut monitors: Vec<Vec<MonitorRef>> = targets
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let mut ids = vec![watch(t)];
            if double[i % double.len()] {
                ids.push(watch(t)); // Independent monitors on one target.
            }
            ids
        })
        .collect();
    for (target, &ending) in targets.iter().zip(endings) {
        end(env, target, ending);
    }
    // Watching an already-dead target delivers Down immediately.
    monitors.push(vec![watch(&targets[0])]);
    let expected: usize = monitors.iter().map(Vec::len).sum();
    let count = |events: &[Obs]| {
        events
            .iter()
            .filter(|o| matches!(o, Obs::Down { .. }))
            .count()
    };
    log.wait_until(LIMIT, "Down notifications", |e| count(e) >= expected);
    observer.child().stop();
    env.wait_exit(&observer.child());
    let events = log.snapshot();
    assert_eq!(count(&events), expected, "duplicate Down");
    let downs = monitors
        .iter()
        .map(|ids| {
            ids.iter()
                .map(|id| {
                    let mut found = events.iter().filter_map(|o| match o {
                        Obs::Down {
                            monitor, reason, ..
                        } if monitor == id => Some(reason.clone()),
                        _ => None,
                    });
                    let reason = found.next().expect("missing Down");
                    assert!(found.next().is_none(), "duplicate Down for {id}");
                    reason
                })
                .collect()
        })
        .collect();
    MonitorOutcome { downs }
}

#[derive(Debug, PartialEq)]
struct TimerOutcome {
    ticks: Vec<u32>,
    pulses: usize,
    reason: ExitReason,
}
fn timer_workload(env: &mut Env, after: u32, pulses: u32) -> TimerOutcome {
    let log = Recorder::new();
    let node = env.start(0, vec![], &log);
    assert_eq!(
        node.request(Cmd::Timers { after, pulses }).unwrap(),
        Reply::Unit
    );
    // The node stops itself once every expected timer message arrived.
    let reason = env.wait_exit(&node.child());
    let events = log.snapshot();
    let mut ticks: Vec<u32> = events
        .iter()
        .filter_map(|o| match o {
            Obs::Tick { n, .. } => Some(*n),
            _ => None,
        })
        .collect();
    ticks.sort_unstable(); // Arrival order between distinct deadlines may differ.
    TimerOutcome {
        ticks,
        pulses: events
            .iter()
            .filter(|o| matches!(o, Obs::Pulse { .. }))
            .count(),
        reason,
    }
}

#[derive(Debug, PartialEq)]
struct SupervisorOutcome {
    generations: Vec<u64>,
    factory_calls: Vec<usize>,
    /// Echo replies from each current generation (fresh state shows in the value).
    replies: Vec<Reply>,
    shutdown_order: Vec<u32>,
    complete: bool,
}
fn affected(strategy: SupervisorStrategy, n: usize, crashed: usize) -> std::ops::Range<usize> {
    match strategy {
        SupervisorStrategy::OneForOne => crashed..crashed + 1,
        SupervisorStrategy::OneForAll => 0..n,
        SupervisorStrategy::RestForOne => crashed..n,
    }
}
fn supervisor_config(strategy: SupervisorStrategy) -> SupervisorConfig {
    SupervisorConfig {
        strategy,
        max_restarts: 10_000,
        restart_window: Duration::from_secs(60),
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
fn supervisor_workload(
    env: &mut Env,
    strategy: SupervisorStrategy,
    n: usize,
    crashes: &[u8],
) -> SupervisorOutcome {
    let log: Log = Recorder::new();
    let calls: Arc<Vec<AtomicUsize>> = Arc::new((0..n).map(|_| AtomicUsize::new(0)).collect());
    let group = env.group.clone().unwrap();
    let mut tree = env
        .system
        .as_ref()
        .unwrap()
        .supervisor(supervisor_config(strategy))
        .unwrap();
    let children: Vec<SupervisedChild<tasks_node::Node>> = (0..n)
        .map(|i| {
            let calls = calls.clone();
            let log = log.clone();
            tree.add(ChildSpec::new(format!("c{i}"), &group, move |_| {
                calls[i].fetch_add(1, Ordering::SeqCst);
                let state = State::new(i as u32, log.clone());
                async move {
                    Ok::<_, String>(tasks_node::Node {
                        state,
                        peers: vec![],
                        interval: None,
                    })
                }
            }))
            .unwrap()
        })
        .collect();
    let supervisor = env.bounded(tree.start()).unwrap().unwrap();
    let generation = |k: usize| children[k].snapshot().generation;
    for &crash in crashes {
        let crashed = crash as usize % n;
        let before: Vec<u64> = (0..n).map(generation).collect();
        let current = children[crashed].current().unwrap();
        // Reply-before-panic would be a bug: the handler panics before replying.
        assert!(env
            .block_on(current.request_with_timeout(Cmd::Boom, LIMIT))
            .is_err());
        let hit = affected(strategy, n, crashed);
        for k in hit.clone() {
            env.bounded(children[k].wait_for_generation(before[k]))
                .unwrap_or_else(|_| panic!("{:?}: child {k} not rebuilt", env.placement))
                .unwrap();
        }
        for k in (0..n).filter(|k| !hit.contains(k)) {
            assert_eq!(generation(k), before[k], "unaffected child {k} restarted");
        }
    }
    let replies = children
        .iter()
        .map(|child| {
            env.block_on(
                child
                    .current()
                    .unwrap()
                    .request_with_timeout(Cmd::Echo(5), LIMIT),
            )
            .unwrap()
        })
        .collect();
    let generations = (0..n).map(generation).collect();
    let stops_before = log.snapshot().len();
    let report = env.block_on(supervisor.shutdown(LIMIT));
    let shutdown_order = log.snapshot()[stops_before..]
        .iter()
        .filter_map(|o| match o {
            Obs::Stopped(i) => Some(*i),
            _ => None,
        })
        .collect();
    SupervisorOutcome {
        generations,
        factory_calls: calls.iter().map(|c| c.load(Ordering::SeqCst)).collect(),
        replies,
        shutdown_order,
        complete: report.complete,
    }
}

#[derive(Debug, PartialEq)]
struct ShutdownOutcome {
    order: Vec<u32>,
    exits: Vec<ExitReason>,
    complete: bool,
}
fn shutdown_workload(env: &mut Env, before: u32, sup_children: u32, after: u32) -> ShutdownOutcome {
    let log = Recorder::new();
    let mut ids = Vec::new();
    for i in 0..before {
        ids.push(env.start(i, vec![], &log).child().id());
    }
    let group = env.group.clone().unwrap();
    let system = env.system.take().unwrap();
    let mut tree = system
        .supervisor(supervisor_config(SupervisorStrategy::OneForOne))
        .unwrap();
    for i in 0..sup_children {
        let log = log.clone();
        tree.add(ChildSpec::new(format!("s{i}"), &group, move |_| {
            let state = State::new(100 + i, log.clone());
            async move {
                Ok::<_, String>(tasks_node::Node {
                    state,
                    peers: vec![],
                    interval: None,
                })
            }
        }))
        .unwrap();
    }
    let supervisor = (sup_children > 0).then(|| env.bounded(tree.start()).unwrap().unwrap());
    for i in 0..after {
        ids.push(env.start(200 + i, vec![], &log).child().id());
    }
    // ActorSystem::shutdown owns ordering; Env::finish must not stop actors first.
    env.started.clear();
    let report = env.block_on(system.shutdown(LIMIT));
    drop(supervisor);
    let order = log
        .snapshot()
        .into_iter()
        .filter_map(|o| match o {
            Obs::Stopped(i) => Some(i),
            _ => None,
        })
        .collect();
    let exits = ids
        .iter()
        .map(|id| {
            report
                .exits
                .iter()
                .find(|e| e.id == *id)
                .map(|e| e.reason.clone())
                .expect("standalone actor missing from shutdown report")
        })
        .collect();
    ShutdownOutcome {
        order,
        exits,
        complete: report.complete,
    }
}

// ---------------------------------------------------------------------------
// Properties
// ---------------------------------------------------------------------------

fn config() -> ProptestConfig {
    let mut config = ProptestConfig {
        failure_persistence: Some(Box::new(
            proptest::test_runner::FileFailurePersistence::WithSource("proptest-regressions"),
        )),
        ..ProptestConfig::default()
    };
    if std::env::var_os("PROPTEST_CASES").is_none() {
        config.cases = 6;
    }
    config
}
fn ending() -> impl Strategy<Value = Ending> {
    prop_oneof![
        Just(Ending::Stop),
        Just(Ending::Panic),
        Just(Ending::Shutdown)
    ]
}
fn strategy() -> impl Strategy<Value = SupervisorStrategy> {
    prop_oneof![
        Just(SupervisorStrategy::OneForOne),
        Just(SupervisorStrategy::OneForAll),
        Just(SupervisorStrategy::RestForOne)
    ]
}

proptest! {
    #![proptest_config(config())]

    #[test]
    fn request_reply_chains_match(len in 1u32..5, values in prop::collection::vec(any::<u32>(), 1..6)) {
        let values: Vec<u64> = values.into_iter().map(u64::from).collect();
        let replies = metamorphic(all_placements(), |env| chain_workload(env, len, &values));
        let expected: Vec<_> = values.iter().map(|&v| chain_oracle(len, v)).collect();
        prop_assert_eq!(replies, expected);
    }

    // Not simulated: compares Tokio, Thread, Blocking and native placements.
    #[cfg(not(spawned_sim))]
    #[test]
    fn fan_out_fan_in_match(
        leaves in 1u32..5,
        values in prop::collection::vec(0u64..1000, 1..4),
        producers in prop::collection::vec(0u8..24, 1..4),
    ) {
        let outcome = metamorphic(all_placements(), |env| fan_workload(env, leaves, &values, &producers));
        prop_assert_eq!(outcome.total, producers.iter().map(|&c| c as usize).sum::<usize>());
        for (sender, &count) in producers.iter().enumerate() {
            let seqs = outcome.per_sender.get(&(sender as u32)).cloned().unwrap_or_default();
            prop_assert_eq!(seqs, (0..u32::from(count)).collect::<Vec<_>>());
        }
    }

    #[test]
    fn links_and_trapped_exits_match(endings in prop::collection::vec(ending(), 1..5)) {
        let outcome = metamorphic(all_placements(), |env| link_workload(env, &endings));
        let expected: Vec<_> = endings.iter().map(|&e| expected_reason(e)).collect();
        prop_assert_eq!(&outcome.child_reasons, &expected);
        // Trapping observers receive every linked exit, normal ones included.
        prop_assert_eq!(
            outcome.trapped,
            expected.iter().cloned().enumerate().collect::<Vec<_>>()
        );
        let bystander = if expected[0] != ExitReason::Normal { expected[0].clone() } else { ExitReason::Normal };
        prop_assert_eq!(outcome.bystander, bystander);
    }

    // Not simulated: the monitor workload blocks the test thread on a Condvar.
    #[cfg(not(spawned_sim))]
    #[test]
    fn monitors_and_down_match(
        endings in prop::collection::vec(ending(), 1..5),
        double in prop::collection::vec(any::<bool>(), 1..5),
    ) {
        let outcome = metamorphic(all_placements(), |env| monitor_workload(env, &endings, &double));
        for (i, &ending) in endings.iter().enumerate() {
            prop_assert!(outcome.downs[i].iter().all(|r| *r == expected_reason(ending)));
        }
        prop_assert_eq!(&outcome.downs[endings.len()], &vec![expected_reason(endings[0])]);
    }

    #[test]
    fn timers_match(after in 0u32..6, pulses in 0u32..6) {
        let outcome = metamorphic(all_placements(), |env| timer_workload(env, after, pulses));
        prop_assert_eq!(outcome, TimerOutcome {
            ticks: (0..after).collect(),
            pulses: pulses as usize,
            reason: ExitReason::Normal,
        });
    }

    #[test]
    fn supervisor_restarts_match(
        strategy in strategy(),
        n in 1usize..4,
        crashes in prop::collection::vec(any::<u8>(), 0..5),
    ) {
        let outcome = metamorphic(group_placements(), |env| supervisor_workload(env, strategy, n, &crashes));
        // The initial instance is generation 1; each reconstruction adds one.
        let mut generations = vec![1u64; n];
        for &crash in &crashes {
            for k in affected(strategy, n, crash as usize % n) {
                generations[k] += 1;
            }
        }
        prop_assert_eq!(&outcome.generations, &generations);
        prop_assert_eq!(
            &outcome.factory_calls,
            &generations.iter().map(|g| *g as usize).collect::<Vec<_>>()
        );
        // Every generation starts from fresh state: first Echo after construction.
        let fresh: Vec<_> = (0..n).map(|i| Reply::Value(5_000 + i as u64 * 10 + 1)).collect();
        prop_assert_eq!(&outcome.replies, &fresh);
        prop_assert_eq!(&outcome.shutdown_order, &(0..n as u32).rev().collect::<Vec<_>>());
        prop_assert!(outcome.complete);
    }

    #[test]
    fn ordered_system_shutdown_matches(before in 0u32..4, sup_children in 0u32..4, after in 0u32..4) {
        let outcome = metamorphic(group_placements(), |env| shutdown_workload(env, before, sup_children, after));
        // Reverse admission order across standalone actors and supervisor trees.
        let mut order: Vec<u32> = (200..200 + after).rev().collect();
        order.extend((100..100 + sup_children).rev());
        order.extend((0..before).rev());
        prop_assert_eq!(&outcome.order, &order);
        prop_assert!(outcome.exits.iter().all(|r| *r == ExitReason::Shutdown));
        prop_assert!(outcome.complete);
    }
}

/// Fixed regression covering every workload family once on every placement.
#[test]
fn fixed_workloads_match_across_placements() {
    let all = all_placements();
    #[cfg(not(spawned_sim))]
    assert!(all.len() >= 6, "expected every non-pinned placement");
    metamorphic(all.clone(), |env| chain_workload(env, 3, &[1, 2, 3]));
    // Not simulated: fan-out drives requests from OS threads, and the
    // monitor workload blocks the test thread on a Condvar.
    #[cfg(not(spawned_sim))]
    metamorphic(all.clone(), |env| fan_workload(env, 3, &[7], &[10, 10]));
    metamorphic(all.clone(), |env| {
        link_workload(env, &[Ending::Panic, Ending::Stop, Ending::Shutdown])
    });
    #[cfg(not(spawned_sim))]
    metamorphic(all.clone(), |env| {
        monitor_workload(env, &[Ending::Shutdown, Ending::Panic], &[true])
    });
    metamorphic(all, |env| timer_workload(env, 3, 2));
    for strategy in [
        SupervisorStrategy::OneForOne,
        SupervisorStrategy::OneForAll,
        SupervisorStrategy::RestForOne,
    ] {
        metamorphic(group_placements(), |env| {
            supervisor_workload(env, strategy, 3, &[1, 0, 2])
        });
    }
    metamorphic(group_placements(), |env| shutdown_workload(env, 2, 2, 2));
}

// ---------------------------------------------------------------------------
// Contract: stop before the first poll (managed versus legacy)
// ---------------------------------------------------------------------------

mod early_stop {
    use super::*;
    use std::sync::{mpsc, Mutex};

    #[derive(Debug)]
    pub struct Block(pub Mutex<mpsc::Receiver<()>>, pub mpsc::Sender<()>);
    impl Message for Block {
        type Result = ();
    }
    pub struct Hooks(pub Arc<Recorder<&'static str>>);
    impl Drop for Hooks {
        fn drop(&mut self) {
            self.0.push("dropped");
        }
    }
    impl spawned_concurrency::tasks::Actor for Hooks {
        async fn started(&mut self, _: &spawned_concurrency::tasks::Context<Self>) {
            self.0.push("started");
        }
        async fn stopped(&mut self, _: &spawned_concurrency::tasks::Context<Self>) {
            self.0.push("stopped");
        }
    }
    impl spawned_concurrency::tasks::Handler<Block> for Hooks {
        async fn handle(&mut self, block: Block, _: &spawned_concurrency::tasks::Context<Self>) {
            // Deliberately blocks the only async worker of the group.
            let _ = block.1.send(());
            let _ = block.0.lock().unwrap().recv_timeout(LIMIT);
        }
    }
    impl spawned_concurrency::threads::Actor for Hooks {
        fn started(&mut self, _: &spawned_concurrency::threads::Context<Self>) {
            self.0.push("started");
        }
        fn stopped(&mut self, _: &spawned_concurrency::threads::Context<Self>) {
            self.0.push("stopped");
        }
    }
}

/// The intended contract for an actor stopped before its first poll (GUIDE.md,
/// "Startup"): a managed actor whose admission closes before `started()` begins
/// skips both hooks and drops its state; legacy unmanaged tasks actors and
/// threads actors retain their existing behavior of running `started()` then
/// `stopped()`. All report Normal and every actor's state is dropped exactly
/// once. Listed as an allowed difference in TESTING.md.
// Not simulated: compares Tokio, Thread, Blocking and native placements.
#[cfg(not(spawned_sim))]
#[test]
fn stop_before_first_poll_follows_managed_and_legacy_contract() {
    use early_stop::*;
    // Unmanaged Async: a current-thread runtime that is not running yet.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let hooks = Recorder::new();
    let actor = {
        let _entered = rt.enter();
        spawned_concurrency::tasks::ActorStart::start(Hooks(hooks.clone()))
    };
    actor.child_handle().stop();
    let reason = rt.block_on(async { tokio::time::timeout(LIMIT, actor.wait_exit()).await });
    assert_eq!(reason.unwrap(), ExitReason::Normal);
    let done = |e: &[&str]| e.contains(&"dropped");
    let legacy = ["started", "stopped", "dropped"];
    assert_eq!(hooks.wait_until(LIMIT, "legacy tasks drop", done), legacy);

    // Legacy threads mode: no admission check either.
    let hooks = Recorder::new();
    let actor = spawned_concurrency::threads::ActorStart::start(Hooks(hooks.clone()));
    actor.child_handle().stop();
    assert_eq!(actor.wait_exit(), ExitReason::Normal);
    assert_eq!(hooks.wait_until(LIMIT, "threads drop", done), legacy);

    // Managed: the group's single worker is blocked, so the actor is admitted
    // but not yet polled when it is stopped. Neither hook runs; state drops.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("early-stop", 1).unwrap();
    let blocker_hooks = Recorder::new();
    let blocker = group.start(Hooks(blocker_hooks.clone())).unwrap();
    rt.block_on(async { tokio::time::timeout(LIMIT, blocker.ready()).await })
        .unwrap()
        .unwrap();
    let (release, wait) = std::sync::mpsc::channel();
    let (entered, blocked) = std::sync::mpsc::channel();
    blocker
        .send(Block(std::sync::Mutex::new(wait), entered))
        .unwrap();
    blocked.recv_timeout(LIMIT).unwrap();
    let hooks = Recorder::new();
    let actor = group.start(Hooks(hooks.clone())).unwrap();
    actor.child_handle().stop();
    release.send(()).unwrap();
    let reason = rt.block_on(async { tokio::time::timeout(LIMIT, actor.wait_exit()).await });
    assert_eq!(reason.unwrap(), ExitReason::Normal);
    assert_eq!(hooks.wait_until(LIMIT, "managed drop", done), ["dropped"]);
    let report = rt.block_on(system.shutdown(LIMIT));
    assert!(report.complete);
}

#[test]
fn fixed_shutdown_order_before_two_supervised_one_after_two() {
    let outcome = metamorphic(group_placements(), |env| shutdown_workload(env, 2, 1, 2));
    assert_eq!(outcome.order, vec![201, 200, 100, 1, 0]);
    assert!(outcome.complete);
}

// Not simulated: compares against a Thread-placement execution group.
#[cfg(not(spawned_sim))]
#[test]
fn ordered_system_shutdown_matches_thread_placement() {
    let mut settings = config();
    settings.source_file = Some(file!());
    let expected_cases = settings.cases;
    let completed = std::cell::Cell::new(0u32);
    let mut runner = proptest::test_runner::TestRunner::new(settings);
    runner
        .run(
            &(0u32..4, 1u32..4, 0u32..4),
            |(before, sup_children, after)| {
                let mut env = Env::new(Placement::GroupThread, 0);
                let outcome = shutdown_workload(&mut env, before, sup_children, after);
                env.finish();
                let mut order: Vec<u32> = (200..200 + after).rev().collect();
                order.extend((100..100 + sup_children).rev());
                order.extend((0..before).rev());
                prop_assert_eq!(outcome.order, order);
                prop_assert!(outcome.complete);
                completed.set(completed.get() + 1);
                Ok(())
            },
        )
        .unwrap();
    // Persisted regressions (proptest-regressions/) are replayed before the
    // configured cases, so a checkout that once recorded a failure runs more.
    assert!(
        completed.get() >= expected_cases,
        "ran {} of {expected_cases} cases",
        completed.get()
    );
    println!(
        "validated {} thread-placement shutdown-order cases",
        completed.get()
    );
}
