//! Deterministic simulation backend, compiled only under `--cfg spawned_sim`
//! (with `--cfg tokio_unstable`, see below).
//!
//! One executor per OS thread drives every task spawned through
//! `spawned_rt::tasks` on that thread:
//!
//! - **One thread.** Tasks are polled only by the thread that owns the world.
//!   A wakeup from any other thread is a determinism leak: the executor
//!   records it and the run panics naming the thread.
//! - **Seeded scheduling.** When several tasks are ready, a SplitMix64
//!   generator seeded per world picks which one to poll next, by rank among
//!   the ready task ids (so the order in which wakers fired does not matter).
//!   The same seed and the same program give the same schedule.
//! - **Tokio's per-poll context.** Each poll runs inside a private,
//!   driverless current-thread Tokio runtime built per world, so Tokio's
//!   cooperative budget applies (a task that keeps receiving from a ready
//!   channel is forced to yield after 128 operations, as on Tokio) and Tokio's
//!   thread-local RNG (`select!` branch order, `watch` waiter shards) is
//!   seeded from the world's seed. Seeding that RNG needs Tokio's
//!   `rng_seed`, so the cfg must be paired with `--cfg tokio_unstable`.
//!   A task spawned directly on Tokio (`tokio::spawn`) is a determinism leak
//!   and fails the run.
//! - **Virtual time.** `sleep` and `timeout` use a virtual clock that only
//!   advances when no task is ready, jumping to the earliest pending timer.
//!   A sleep whose deadline has already passed still yields once, as Tokio's
//!   timer does.
//! - **Detected limits.** `spawn_blocking`, `block_in_place` and a nested
//!   `block_on` panic with a message naming the call; a run with no ready task
//!   and no pending timer panics as a deadlock instead of hanging. A single
//!   poll that runs longer than `SPAWNED_SIM_POLL_TIMEOUT` wall-clock seconds
//!   (default 60, 0 = off; a loop with no await point that the budget can
//!   interrupt) aborts the process with a message naming the task and seed.
//!
//! Channels, `watch`, `CancellationToken` and the stream wrappers stay Tokio's:
//! they only use wakers, so they are deterministic under this executor.
//!
//! Fidelity limits: everything a task does inside one poll is atomic (no OS
//! preemption, no weak-memory reorderings), and the wall clock
//! (`Instant::now`, `SystemTime`), `std::thread`, Tokio I/O and `HashMap`
//! iteration order stay outside its control.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::future::Future;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};
use std::task::{Context, Poll, Wake, Waker};
use std::thread::ThreadId;
use std::time::Duration;

type BoxTask = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

const ROOT: u64 = 0;
const DEFAULT_MAX_STEPS: u64 = 50_000_000;
/// Wall-clock seconds one poll may run before the watchdog aborts the process.
const DEFAULT_POLL_TIMEOUT_SECS: u64 = 60;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn splitmix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Index in `ready` of the task whose id has rank `rank` (0 = smallest), so
/// a seeded pick depends on which tasks are ready, not on the order their
/// wakers fired.
fn by_rank(ready: &[u64], rank: usize) -> usize {
    let mut ids = ready.to_vec();
    let (_, id, _) = ids.select_nth_unstable(rank);
    let id = *id;
    ready.iter().position(|r| *r == id).unwrap_or(0)
}

/// Wall-clock guard against a poll that never returns (a loop with no await
/// point, which neither the scheduler nor the cooperative budget can
/// interrupt on one thread). One process-wide thread checks every live world.
mod watchdog {
    use super::{lock, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex, OnceLock, Weak};
    use std::time::{Duration, Instant};

    pub(super) struct Watch {
        seed: u64,
        /// Milliseconds since `base()` when the current poll began, 0 if idle.
        since: AtomicU64,
        task: AtomicU64,
    }

    fn base() -> Instant {
        static BASE: OnceLock<Instant> = OnceLock::new();
        *BASE.get_or_init(Instant::now)
    }

    fn limit() -> u64 {
        std::env::var("SPAWNED_SIM_POLL_TIMEOUT")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(super::DEFAULT_POLL_TIMEOUT_SECS)
    }

