//! Cancellation safety: every public async operation is dropped after N polls,
//! for N = 0 until it completes (or parks waiting for an external event). After
//! each drop the framework must stay consistent: the actor is still usable or
//! cleanly stopped, effects happen at most once, registrations, monitors and
//! links are not leaked or duplicated, and a later shutdown completes within a
//! deadline. See docs/TESTING.md for the per-operation contract.
#![cfg(feature = "async")]

use spawned_concurrency::{
    message::Message,
    protocol, registry,
    supervision::{Backoff, ChildSpec, SupervisorConfig},
    system::{ActorSystem, ExecutionGroup},
    tasks::{self, Actor, ActorRef, ActorStart, Backend, Context, Handler},
    ChildHandle, Down, Exit, ExitReason, Response,
};
use std::{
    future::Future,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    task::Poll,
    time::Duration,
};
use tokio::sync::Notify;

const LIMIT: Duration = Duration::from_secs(10);
/// Upper bound on polls before an operation is reported as never completing.
const MAX_POLLS: usize = 256;
/// A future waiting on an external event is woken by nothing in this test;
/// after this long it is dropped while parked (still a cancellation point).
const PARK: Duration = Duration::from_millis(30);

// ---------------------------------------------------------------------------
// Poll-budget wrapper
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum Attempt<T> {
    /// Completed within the budget.
    Done(T),
    /// Dropped right after its N-th poll returned Pending (N = 0: never polled).
    Dropped,
    /// Waiting for an external event; dropped by the park timeout.
    Parked,
}
impl<T> Attempt<T> {
    fn finished(&self) -> bool {
        !matches!(self, Self::Dropped)
    }
}
/// Poll `future` at most `budget` times, then drop it. The future is always
/// dropped before this returns, so post-drop checks observe the cancelled state.
async fn attempt<F: Future>(future: F, budget: usize, park: Duration) -> Attempt<F::Output> {
    let mut future = Box::pin(future);
    let mut polls = 0usize;
    let result = spawned_rt::tasks::timeout(
        park,
        std::future::poll_fn(|cx| {
            if polls == budget {
                return Poll::Ready(None);
            }
            polls += 1;
            match future.as_mut().poll(cx) {
                Poll::Ready(v) => Poll::Ready(Some(v)),
                Poll::Pending if polls == budget => Poll::Ready(None),
                Poll::Pending => Poll::Pending,
            }
        }),
    )
    .await;
    drop(future);
    match result {
        Ok(Some(v)) => Attempt::Done(v),
        Ok(None) => Attempt::Dropped,
        Err(_) => Attempt::Parked,
    }
}

/// Drive `run(n)` for n = 0.. until an attempt finishes. Returns the poll count.
async fn sweep<F, Fut>(op: &str, mut run: F) -> usize
where
    F: FnMut(usize) -> Fut,
    Fut: Future<Output = bool>,
{
    for n in 0..=MAX_POLLS {
        let finished = spawned_rt::tasks::timeout(LIMIT * 3, run(n))
            .await
            .unwrap_or_else(|_| panic!("{op}: cancellation at poll {n} wedged the fixture"));
        if finished {
            if std::env::var_os("SPAWNED_CANCEL_REPORT").is_some() {
                eprintln!("cancel-safety: {op}: dropped at polls 0..{n}, finished at {n}");
            }
            return n;
        }
    }
    panic!("{op}: did not complete within {MAX_POLLS} polls");
}

// Virtual time under spawned_sim.
use spawned_rt::time::Instant;

async fn eventually(what: &str, check: impl Fn() -> bool) {
    let deadline = Instant::now() + LIMIT;
    while !check() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        spawned_rt::tasks::yield_now().await;
        spawned_rt::tasks::sleep(Duration::from_millis(1)).await;
    }
}

// ---------------------------------------------------------------------------
// Probe actor
// ---------------------------------------------------------------------------