    static WATCHED: Mutex<Vec<Weak<Watch>>> = Mutex::new(Vec::new());

    impl Watch {
        pub(super) fn register(seed: u64) -> Arc<Self> {
            let watch = Arc::new(Self {
                seed,
                since: AtomicU64::new(0),
                task: AtomicU64::new(0),
            });
            let limit = limit();
            if limit > 0 {
                static STARTED: OnceLock<()> = OnceLock::new();
                STARTED.get_or_init(|| {
                    base();
                    std::thread::Builder::new()
                        .name("spawned-sim-watchdog".into())
                        .spawn(move || run(limit))
                        .expect("spawned_sim: cannot start the poll watchdog");
                });
                let mut watched = lock(&WATCHED);
                watched.retain(|w| w.strong_count() > 0);
                watched.push(Arc::downgrade(&watch));
            }
            watch
        }

        pub(super) fn busy(&self, task: u64) -> Busy<'_> {
            self.task.store(task, Ordering::Relaxed);
            let now = base().elapsed().as_millis() as u64;
            self.since.store(now.max(1), Ordering::Release);
            Busy(self)
        }
    }

    pub(super) struct Busy<'a>(&'a Watch);
    impl Drop for Busy<'_> {
        fn drop(&mut self) {
            self.0.since.store(0, Ordering::Release);
        }
    }

    fn run(limit: u64) {
        loop {
            std::thread::sleep(Duration::from_millis(200));
            let now = base().elapsed().as_millis() as u64;
            for watch in lock(&WATCHED).iter().filter_map(Weak::upgrade) {
                let since = watch.since.load(Ordering::Acquire);
                if since != 0 && now.saturating_sub(since) > limit * 1000 {
                    eprintln!(
                        "spawned_sim: task {} has been inside one poll for over {limit} s \
                         (seed {}): a loop with no await point never returns to the \
                         scheduler; aborting (SPAWNED_SIM_POLL_TIMEOUT sets the limit)",
                        watch.task.load(Ordering::Relaxed),
                        watch.seed
                    );
                    std::process::abort();
                }
            }
        }
    }
}

struct State {
    seed: u64,
    rng: u64,
    now: Duration,
    next_id: u64,
    tasks: BTreeMap<u64, Arc<Mutex<Option<BoxTask>>>>,
    ready: Vec<u64>,
    queued: BTreeSet<u64>,
    timers: BTreeMap<(Duration, u64), Waker>,
    next_timer: u64,
    steps: u64,
    hash: u64,
    trace: Option<Vec<(u64, Duration)>>,
    foreign: Option<String>,
    running: bool,
    runtimes: usize,
    /// Seed of the per-world Tokio runtime's RNG (`select!`, `watch`).
    tokio_seed: u64,
}

struct Shared {
    owner: ThreadId,
    max_steps: u64,
    state: Mutex<State>,
    /// Driverless Tokio runtime every poll runs inside (see the module docs),
    /// built on the first poll so a replay can still choose its seed.
    tokio: OnceLock<::tokio::runtime::Runtime>,
    watch: Arc<watchdog::Watch>,
}

impl Drop for Shared {
    fn drop(&mut self) {
        // May run inside another world's poll, where a blocking shutdown panics.
        if let Some(runtime) = self.tokio.take() {
            runtime.shutdown_background();
        }
    }
}

impl Shared {
    fn new(seed: u64) -> Arc<Self> {
        let max_steps = std::env::var("SPAWNED_SIM_MAX_STEPS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_MAX_STEPS);
        Arc::new(Self {
            owner: std::thread::current().id(),
            max_steps,
            tokio: OnceLock::new(),
            watch: watchdog::Watch::register(seed),
            state: Mutex::new(State {
                seed,
                rng: seed,
                now: Duration::ZERO,
                next_id: ROOT + 1,
                tasks: BTreeMap::new(),
                ready: Vec::new(),
                queued: BTreeSet::new(),
                timers: BTreeMap::new(),
                next_timer: 0,
                steps: 0,
                hash: 0xcbf2_9ce4_8422_2325,
                trace: None,
                foreign: None,
                running: false,
                runtimes: 0,
                tokio_seed: seed,
            }),
        })
    }

    /// Run one poll (`f`) inside this world's Tokio runtime: with a fresh
    /// cooperative budget and a seeded Tokio RNG, under the poll watchdog.
    ///
    /// Tokio defers some wakeups (a forced budget yield, `tokio::task::yield_now`)
    /// to the end of its scheduler tick, so the wrapper returns `Pending` once
    /// after `f` to let Tokio deliver them to our wakers before `block_on`
    /// returns.
    fn run_poll<R>(&self, id: u64, f: impl FnOnce() -> R) -> R {
        let runtime = self.tokio.get_or_init(|| {
            let seed = lock(&self.state).tokio_seed;
            ::tokio::runtime::Builder::new_current_thread()
                .rng_seed(::tokio::runtime::RngSeed::from_bytes(&seed.to_le_bytes()))
                .build()
                .expect("spawned_sim: cannot build the per-world Tokio runtime")
        });
        let spawned = runtime.metrics().spawned_tasks_count();
        let mut f = Some(f);
        let mut out = None;
        let _busy = self.watch.busy(id);
        runtime.block_on(std::future::poll_fn(|cx| match f.take() {
            Some(f) => {
                out = Some(f());
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            None => Poll::Ready(()),
        }));
        if runtime.metrics().spawned_tasks_count() != spawned {
            let mut state = lock(&self.state);
            if state.foreign.is_none() {
                state.foreign = Some(format!(
                    "task {id} spawned a task on Tokio directly (tokio::spawn); \
                     use spawned_rt::tasks::spawn"
                ));
            }
        }
        out.expect("spawned_sim: poll did not run")
    }

    fn schedule(&self, id: u64) {
        let mut state = lock(&self.state);
        if std::thread::current().id() != self.owner && state.foreign.is_none() {
            let thread = std::thread::current();
            state.foreign = Some(format!(
                "task {id} woken from foreign thread {:?} ({})",
                thread.id(),
                thread.name().unwrap_or("unnamed")
            ));
        }
        if (id == ROOT || state.tasks.contains_key(&id)) && state.queued.insert(id) {
            state.ready.push(id);
        }
    }

    fn spawn_boxed(self: &Arc<Self>, id: u64, task: BoxTask) {
        lock(&self.state)
            .tasks
            .insert(id, Arc::new(Mutex::new(Some(task))));
        self.schedule(id);
    }

    fn reserve_id(&self) -> u64 {
        let mut state = lock(&self.state);
        let id = state.next_id;
        state.next_id += 1;
        id
    }

    /// Pick the next task to poll, advancing virtual time when nothing is
    /// ready. Returns `None` on deadlock.
    fn next(&self) -> Option<u64> {
        loop {
            let mut state = lock(&self.state);
            if let Some(foreign) = state.foreign.take() {
                drop(state);
                panic!("spawned_sim: determinism leak: {foreign}");
            }
            if !state.ready.is_empty() {
                let len = state.ready.len() as u64;
                let rank = (splitmix(&mut state.rng) % len) as usize;
                let index = by_rank(&state.ready, rank);
                let id = state.ready.swap_remove(index);
                state.queued.remove(&id);
                state.steps += 1;
                let now = state.now;
                state.hash = (state.hash ^ id).wrapping_mul(0x0100_0000_01b3);
                state.hash = (state.hash ^ now.as_nanos() as u64).wrapping_mul(0x0100_0000_01b3);
                if let Some(trace) = &mut state.trace {
                    trace.push((id, now));
                }
                if state.steps > self.max_steps {
                    let (seed, steps) = (state.seed, state.steps);
                    drop(state);
                    panic!(
                        "spawned_sim: livelock: {steps} polls without finishing (seed {seed}); \
                         raise SPAWNED_SIM_MAX_STEPS if this is expected"
                    );
                }
                return Some(id);
            }
            let (deadline, _) = *state.timers.keys().next()?;
            state.now = deadline;
            let mut due = Vec::new();
            while let Some(entry) = state.timers.first_entry() {
                if entry.key().0 > deadline {
                    break;
                }
                due.push(entry.remove());
            }
            drop(state);
            for waker in due {
                waker.wake();
            }
        }
    }

    fn poll_task(self: &Arc<Self>, id: u64) {
        let Some(slot) = lock(&self.state).tasks.get(&id).cloned() else {
            return;
        };
        let Some(mut task) = lock(&slot).take() else {
            return;
        };
        let waker = Waker::from(Arc::new(TaskWaker {
            id,
            shared: Arc::downgrade(self),
        }));
        let done = self.run_poll(id, || {
            task.as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_ready()
        });
        if done {
            self.retire(id);
            drop(task);
        } else {
            *lock(&slot) = Some(task);
        }
    }

    /// Forget a finished task, including a wakeup it gave itself during its
    /// final poll (otherwise a dead id stays in the ready list).
    fn retire(&self, id: u64) {
        let mut state = lock(&self.state);
        state.tasks.remove(&id);
        if state.queued.remove(&id) {
            state.ready.retain(|ready| *ready != id);
        }
    }

    fn block_on<F: Future>(self: &Arc<Self>, future: F) -> F::Output {
        assert_eq!(
            std::thread::current().id(),
            self.owner,
            "spawned_sim: block_on called from a thread that does not own the simulation"
        );
        {
            let mut state = lock(&self.state);
            assert!(
                !state.running,
                "spawned_sim: nested block_on (for example ChildHandle::wait_exit_blocking \
                 or Handle::block_on inside a task) cannot run on the single simulation thread"
            );
            state.running = true;
        }
        struct Running<'a>(&'a Shared);
        impl Drop for Running<'_> {
            fn drop(&mut self) {
                lock(&self.0.state).running = false;
            }
        }
        let _running = Running(self);
        let _context = enter(self.clone());
        let mut future = std::pin::pin!(future);
        let waker = Waker::from(Arc::new(TaskWaker {
            id: ROOT,
            shared: Arc::downgrade(self),
        }));
        self.schedule(ROOT);
        loop {
            let Some(id) = self.next() else {
                let state = lock(&self.state);
                let (seed, now, tasks) = (state.seed, state.now, state.tasks.len());
                drop(state);
                panic!(
                    "spawned_sim: deadlock: no ready task and no pending timer \
                     (seed {seed}, virtual time {now:?}, {tasks} parked tasks)"
                );
            };
            if id == ROOT {
                let polled = self.run_poll(ROOT, || {
                    future.as_mut().poll(&mut Context::from_waker(&waker))
                });
                if let Poll::Ready(output) = polled {
                    self.retire(ROOT);
                    return output;
                }
            } else {
                self.poll_task(id);
            }
        }
    }