/// Counts live actor instances: a leaked actor keeps the count above zero.
struct Live(Arc<AtomicUsize>);
impl Live {
    fn new(count: &Arc<AtomicUsize>) -> Self {
        count.fetch_add(1, Ordering::SeqCst);
        Self(count.clone())
    }
}
impl Drop for Live {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[derive(Clone, Default)]
struct Counters {
    effects: Arc<AtomicUsize>,
    live: Arc<AtomicUsize>,
    watched: Arc<AtomicUsize>,
    downs: Arc<AtomicUsize>,
    linked: Arc<AtomicUsize>,
    exits: Arc<AtomicUsize>,
}

struct Probe {
    counters: Counters,
    _live: Live,
    start_gate: Option<Arc<Notify>>,
    stop_gate: Option<Arc<Notify>>,
    name: Option<String>,
}
impl Probe {
    fn new(counters: &Counters) -> Self {
        Self {
            counters: counters.clone(),
            _live: Live::new(&counters.live),
            start_gate: None,
            stop_gate: None,
            name: None,
        }
    }
}
impl Actor for Probe {
    async fn started(&mut self, ctx: &Context<Self>) {
        // A stop gate slows stopped(); on a parent's shutdown it runs only
        // for a trapping actor (v0.8, E3).
        if self.stop_gate.is_some() {
            ctx.trap_exit(true);
        }
        if let Some(gate) = &self.start_gate {
            gate.notified().await;
        }
        if let Some(name) = &self.name {
            ctx.register_name(name).unwrap();
        }
    }
    async fn stopped(&mut self, _: &Context<Self>) {
        if let Some(gate) = &self.stop_gate {
            gate.notified().await;
        }
    }
    async fn exit_received(&mut self, _: Exit, _: &Context<Self>) {
        self.counters.exits.fetch_add(1, Ordering::SeqCst);
    }
}

#[derive(Debug)]
enum Cmd {
    Bump,
    Read,
    Watch(ChildHandle),
    Link(ChildHandle),
    Trap,
    Helper(Arc<Notify>),
    Boom,
}
impl Message for Cmd {
    type Result = u64;
}
impl Handler<Cmd> for Probe {
    async fn handle(
        &mut self,
        cmd: Cmd,
        reply: spawned_concurrency::reply::PendingReply<'_, u64>,
        ctx: &Context<Self>,
    ) {
        let __reply_value: u64 = {
            let c = &self.counters;
            match cmd {
                Cmd::Bump => c.effects.fetch_add(1, Ordering::SeqCst) as u64 + 1,
                Cmd::Read => c.effects.load(Ordering::SeqCst) as u64,
                Cmd::Watch(target) => {
                    ctx.monitor(&target);
                    c.watched.fetch_add(1, Ordering::SeqCst) as u64
                }
                Cmd::Link(target) => {
                    ctx.link(&target);
                    c.linked.fetch_add(1, Ordering::SeqCst) as u64
                }
                Cmd::Trap => {
                    ctx.trap_exit(true);
                    0
                }
                Cmd::Boom => panic!("cancellation fixture crash"),
                Cmd::Helper(gate) => {
                    let task = ctx
                        .spawn_helper(async move { gate.notified().await })
                        .unwrap();
                    // Drop the ScopedTask: the helper stays owned by the actor scope.
                    drop(task);
                    0
                }
            }
        };
        reply.reply(__reply_value);
    }
}
impl Handler<Down> for Probe {
    async fn handle(
        &mut self,
        _: Down,
        reply: spawned_concurrency::reply::PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
        self.counters.downs.fetch_add(1, Ordering::SeqCst);
        reply.reply(());
    }
}

#[protocol]
pub trait Bumper: Send + Sync {
    fn bump(&self) -> Response<u64>;
}
impl Handler<bumper::Bump> for Probe {
    async fn handle(
        &mut self,
        _: bumper::Bump,
        reply: spawned_concurrency::reply::PendingReply<'_, u64>,
        _: &Context<Self>,
    ) {
        let __reply_value: u64 =
            { self.counters.effects.fetch_add(1, Ordering::SeqCst) as u64 + 1 };
        reply.reply(__reply_value);
    }
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(spawned_sim, allow(dead_code))]
enum Place {
    Async,
    Blocking,
    Thread,
    GroupAsync,
    GroupThread,
}
#[cfg(not(spawned_sim))]
const PLACES: [Place; 5] = [
    Place::Async,
    Place::Blocking,
    Place::Thread,
    Place::GroupAsync,
    Place::GroupThread,
];
#[cfg(not(spawned_sim))]
const GROUPS: [Place; 2] = [Place::GroupAsync, Place::GroupThread];
// The simulator runs only async placements (Blocking, Thread and native
// groups need OS threads).
#[cfg(spawned_sim)]
const PLACES: [Place; 2] = [Place::Async, Place::GroupAsync];
#[cfg(spawned_sim)]
const GROUPS: [Place; 1] = [Place::GroupAsync];

struct Fx {
    place: Place,
    system: Option<ActorSystem>,
    group: Option<ExecutionGroup>,
    counters: Counters,
}
impl Fx {
    fn new(place: Place) -> Self {
        let (system, group) = match place {
            Place::GroupAsync | Place::GroupThread => {
                let system = ActorSystem::new().unwrap();
                let group = if place == Place::GroupAsync {
                    system.async_group("cancel", 2).unwrap()
                } else {
                    system.thread_group("cancel", 16).unwrap()
                };
                (Some(system), Some(group))
            }
            _ => (None, None),
        };
        Self {
            place,
            system,
            group,
            counters: Counters::default(),
        }
    }
    fn spawn(&self, probe: Probe) -> ActorRef<Probe> {
        match self.place {
            Place::Async => probe.start_with_backend(Backend::Async),
            Place::Blocking => probe.start_with_backend(Backend::Blocking),
            Place::Thread => probe.start_with_backend(Backend::Thread),
            Place::GroupAsync | Place::GroupThread => {
                self.group.as_ref().unwrap().start(probe).unwrap()
            }
        }
    }
    fn probe(&self) -> Probe {
        Probe::new(&self.counters)
    }
    /// Stop actors, require bounded completion and no surviving instances.
    async fn finish(self, actors: &[&ActorRef<Probe>]) {
        for actor in actors {
            actor.child_handle().stop();
            spawned_rt::tasks::timeout(LIMIT, actor.wait_exit())
                .await
                .unwrap_or_else(|_| panic!("{:?}: actor did not stop", self.place));
        }
        if let Some(system) = &self.system {
            let report = system.shutdown(LIMIT).await;
            assert!(
                report.complete,
                "{:?}: system shutdown incomplete",
                self.place
            );
        }
        let live = self.counters.live.clone();
        eventually("actor state release", || live.load(Ordering::SeqCst) == 0).await;
    }
}

async fn usable(actor: &ActorRef<Probe>) {
    let before = actor.request(Cmd::Read).await.unwrap();
    assert_eq!(actor.request(Cmd::Bump).await.unwrap(), before + 1);
}

// ---------------------------------------------------------------------------
// Request paths
// ---------------------------------------------------------------------------

/// `request` is an `async fn`: an unpolled request enqueues nothing. Once the
/// first poll enqueues it, dropping the caller does not cancel the effect.
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn request_family_is_cancel_safe() {
    for place in PLACES {
        for variant in 0..3 {
            let op = format!("{place:?} request variant {variant}");
            sweep(&op, |n| {
                let op = op.clone();
                async move {
                    let fx = Fx::new(place);
                    let actor = fx.spawn(fx.probe());
                    let recipient = actor.recipient::<Cmd>();
                    let outcome = match variant {
                        0 => attempt(actor.request(Cmd::Bump), n, LIMIT).await,
                        1 => attempt(actor.request_with_timeout(Cmd::Bump, LIMIT), n, LIMIT).await,
                        _ => attempt(tasks::request(&*recipient, Cmd::Bump, LIMIT), n, LIMIT).await,
                    };
                    // A same-sender request is a fence for the dropped one.
                    let effects = actor.request(Cmd::Read).await.unwrap() as usize;
                    match &outcome {
                        Attempt::Done(reply) => {
                            assert_eq!(reply.as_ref().unwrap(), &1, "{op}");
                            assert_eq!(effects, 1, "{op}");
                        }
                        Attempt::Dropped if n == 0 => assert_eq!(effects, 0, "{op}: N=0"),
                        Attempt::Dropped => assert_eq!(effects, 1, "{op}: N={n}"),
                        Attempt::Parked => panic!("{op}: request parked at N={n}"),
                    }
                    usable(&actor).await;
                    fx.finish(&[&actor]).await;
                    outcome.finished()
                }
            })
            .await;
        }
    }
}

/// Protocol calls send eagerly when the method is called, before any poll:
/// dropping the returned `Response` (even unpolled) never cancels the effect.
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn protocol_calls_are_cancel_safe_and_send_eagerly() {
    for place in PLACES {
        let op = format!("{place:?} protocol call");
        let polls = sweep(&op, |n| {
            let op = op.clone();
            async move {
                let fx = Fx::new(place);
                let actor = fx.spawn(fx.probe());
                let outcome = attempt(actor.bump(), n, LIMIT).await;
                let effects = actor.request(Cmd::Read).await.unwrap();
                assert_eq!(effects, 1, "{op}: N={n} (eager send)");
                if let Attempt::Done(reply) = &outcome {
                    assert_eq!(reply.as_ref().unwrap(), &1);
                }
                usable(&actor).await;
                fx.finish(&[&actor]).await;
                outcome.finished()
            }
        })
        .await;
        assert!(polls >= 1);
    }
}

/// Monitor and link setup run inside handlers. A dropped request that was
/// enqueued still registers exactly one monitor/link, and it fires exactly once.
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn monitor_and_link_setup_requests_are_cancel_safe() {
    for place in PLACES {
        for link in [false, true] {
            let op = format!("{place:?} {} setup", if link { "link" } else { "monitor" });
            sweep(&op, |n| {
                let op = op.clone();
                async move {
                    let fx = Fx::new(place);
                    let observer = fx.spawn(fx.probe());
                    let target = fx.spawn(fx.probe());
                    if link {
                        observer.request(Cmd::Trap).await.unwrap();
                    }
                    let setup = if link {
                        Cmd::Link(target.child_handle())
                    } else {
                        Cmd::Watch(target.child_handle())
                    };
                    let outcome = attempt(observer.request(setup), n, LIMIT).await;
                    observer.request(Cmd::Read).await.unwrap();
                    let registered = if link {
                        fx.counters.linked.load(Ordering::SeqCst)
                    } else {
                        fx.counters.watched.load(Ordering::SeqCst)
                    };
                    assert_eq!(registered, usize::from(n > 0), "{op}: N={n}");
                    target.child_handle().shutdown();
                    assert_eq!(
                        spawned_rt::tasks::timeout(LIMIT, target.wait_exit())
                            .await
                            .unwrap(),
                        ExitReason::Shutdown
                    );
                    let counters = fx.counters.clone();
                    let delivered = move || {
                        if link {
                            counters.exits.load(Ordering::SeqCst)
                        } else {
                            counters.downs.load(Ordering::SeqCst)
                        }
                    };
                    let expected = registered;
                    let probe = delivered.clone();
                    eventually("exit notification", move || probe() >= expected).await;
                    // Fence, then require no duplicate delivery.
                    observer.request(Cmd::Read).await.unwrap();
                    assert_eq!(delivered(), expected, "{op}: N={n} duplicate notification");
                    usable(&observer).await;
                    fx.finish(&[&observer]).await;
                    outcome.finished()
                }
            })
            .await;
        }
    }
}

// ---------------------------------------------------------------------------
// Waits
// ---------------------------------------------------------------------------

/// join, wait_exit and ChildHandle::wait_exit_async park until the actor
/// stops. Dropping them at any point leaves the actor running and later
/// waiters still observe the single exit reason.
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn exit_waits_are_cancel_safe() {
    for place in PLACES {
        for variant in 0..3 {
            let op = format!("{place:?} exit wait {variant}");
            sweep(&op, |n| {
                let op = op.clone();
                async move {
                    let fx = Fx::new(place);
                    let actor = fx.spawn(fx.probe());
                    let child = actor.child_handle();
                    let outcome = match variant {
                        0 => attempt(actor.join(), n, PARK).await.map_unit(),
                        1 => attempt(actor.wait_exit(), n, PARK).await.map_unit(),
                        _ => attempt(child.wait_exit_async(), n, PARK).await.map_unit(),
                    };
                    assert!(
                        !matches!(outcome, Attempt::Done(())),
                        "{op}: returned while alive"
                    );
                    assert!(child.is_alive());
                    usable(&actor).await;
                    child.stop();
                    let (a, b) = tokio::join!(actor.wait_exit(), child.wait_exit_async());
                    assert_eq!((a, b), (ExitReason::Normal, ExitReason::Normal));
                    fx.finish(&[&actor]).await;
                    outcome.finished()
                }
            })
            .await;
        }
    }
}
impl<T> Attempt<T> {
    fn map_unit(self) -> Attempt<()> {
        match self {
            Self::Done(_) => Attempt::Done(()),
            Self::Dropped => Attempt::Dropped,
            Self::Parked => Attempt::Parked,
        }
    }
}

/// ActorRef::ready (execution group start barrier) parks on a gated started().
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn group_ready_wait_is_cancel_safe() {
    for place in GROUPS {
        let op = format!("{place:?} ready");
        sweep(&op, |n| {
            let op = op.clone();
            async move {
                let fx = Fx::new(place);
                let gate = Arc::new(Notify::new());
                let name = format!("cancel-ready-{place:?}-{n}");
                let mut probe = fx.probe();
                probe.start_gate = Some(gate.clone());
                probe.name = Some(name.clone());
                let actor = fx.spawn(probe);
                let outcome = attempt(actor.ready(), n, PARK).await;
                assert!(
                    !matches!(outcome, Attempt::Done(_)),
                    "{op}: ready before started()"
                );
                gate.notify_one();
                spawned_rt::tasks::timeout(LIMIT, actor.ready())
                    .await
                    .unwrap()
                    .unwrap();
                assert!(registry::registered().contains(&name));
                usable(&actor).await;
                fx.finish(&[&actor]).await;
                assert!(
                    !registry::registered().contains(&name),
                    "{op}: leaked registration"
                );
                outcome.finished()
            }
        })
        .await;
    }
}

// ---------------------------------------------------------------------------
// Supervisor and system lifecycle
// ---------------------------------------------------------------------------

fn config() -> SupervisorConfig {
    SupervisorConfig {
        max_restarts: 100,
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

/// SupervisorBuilder::start: a dropped startup requests cleanup of every
/// constructed child; nothing stays running or registered afterwards.
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn supervisor_start_is_cancel_safe() {
    for place in GROUPS {
        for gated in [false, true] {
            let op = format!("{place:?} supervisor start (gated factory: {gated})");
            sweep(&op, |n| {
                let op = op.clone();
                async move {
                    let fx = Fx::new(place);
                    let system = fx.system.as_ref().unwrap();
                    let group = fx.group.clone().unwrap();
                    let mut tree = system.supervisor(config()).unwrap();
                    let names: Vec<String> = (0..3)
                        .map(|i| format!("cancel-sup-{place:?}-{gated}-{n}-{i}"))
                        .collect();
                    let children: Vec<_> = names
                        .iter()
                        .enumerate()
                        .map(|(i, name)| {
                            let counters = fx.counters.clone();
                            let name = name.clone();
                            // A gated middle factory holds startup inside child construction.
                            let gate = (gated && i == 1).then(|| Arc::new(Notify::new()));
                            tree.add(ChildSpec::new(name.clone(), &group, move |_| {
                                let mut probe = Probe::new(&counters);
                                probe.name = Some(name.clone());
                                let gate = gate.clone();
                                async move {
                                    if let Some(gate) = gate {
                                        gate.notified().await;
                                    }
                                    Ok::<_, String>(probe)
                                }
                            }))
                            .unwrap()
                        })
                        .collect();
                    let park = if gated { PARK } else { LIMIT };
                    let outcome = attempt(tree.start(), n, park).await;
                    match &outcome {
                        Attempt::Done(Ok(supervisor)) => {
                            for child in &children {
                                usable(&child.current().unwrap()).await;
                            }
                            assert!(supervisor.shutdown(LIMIT).await.complete, "{op}");
                        }
                        Attempt::Done(Err(e)) => panic!("{op}: start failed: {e}"),
                        Attempt::Parked if !gated => panic!("{op}: start parked at N={n}"),
                        Attempt::Parked | Attempt::Dropped => {
                            // Cancellation closes the tree: waits resolve, nothing restarts.
                            for child in &children {
                                let ready = spawned_rt::tasks::timeout(LIMIT, child.wait_ready())
                                    .await
                                    .unwrap_or_else(|_| panic!("{op}: N={n} child wait wedged"));
                                assert!(ready.is_err(), "{op}: N={n} child ready after cancel");
                            }
                        }
                    }
                    let live = fx.counters.live.clone();
                    eventually("children stopped", || live.load(Ordering::SeqCst) == 0).await;
                    // Scope registrations drop after the actor state, so wait for them too.
                    let names_left = names.clone();
                    eventually("registry release", move || {
                        let registered = registry::registered();
                        names_left.iter().all(|n| !registered.contains(n))
                    })
                    .await;
                    fx.finish(&[]).await;
                    let registered = registry::registered();
                    assert!(
                        names.iter().all(|n| !registered.contains(n)),
                        "{op}: N={n} leaked registration"
                    );
                    outcome.finished()
                }
            })
            .await;
        }
    }
}

/// Supervisor::shutdown requests stop synchronously; dropping the wait never
/// leaves the tree half-stopped.
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn supervisor_shutdown_and_child_waits_are_cancel_safe() {
    for place in GROUPS {
        for variant in 0..4 {
            let op = format!("{place:?} supervisor wait {variant}");
            sweep(&op, |n| {
                let op = op.clone();
                async move {
                    let fx = Fx::new(place);
                    let system = fx.system.as_ref().unwrap();
                    let group = fx.group.clone().unwrap();
                    let mut tree = system.supervisor(config()).unwrap();
                    let counters = fx.counters.clone();
                    let child = tree
                        .add(ChildSpec::new("child", &group, move |_| {
                            let probe = Probe::new(&counters);
                            async move { Ok::<_, String>(probe) }
                        }))
                        .unwrap();
                    let supervisor = tree.start().await.unwrap();
                    let first = child.current().unwrap();
                    let finished = match variant {
                        0 => {
                            let outcome = attempt(supervisor.shutdown(LIMIT), n, LIMIT).await;
                            if let Attempt::Done(report) = &outcome {
                                assert!(report.complete, "{op}");
                            }
                            if n == 0 {
                                // An unpolled async fn requested nothing: the tree still serves.
                                usable(&child.current().unwrap()).await;
                                supervisor.stop();
                            }
                            // The first poll requests stop; the tree must finish regardless.
                            spawned_rt::tasks::timeout(LIMIT, supervisor.join())
                                .await
                                .unwrap_or_else(|_| panic!("{op}: N={n} tree wedged"));
                            assert_eq!(first.wait_exit().await, ExitReason::Shutdown);
                            outcome.finished()
                        }
                        1 => {
                            let outcome = attempt(supervisor.join(), n, PARK).await;
                            assert!(!matches!(outcome, Attempt::Done(_)));
                            usable(&first).await;
                            assert!(supervisor.shutdown(LIMIT).await.complete, "{op}");
                            outcome.finished()
                        }
                        2 => {
                            // wait_ready on a ready child completes; drop it at every poll.
                            let outcome = attempt(child.wait_ready(), n, LIMIT).await;
                            if let Attempt::Done(r) = &outcome {
                                assert_eq!(r.as_ref().unwrap().id(), first.id());
                            }
                            usable(&child.current().unwrap()).await;
                            assert!(supervisor.shutdown(LIMIT).await.complete, "{op}");
                            outcome.finished()
                        }
                        _ => {
                            // wait_for_generation parks until a restart.
                            let outcome = attempt(child.wait_for_generation(1), n, PARK).await;
                            assert!(!matches!(outcome, Attempt::Done(_)));
                            assert!(first.request(Cmd::Boom).await.is_err());
                            let next =
                                spawned_rt::tasks::timeout(LIMIT, child.wait_for_generation(1))
                                    .await
                                    .unwrap()
                                    .unwrap();
                            assert_ne!(next.id(), first.id());
                            usable(&next).await;
                            assert!(supervisor.shutdown(LIMIT).await.complete, "{op}");
                            outcome.finished()
                        }
                    };
                    fx.finish(&[]).await;
                    finished
                }
            })
            .await;
        }
    }
}
/// ActorSystem::shutdown and ShutdownReport::wait.
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn system_shutdown_is_cancel_safe() {
    for place in GROUPS {
        let op = format!("{place:?} system shutdown");
        sweep(&op, |n| {
            let op = op.clone();
            async move {
                let fx = Fx::new(place);
                let name = format!("cancel-system-{place:?}-{n}");
                let mut probe = fx.probe();
                probe.name = Some(name.clone());
                let actor = fx.spawn(probe);
                actor.ready().await.unwrap();
                let helper_gate = Arc::new(Notify::new());
                actor.request(Cmd::Helper(helper_gate)).await.unwrap();
                let system = fx.system.as_ref().unwrap();
                let outcome = attempt(system.shutdown(LIMIT), n, LIMIT).await;
                if let Attempt::Done(report) = &outcome {
                    assert!(report.complete, "{op}");
                }
                // Admission closed on the first poll; a second shutdown completes.
                let report = spawned_rt::tasks::timeout(LIMIT * 2, system.shutdown(LIMIT))
                    .await
                    .unwrap_or_else(|_| panic!("{op}: N={n} repeated shutdown wedged"));
                if n > 0 {
                    assert!(report.complete, "{op}: N={n}");
                    assert!(fx.group.as_ref().unwrap().is_closed());
                    assert_eq!(actor.wait_exit().await, ExitReason::Shutdown);
                    assert!(!registry::registered().contains(&name), "{op}: leaked name");
                }
                fx.finish(&[&actor]).await;
                assert!(!registry::registered().contains(&name), "{op}: leaked name");
                outcome.finished()
            }
        })
        .await;
    }
    // A report that expired with a stuck stopped() can be awaited again.
    for place in GROUPS {
        let op = format!("{place:?} report wait");
        sweep(&op, |n| {
            let op = op.clone();
            async move {
                let fx = Fx::new(place);
                let gate = Arc::new(Notify::new());
                let mut probe = fx.probe();
                probe.stop_gate = Some(gate.clone());
                let actor = fx.spawn(probe);
                actor.ready().await.unwrap();
                let report = fx.system.as_ref().unwrap().shutdown(Duration::ZERO).await;
                assert!(!report.complete);
                let outcome = attempt(report.wait(LIMIT), n, PARK).await;
                assert!(
                    !matches!(outcome, Attempt::Done(ref r) if r.complete),
                    "{op}"
                );
                // Zero grace: the system kills at once after its ordered pass
                // (E3); let the kill land while stopped() still runs (E1).
                spawned_rt::tasks::sleep(Duration::from_millis(50)).await;
                gate.notify_one();
                assert_eq!(
                    spawned_rt::tasks::timeout(LIMIT, actor.wait_exit())
                        .await
                        .unwrap(),
                    ExitReason::Killed
                );
                fx.finish(&[]).await;
                outcome.finished()
            }
        })
        .await;
    }
}

/// ScopedTask::join for owned helpers.
#[spawned_macros::sim_test(flavor = "multi_thread", worker_threads = 2)]
async fn helper_join_is_cancel_safe() {
    for place in GROUPS {
        let op = format!("{place:?} helper join");
        sweep(&op, |n| {
            let op = op.clone();
            async move {
                let fx = Fx::new(place);
                let actor = fx.spawn(fx.probe());
                actor.ready().await.unwrap();
                let gate = Arc::new(Notify::new());
                let waiting = gate.clone();
                let task = actor
                    .context()
                    .spawn_helper(async move {
                        waiting.notified().await;
                        7u8
                    })
                    .unwrap();
                let outcome = attempt(task.join(), n, PARK).await;
                assert!(!matches!(outcome, Attempt::Done(_)), "{op}");
                gate.notify_one();
                // The detached helper remains owned and is cancelled or joined at shutdown.
                let second = actor
                    .context()
                    .spawn_helper(async { 9u8 })
                    .unwrap()
                    .join()
                    .await;
                assert_eq!(second, Ok(9));
                fx.finish(&[&actor]).await;
                outcome.finished()
            }
        })
        .await;
    }
}