    /// Drop every remaining task inside the simulation context.
    fn drain(self: &Arc<Self>) {
        let _context = enter(self.clone());
        for _ in 0..64 {
            let tasks = std::mem::take(&mut lock(&self.state).tasks);
            if tasks.is_empty() {
                break;
            }
            for slot in tasks.into_values() {
                let task = lock(&slot).take();
                drop(task);
            }
        }
        let mut state = lock(&self.state);
        state.ready.clear();
        state.queued.clear();
        state.timers.clear();
    }
}

struct TaskWaker {
    id: u64,
    shared: Weak<Shared>,
}
impl Wake for TaskWaker {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        if let Some(shared) = self.shared.upgrade() {
            shared.schedule(self.id);
        }
    }
}

thread_local! {
    static AMBIENT: RefCell<Weak<Shared>> = const { RefCell::new(Weak::new()) };
    static CURRENT: RefCell<Vec<Arc<Shared>>> = const { RefCell::new(Vec::new()) };
}

fn ambient_or_new() -> Arc<Shared> {
    AMBIENT.with(|ambient| {
        if let Some(shared) = ambient.borrow().upgrade() {
            return shared;
        }
        let seed = std::env::var("SPAWNED_SIM_SEED")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let shared = Shared::new(seed);
        *ambient.borrow_mut() = Arc::downgrade(&shared);
        shared
    })
}

fn current() -> Option<Arc<Shared>> {
    CURRENT.with(|c| c.borrow().last().cloned())
}

fn current_or_ambient() -> Arc<Shared> {
    current().unwrap_or_else(ambient_or_new)
}

/// Guard returned by [`Handle::enter`] and [`Runtime::enter`].
pub struct EnterGuard {
    _private: (),
}
impl Drop for EnterGuard {
    fn drop(&mut self) {
        CURRENT.with(|c| c.borrow_mut().pop());
    }
}
fn enter(shared: Arc<Shared>) -> EnterGuard {
    CURRENT.with(|c| c.borrow_mut().push(shared));
    EnterGuard { _private: () }
}

// ---------------------------------------------------------------------------
// Runtime and Handle
// ---------------------------------------------------------------------------

/// Only `CurrentThread` exists under simulation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RuntimeFlavor {
    CurrentThread,
    MultiThread,
}

#[derive(Debug)]
pub struct TryCurrentError;
impl fmt::Display for TryCurrentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("no simulation is running on this thread")
    }
}
impl std::error::Error for TryCurrentError {}

/// A handle to this thread's simulated executor.
#[derive(Clone)]
pub struct Handle {
    shared: Arc<Shared>,
}
impl fmt::Debug for Handle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("sim::Handle").finish_non_exhaustive()
    }
}
impl Handle {
    pub fn current() -> Self {
        Self::try_current().expect("spawned_sim: Handle::current() outside the simulation")
    }
    /// Succeeds inside `block_on` or an `enter` guard, as Tokio's does.
    pub fn try_current() -> Result<Self, TryCurrentError> {
        current()
            .map(|shared| Self { shared })
            .ok_or(TryCurrentError)
    }
    pub fn spawn<F>(&self, future: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        spawn_on(&self.shared, future)
    }
    pub fn spawn_blocking<F, R>(&self, _f: F) -> JoinHandle<R>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        unsupported("Handle::spawn_blocking")
    }
    pub fn block_on<F: Future>(&self, future: F) -> F::Output {
        self.shared.block_on(future)
    }
    pub fn runtime_flavor(&self) -> RuntimeFlavor {
        RuntimeFlavor::CurrentThread
    }
    pub fn enter(&self) -> EnterGuard {
        enter(self.shared.clone())
    }
    pub fn metrics(&self) -> RuntimeMetrics {
        RuntimeMetrics(())
    }
}

/// Runtime metrics for the simulated executor.
#[derive(Clone, Debug)]
pub struct RuntimeMetrics(());
impl RuntimeMetrics {
    /// The simulation polls every task on one thread.
    pub fn num_workers(&self) -> usize {
        1
    }
}

/// Tokio's: every simulated poll runs with Tokio's cooperative budget.
pub use ::tokio::task::unconstrained;

/// A view of this thread's simulated executor. Every `Runtime` on one thread
/// shares one executor; the last one dropped drops the remaining tasks.
pub struct Runtime {
    handle: Handle,
}
impl Runtime {
    /// Joins this thread's simulation, creating it (seeded from
    /// `SPAWNED_SIM_SEED`, default 0) if none is alive.
    pub fn new() -> std::io::Result<Self> {
        Ok(Self::attach(ambient_or_new()))
    }
    /// Start a fresh simulation on this thread with `seed`, replacing any
    /// previous one as the thread's ambient simulation.
    pub fn with_seed(seed: u64) -> Self {
        let shared = Shared::new(seed);
        AMBIENT.with(|ambient| *ambient.borrow_mut() = Arc::downgrade(&shared));
        Self::attach(shared)
    }
    fn attach(shared: Arc<Shared>) -> Self {
        lock(&shared.state).runtimes += 1;
        Self {
            handle: Handle { shared },
        }
    }
    pub fn handle(&self) -> &Handle {
        &self.handle
    }
    pub fn block_on<F: Future>(&self, future: F) -> F::Output {
        self.handle.block_on(future)
    }
    pub fn spawn<F>(&self, future: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.handle.spawn(future)
    }
    pub fn enter(&self) -> EnterGuard {
        self.handle.enter()
    }
    /// Record every scheduling decision (task id, virtual time) from now on.
    pub fn record_trace(&self) {
        lock(&self.handle.shared.state).trace = Some(Vec::new());
    }
    /// The recorded scheduling decisions, if [`Runtime::record_trace`] was called.
    pub fn trace(&self) -> Vec<(u64, Duration)> {
        lock(&self.handle.shared.state)
            .trace
            .clone()
            .unwrap_or_default()
    }
    pub fn stats(&self) -> Stats {
        let state = lock(&self.handle.shared.state);
        Stats {
            seed: state.seed,
            steps: state.steps,
            schedule_hash: state.hash,
            now: state.now,
            tasks: state.tasks.len(),
        }
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        let last = {
            let mut state = lock(&self.handle.shared.state);
            state.runtimes -= 1;
            state.runtimes == 0 && !state.running
        };
        if last {
            self.handle.shared.drain();
        }
    }
}

/// Summary of one simulation run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    pub seed: u64,
    /// Scheduling decisions (polls) made so far.
    pub steps: u64,
    /// FNV hash over every (task id, virtual time) decision.
    pub schedule_hash: u64,
    /// Virtual time elapsed.
    pub now: Duration,
    /// Tasks still parked.
    pub tasks: usize,
}

/// The handle used by execution groups and the system control runtime: the
/// running simulation, else this thread's ambient one.
pub fn ambient_handle() -> Handle {
    Handle {
        shared: current_or_ambient(),
    }
}

/// Virtual time elapsed in this thread's simulation.
pub fn now() -> Duration {
    lock(&current_or_ambient().state).now
}

fn unsupported(call: &str) -> ! {
    panic!(
        "spawned_sim: {call} is not supported under the simulation backend \
         (it needs a real OS thread); use Backend::Async and async groups"
    )
}

// ---------------------------------------------------------------------------
// Tasks
// ---------------------------------------------------------------------------

/// Simulated task id, unique within one simulation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Id(u64);
impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

thread_local! {
    static TASK: std::cell::Cell<u64> = const { std::cell::Cell::new(ROOT) };
}

/// Id of the task being polled (0 for the `block_on` root).
pub fn task_id() -> Id {
    Id(TASK.with(|t| t.get()))
}

pub fn spawn<F>(future: F) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    spawn_on(&current_or_ambient(), future)
}

pub fn spawn_blocking<F, R>(_f: F) -> JoinHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    unsupported("spawn_blocking")
}

pub fn block_in_place<F, R>(_f: F) -> R
where
    F: FnOnce() -> R,
{
    unsupported("block_in_place")
}

fn spawn_on<F>(shared: &Arc<Shared>, future: F) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let id = shared.reserve_id();
    let join = Arc::new(JoinState {
        inner: Mutex::new(JoinInner {
            result: None,
            waker: None,
            aborted: false,
            finished: false,
            taken: false,
        }),
    });
    let task = TaskFuture {
        id,
        future: Some(Box::pin(future)),
        join: join.clone(),
    };
    shared.spawn_boxed(id, Box::pin(task));
    JoinHandle {
        id,
        join,
        shared: Arc::downgrade(shared),
    }
}

enum JoinErrorKind {
    Cancelled,
    // The payload is kept (as Tokio does) so callers can take ownership with
    // `into_panic` and dispose of it; the Mutex keeps `JoinError: Sync`.
    Panic(String, Mutex<Box<dyn std::any::Any + Send>>),
}

/// Why a simulated task did not return a value.
pub struct JoinError {
    id: u64,
    kind: JoinErrorKind,
}
impl JoinError {
    pub fn is_cancelled(&self) -> bool {
        matches!(self.kind, JoinErrorKind::Cancelled)
    }
    pub fn is_panic(&self) -> bool {
        matches!(self.kind, JoinErrorKind::Panic(..))
    }
    /// The panic payload; panics if the task was cancelled, as Tokio does.
    pub fn into_panic(self) -> Box<dyn std::any::Any + Send> {
        self.try_into_panic()
            .expect("`JoinError` reason is not a panic.")
    }
    pub fn try_into_panic(self) -> Result<Box<dyn std::any::Any + Send>, JoinError> {
        match self.kind {
            JoinErrorKind::Panic(_, payload) => {
                Ok(payload.into_inner().unwrap_or_else(|p| p.into_inner()))
            }
            kind => Err(JoinError { id: self.id, kind }),
        }
    }
}
impl fmt::Debug for JoinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            JoinErrorKind::Cancelled => write!(f, "JoinError::Cancelled({})", self.id),
            JoinErrorKind::Panic(message, _) => {
                write!(f, "JoinError::Panic({}, {message:?})", self.id)
            }
        }
    }
}
impl fmt::Display for JoinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            JoinErrorKind::Cancelled => write!(f, "task {} was cancelled", self.id),
            JoinErrorKind::Panic(message, _) => {
                write!(f, "task {} panicked with message {message:?}", self.id)
            }
        }
    }
}
impl std::error::Error for JoinError {}

struct JoinInner<T> {
    result: Option<Result<T, JoinError>>,
    waker: Option<Waker>,
    aborted: bool,
    finished: bool,
    /// The result was returned by a poll of the `JoinHandle`.
    taken: bool,
}
struct JoinState<T> {
    inner: Mutex<JoinInner<T>>,
}
impl<T> JoinState<T> {
    fn complete(&self, result: Result<T, JoinError>) {
        let waker = {
            let mut inner = lock(&self.inner);
            if inner.finished {
                return;
            }
            inner.finished = true;
            inner.result = Some(result);
            inner.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

pub struct JoinHandle<T> {
    id: u64,
    join: Arc<JoinState<T>>,
    shared: Weak<Shared>,
}
impl<T> fmt::Debug for JoinHandle<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("sim::JoinHandle")
            .field("id", &self.id)
            .finish()
    }
}
impl<T> JoinHandle<T> {
    pub fn abort(&self) {
        let finished = {
            let mut inner = lock(&self.join.inner);
            inner.aborted = true;
            inner.finished
        };
        if !finished {
            if let Some(shared) = self.shared.upgrade() {
                shared.schedule(self.id);
            }
        }
    }
    pub fn is_finished(&self) -> bool {
        lock(&self.join.inner).finished
    }
    pub fn id(&self) -> Id {
        Id(self.id)
    }
}
impl<T> Future for JoinHandle<T> {
    type Output = Result<T, JoinError>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut inner = lock(&self.join.inner);
        match inner.result.take() {
            Some(result) => {
                inner.taken = true;
                Poll::Ready(result)
            }
            None if inner.taken => {
                drop(inner);
                panic!("JoinHandle polled after completion")
            }
            None => {
                inner.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

struct TaskFuture<F: Future> {
    id: u64,
    future: Option<Pin<Box<F>>>,
    join: Arc<JoinState<F::Output>>,
}
fn panic_error(id: u64, payload: Box<dyn std::any::Any + Send>) -> JoinError {
    let message = payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "Box<dyn Any>".into());
    JoinError {
        id,
        kind: JoinErrorKind::Panic(message, Mutex::new(payload)),
    }
}

impl<F: Future> TaskFuture<F> {
    /// Drop the task's future inside a panic guard, as Tokio does: a panicking
    /// destructor becomes the task's `JoinError` instead of unwinding through
    /// the executor.
    fn drop_future(&mut self) -> Result<(), JoinError> {
        let Some(future) = self.future.take() else {
            return Ok(());
        };
        let previous = TASK.with(|t| t.replace(self.id));
        let dropped = catch_unwind(AssertUnwindSafe(move || drop(future)));
        TASK.with(|t| t.set(previous));
        dropped.map_err(|payload| panic_error(self.id, payload))
    }

    /// Cancel: drop the future; the joiner sees `Cancelled`, or the
    /// destructor's panic.
    fn cancel(&mut self) {
        let result = match self.drop_future() {
            Ok(()) => Err(JoinError {
                id: self.id,
                kind: JoinErrorKind::Cancelled,
            }),
            Err(panic) => Err(panic),
        };
        self.join.complete(result);
    }
}

impl<F: Future> Future for TaskFuture<F> {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let id = self.id;
        if lock(&self.join.inner).aborted {
            self.cancel();
            return Poll::Ready(());
        }
        let Some(future) = self.future.as_mut() else {
            return Poll::Ready(());
        };
        let previous = TASK.with(|t| t.replace(id));
        let polled = catch_unwind(AssertUnwindSafe(|| future.as_mut().poll(cx)));
        TASK.with(|t| t.set(previous));
        let result = match polled {
            Ok(Poll::Pending) => return Poll::Pending,
            // A destructor panic while dropping the finished future replaces
            // the value, as on Tokio.
            Ok(Poll::Ready(value)) => self.drop_future().map(|()| value),
            Err(payload) => {
                // The poll's panic wins over a second one from the destructor.
                let _ = self.drop_future();
                Err(panic_error(id, payload))
            }
        };
        self.join.complete(result);
        Poll::Ready(())
    }
}
impl<F: Future> Drop for TaskFuture<F> {
    fn drop(&mut self) {
        // Dropped unfinished (runtime teardown): wake the joiner as cancelled.
        self.cancel();
    }
}

// ---------------------------------------------------------------------------
// Virtual time
// ---------------------------------------------------------------------------

/// Completes once virtual time reaches its deadline.
pub struct Sleep {
    shared: Arc<Shared>,
    deadline: Duration,
    key: Option<(Duration, u64)>,
    yielded: bool,
}
impl fmt::Debug for Sleep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("sim::Sleep")
            .field("deadline", &self.deadline)
            .finish()
    }
}
impl Future for Sleep {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = &mut *self;
        let mut state = lock(&this.shared.state);
        if state.now >= this.deadline {
            if let Some(key) = this.key.take() {
                state.timers.remove(&key);
            } else if !this.yielded {
                // Already due at the first poll (`sleep(Duration::ZERO)`):
                // still a scheduling point, as Tokio's timer is.
                this.yielded = true;
                drop(state);
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
            return Poll::Ready(());
        }
        let key = match this.key {
            Some(key) => key,
            None => {
                let key = (this.deadline, state.next_timer);
                state.next_timer += 1;
                this.key = Some(key);
                key
            }
        };
        state.timers.insert(key, cx.waker().clone());
        Poll::Pending
    }
}
impl Drop for Sleep {
    fn drop(&mut self) {
        if let Some(key) = self.key.take() {
            lock(&self.shared.state).timers.remove(&key);
        }
    }
}

pub fn sleep(duration: Duration) -> Sleep {
    let shared = current_or_ambient();
    let deadline = lock(&shared.state).now.saturating_add(duration);
    Sleep {
        shared,
        deadline,
        key: None,
        yielded: false,
    }
}

/// The deadline of a [`timeout`] passed first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Elapsed(());
impl fmt::Display for Elapsed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("deadline has elapsed")
    }
}
impl std::error::Error for Elapsed {}

pub struct Timeout<F> {
    future: Pin<Box<F>>,
    sleep: Sleep,
}
impl<F: Future> Future for Timeout<F> {
    type Output = Result<F::Output, Elapsed>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Poll::Ready(value) = self.future.as_mut().poll(cx) {
            return Poll::Ready(Ok(value));
        }
        match Pin::new(&mut self.sleep).poll(cx) {
            Poll::Ready(()) => Poll::Ready(Err(Elapsed(()))),
            Poll::Pending => Poll::Pending,
        }
    }
}

pub fn timeout<F: Future>(duration: Duration, future: F) -> Timeout<F> {
    Timeout {
        future: Box::pin(future),
        sleep: sleep(duration),
    }
}

/// Yield once to the simulated scheduler.
pub async fn yield_now() {
    let mut yielded = false;
    std::future::poll_fn(|cx| {
        if yielded {
            Poll::Ready(())
        } else {
            yielded = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    })
    .await
}

/// Never completes: there are no signals in a simulation.
pub async fn ctrl_c() {
    std::future::pending::<()>().await
}
