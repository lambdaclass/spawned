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
//!   Work handed to Tokio directly (`tokio::spawn`, `tokio::task::spawn_blocking`)
//!   is a determinism leak: it fails the run, including when the root
//!   returns right after it, and the escaped work is shut down with the
//!   world's Tokio runtime.
//! - **Virtual time.** `sleep` and `timeout` use a virtual clock that
//!   advances when no task is ready, jumping to the earliest pending timer.
//!   A sleep whose deadline has already passed still yields once, as Tokio's
//!   timer does.
//!   A busy run (tasks that keep yielding) creeps it forward by 1 ms every
//!   `SPAWNED_SIM_SPIN` polls (default 10,000), so spin-waits still time out;
//!   every 100 creeps without an idle moment the poll threshold halves (down
//!   to one poll per millisecond), so a spin-wait reaches a 5 s timeout in
//!   about two million polls, well before the livelock guard
//!   (`SPAWNED_SIM_MAX_STEPS`, default 10 million), even if other timers keep
//!   firing.
//!   `spawned_rt::time::Instant` reads this clock.
//! - **Detected limits.** `spawn_blocking`, `block_in_place` and a nested
//!   `block_on` panic with a message naming the call; a run with no ready task
//!   and no pending timer panics as a deadlock instead of hanging. A single
//!   poll that runs longer than `SPAWNED_SIM_POLL_TIMEOUT` wall-clock seconds
//!   (default 60; `SPAWNED_SIM_POLL_TIMEOUT=0` disables it, and the per-poll
//!   clock read with it) aborts the process with a message naming the task
//!   and seed: a loop with no await point cannot be interrupted on one
//!   thread. It is wall-clock based, so a debugger pause or a badly
//!   overloaded machine can trip it; it never changes a schedule.
//!
//! Channels, `watch`, `CancellationToken` and the stream wrappers stay Tokio's:
//! they only use wakers, so they are deterministic under this executor.
//!
//! - **Decision tape.** Every pick is recorded as `c` (canonical: smallest
//!   ready id) or `p <id>`, and every clock jump as `t <nanos>`. Replaying a
//!   [`Tape`] picks by task id rather than by generator draw, so a schedule
//!   survives code changes that shift random-number consumption. Replay is
//!   strict ([`ReplayMode::Strict`]): clock jumps come from the tape too, and
//!   any decision that does not apply, or a tape that runs out or is left
//!   over, fails the run as `replay diverged` instead of silently running a
//!   different schedule. [`shrink`] moves a failing tape toward the canonical
//!   schedule, guiding candidates permissively and keeping only those whose
//!   recorded tape reproduces strictly.
//! - **Exploration.** [`explore`] swaps the random walk for systematic
//!   strategies (PCT, bounded DFS over delays) that record into the same tape.
//!
//! Library code reads time and jitter entropy through `spawned_rt::time`,
//! which this module backs under the cfg. The one scheduling-relevant
//! `HashMap` walk in the library (waking monitor waiters on exit) is done in
//! monitor-creation order under the cfg. Still outside its control: direct
//! `std::time` / `SystemTime` reads, `std::thread`, Tokio I/O, and
//! process-global id counters (ids differ between runs in one process, but
//! their relative order within a run is fixed).
//!
//! Fidelity limits: everything a task does inside one poll is atomic (no OS
//! preemption, no weak-memory reorderings); timers fire only when no task is
//! ready (or after the spin limit), not at wall-clock moments.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::future::Future;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::task::{Context, Poll, Wake, Waker};
use std::thread::ThreadId;
use std::time::Duration;

type BoxTask = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

const ROOT: u64 = 0;
const DEFAULT_MAX_STEPS: u64 = 10_000_000;
/// Polls without the clock moving after which virtual time creeps forward by
/// [`SPIN_QUANTUM`] even though tasks are still ready, so a task that keeps
/// yielding cannot starve timers forever.
const DEFAULT_SPIN_LIMIT: u64 = 10_000;
/// How far the clock creeps after [`DEFAULT_SPIN_LIMIT`] busy polls.
const SPIN_QUANTUM: Duration = Duration::from_millis(1);
/// Creeps in a row (with no idle moment in between) after which the busy-poll
/// threshold halves, down to one poll per quantum. A spin-wait therefore
/// reaches a 5 s timeout in about two million polls, whatever other timers
/// fire meanwhile, instead of the 50 million that 1 ms per 10,000 polls would
/// need (past the livelock guard).
const CREEPS_PER_HALVING: u64 = 100;
/// Wall-clock seconds one poll may run before the watchdog aborts the process.
const DEFAULT_POLL_TIMEOUT_SECS: u64 = 60;
/// Wall-clock milliseconds one actor turn may run inside a single poll
/// before the run fails (`SPAWNED_SIM_TURN_BUDGET`, 0 disables).
const DEFAULT_TURN_BUDGET_MS: u64 = 250;

pub mod explore;

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
        /// This world's limit in seconds (`SPAWNED_SIM_POLL_TIMEOUT` when it
        /// was created); 0 disables the watch and its per-poll clock read.
        limit: u64,
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
                limit: limit(),
                since: AtomicU64::new(0),
                task: AtomicU64::new(0),
            });
            if watch.limit > 0 {
                static STARTED: OnceLock<()> = OnceLock::new();
                STARTED.get_or_init(|| {
                    base();
                    std::thread::Builder::new()
                        .name("spawned-sim-watchdog".into())
                        .spawn(run)
                        .expect("spawned_sim: cannot start the poll watchdog");
                });
                let mut watched = lock(&WATCHED);
                watched.retain(|w| w.strong_count() > 0);
                watched.push(Arc::downgrade(&watch));
            }
            watch
        }

        pub(super) fn busy(&self, task: u64) -> Busy<'_> {
            if self.limit > 0 {
                self.task.store(task, Ordering::Relaxed);
                let now = base().elapsed().as_millis() as u64;
                self.since.store(now.max(1), Ordering::Release);
            }
            Busy(self)
        }
    }

    pub(super) struct Busy<'a>(&'a Watch);
    impl Drop for Busy<'_> {
        fn drop(&mut self) {
            if self.0.limit > 0 {
                self.0.since.store(0, Ordering::Release);
            }
        }
    }

    fn run() {
        loop {
            std::thread::sleep(Duration::from_millis(200));
            let now = base().elapsed().as_millis() as u64;
            for watch in lock(&WATCHED).iter().filter_map(Weak::upgrade) {
                let since = watch.since.load(Ordering::Acquire);
                let task = watch.task.load(Ordering::Relaxed);
                // Still the same poll (a new one would have moved `since`), so
                // `task` names it.
                let same = watch.since.load(Ordering::Acquire) == since;
                if since != 0 && same && now.saturating_sub(since) > watch.limit * 1000 {
                    eprintln!(
                        "spawned_sim: task {task} has been inside one poll for over {} s \
                         (seed {}): a loop with no await point never returns to the \
                         scheduler; aborting (SPAWNED_SIM_POLL_TIMEOUT sets the limit, \
                         0 disables the watchdog)",
                        watch.limit, watch.seed
                    );
                    std::process::abort();
                }
            }
        }
    }
}

/// Actor turn budget: fails a run when one actor turn runs longer than the
/// budget inside a single poll (a blocking call or a loop with no `.await`).
///
/// The actor dispatch loop marks each turn with `sim::turn`; the
/// executor keeps the label of a turn suspended at an `.await` per task and
/// times only the stretches of a poll that run inside a turn, so a poll that
/// handles several messages charges each turn separately and time between
/// turns (the mailbox, the test body) is never charged.
///
/// Wall-clock, like the poll watchdog: it can only see a stretch after it
/// returns (one thread cannot preempt it), and its timing is not
/// deterministic. What it measures is still a property of the code, so the
/// default is generous and a failure replays with the same seed or tape.
mod turn {
    use std::cell::RefCell;
    use std::time::{Duration, Instant};

    /// The actor and message type of a turn.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) struct Label {
        pub actor: &'static str,
        pub message: &'static str,
    }

    /// The first stretch of a turn that ran past the budget.
    #[derive(Debug, Clone, Copy)]
    pub(super) struct Overrun {
        pub label: Label,
        pub took: Duration,
    }

    /// Timing of the poll in progress on this thread.
    pub(super) struct Poll {
        budget: Duration,
        turn: Option<Label>,
        since: Instant,
        overrun: Option<Overrun>,
    }

    thread_local! {
        /// `None` outside a timed poll: then [`begin`] does nothing (no
        /// clock read), for example with the budget disabled.
        static POLL: RefCell<Option<Poll>> = const { RefCell::new(None) };
    }

    impl Poll {
        /// Charge the stretch since the last boundary to the current turn.
        fn close(&mut self) {
            let now = Instant::now();
            if let Some(label) = self.turn {
                let took = now.duration_since(self.since);
                if took > self.budget && self.overrun.is_none() {
                    self.overrun = Some(Overrun { label, took });
                }
            }
            self.since = now;
        }
    }

    /// Start timing a poll that resumes `turn` (a turn suspended at an
    /// `.await`, or `None`). Returns the enclosing poll's timing (another
    /// world polled from inside this one), which [`leave`] restores.
    pub(super) fn enter(budget: Duration, turn: Option<Label>) -> Option<Poll> {
        let poll = Poll {
            budget,
            turn,
            since: Instant::now(),
            overrun: None,
        };
        POLL.with(|p| p.borrow_mut().replace(poll))
    }

    /// Stop timing the poll: returns the turn still open (suspended at an
    /// `.await`) and the first overrun, if any.
    pub(super) fn leave(outer: Option<Poll>) -> (Option<Label>, Option<Overrun>) {
        let poll = POLL.with(|p| std::mem::replace(&mut *p.borrow_mut(), outer));
        match poll {
            Some(mut poll) => {
                poll.close();
                (poll.turn, poll.overrun)
            }
            None => (None, None),
        }
    }

    /// Mark the start (`Some`) or end (`None`) of a turn.
    pub(super) fn mark(turn: Option<Label>) -> bool {
        POLL.with(|p| match p.borrow_mut().as_mut() {
            Some(poll) => {
                poll.close();
                poll.turn = turn;
                true
            }
            None => false,
        })
    }
}

/// Marks one actor turn for the turn budget (`SPAWNED_SIM_TURN_BUDGET`):
/// hold it while the handler for one message runs. A no-op outside a
/// simulated poll or with the budget disabled.
#[doc(hidden)]
#[must_use = "the turn ends when the guard is dropped"]
pub fn turn(actor: &'static str, message: &'static str) -> TurnGuard {
    TurnGuard(turn::mark(Some(turn::Label { actor, message })))
}

/// Ends the turn started by [`turn`] when dropped.
#[doc(hidden)]
pub struct TurnGuard(bool);
impl Drop for TurnGuard {
    fn drop(&mut self) {
        if self.0 {
            turn::mark(None);
        }
    }
}

fn turn_budget() -> Option<Duration> {
    parse_turn_budget(std::env::var("SPAWNED_SIM_TURN_BUDGET").ok().as_deref())
}

/// `SPAWNED_SIM_TURN_BUDGET` in milliseconds: unset or unparsable is the
/// default, 0 disables the check.
fn parse_turn_budget(value: Option<&str>) -> Option<Duration> {
    let ms = value
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(DEFAULT_TURN_BUDGET_MS);
    (ms > 0).then(|| Duration::from_millis(ms))
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
    spin: u64,
    /// Busy polls after which the clock creeps (`SPAWNED_SIM_SPIN`).
    spin_limit: u64,
    /// Creeps since the last idle moment (no task ready).
    streak: u64,
    hash: u64,
    trace: Option<Vec<(u64, Duration)>>,
    tape: Option<Vec<Decision>>,
    script: Option<Script>,
    plan: Option<explore::Plan>,
    entropy: u64,
    foreign: Option<String>,
    running: bool,
    runtimes: usize,
    /// Seed of the per-world Tokio runtime's RNG (`select!`, `watch`).
    tokio_seed: u64,
    /// Wall-clock budget of one actor turn inside a poll
    /// (`SPAWNED_SIM_TURN_BUDGET`); `None` disables the check.
    turn_budget: Option<Duration>,
    /// Actor turns suspended at an `.await`, by task.
    turns: BTreeMap<u64, turn::Label>,
    /// The first turn that ran past the budget; fails the run.
    overrun: Option<String>,
}

/// One scheduling decision of a simulation run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Polled the ready task with the smallest id (the canonical choice);
    /// recorded with that id, which a strict replay checks.
    Canonical(Option<u64>),
    /// Polled this task, which was not the canonical choice.
    Poll(u64),
    /// Virtual time advanced to this instant and the timers due by then fired:
    /// a jump to the earliest timer when nothing was ready, or a creep of one
    /// quantum (possibly firing nothing) after the busy-poll threshold.
    Fire(Duration),
}

/// How [`Runtime::replay`] follows a tape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayMode {
    /// Reproduce the recorded run exactly: every pick must name a ready task,
    /// clock jumps come from the tape (so the spin limit of the replaying
    /// process does not matter), and a run that needs a decision the tape
    /// does not have, or finishes with decisions left over, fails with a
    /// `replay diverged` panic.
    Strict,
    /// Guide a run with a partial or edited tape (the shrinker's candidates):
    /// a scripted task that is not ready, and every poll past the tape's end,
    /// takes the canonical choice; clock jumps follow the executor's own
    /// policy with the tape's spin limit. Never use it to claim a
    /// reproduction: re-check the recorded tape in [`ReplayMode::Strict`].
    Permissive,
}

/// Replay script: the tape being followed and how far it has been read.
struct Script {
    decisions: Vec<Decision>,
    pos: usize,
    mode: ReplayMode,
    misses: u64,
}

/// What the executor does next.
enum Action {
    /// Poll the ready task at this index.
    Poll(usize),
    /// Advance the clock to this deadline and fire the timers due by then.
    Fire(Duration),
}

/// Strict replay: the next action is the tape's next decision, which must
/// apply to the current ready set. `Ok(None)` is a deadlock that the recorded
/// run reached too (the tape ended with nothing ready and no timer).
fn scripted(state: &mut State) -> Result<Option<Action>, String> {
    let State {
        script,
        ready,
        timers,
        now,
        ..
    } = state;
    let now = *now;
    let script = script.as_mut().expect("strict replay without a script");
    let at = script.pos;
    let mut ids = ready.clone();
    ids.sort_unstable();
    let action = match script.decisions.get(at).copied() {
        None if ready.is_empty() && timers.is_empty() => return Ok(None),
        None => {
            return Err(format!(
                "the tape ended after {at} decisions but the run continues (ready: {ids:?})"
            ))
        }
        Some(Decision::Fire(deadline)) => {
            let earliest = timers.keys().next().map(|(at, _)| *at);
            match earliest {
                None => {
                    return Err(format!(
                        "decision {at} advances the clock to {deadline:?}, but no timer is pending"
                    ))
                }
                Some(_) if deadline < now => {
                    return Err(format!(
                        "decision {at} moves the clock back to {deadline:?} from {now:?}"
                    ))
                }
                Some(earliest) if deadline > earliest => {
                    return Err(format!(
                        "decision {at} advances the clock to {deadline:?}, past the earliest \
                         pending timer at {earliest:?}"
                    ))
                }
                Some(earliest) if ready.is_empty() && deadline != earliest => {
                    return Err(format!(
                        "decision {at} creeps the clock to {deadline:?} while no task is \
                         ready; the recorded run would have jumped to {earliest:?}"
                    ))
                }
                Some(_) => Action::Fire(deadline),
            }
        }
        Some(_) if ready.is_empty() => {
            return Err(format!(
                "decision {at} polls a task but none is ready (the recorded run had one ready)"
            ))
        }
        Some(Decision::Canonical(id)) => {
            let index = canonical(ready);
            match id {
                Some(id) if ready[index] != id => {
                    return Err(format!(
                        "decision {at} polls task {id} as the canonical choice, but the ready \
                         tasks are {ids:?}"
                    ))
                }
                _ => Action::Poll(index),
            }
        }
        Some(Decision::Poll(id)) => match ready.iter().position(|r| *r == id) {
            Some(index) if index != canonical(ready) => Action::Poll(index),
            _ => {
                return Err(format!(
                    "decision {at} polls task {id} as a non-canonical choice, but the ready \
                     tasks are {ids:?}"
                ))
            }
        },
    };
    script.pos += 1;
    Ok(Some(action))
}

/// Index of the ready task with the smallest id.
fn canonical(ready: &[u64]) -> usize {
    ready
        .iter()
        .enumerate()
        .min_by_key(|(_, id)| **id)
        .map_or(0, |(index, _)| index)
}

/// Pick the index of the next ready task: from a permissive replay script
/// while it lasts (canonical when a scripted task is not ready, or past its
/// end; its clock jumps are skipped), otherwise from the exploration plan if
/// one is set, otherwise from the seeded generator.
fn choose(state: &mut State) -> usize {
    let State {
        script,
        plan,
        ready,
        rng,
        ..
    } = state;
    match script {
        Some(script) => {
            while let Some(Decision::Fire(_)) = script.decisions.get(script.pos) {
                script.pos += 1;
            }
            let pick = script.decisions.get(script.pos).copied();
            script.pos += 1;
            match pick {
                Some(Decision::Poll(id)) => {
                    ready.iter().position(|r| *r == id).unwrap_or_else(|| {
                        script.misses += 1;
                        canonical(ready)
                    })
                }
                _ => canonical(ready),
            }
        }
        None => match plan {
            Some(plan) => plan.pick(ready),
            None => by_rank(ready, (splitmix(rng) % ready.len() as u64) as usize),
        },
    }
}

struct Shared {
    owner: ThreadId,
    max_steps: u64,
    state: Mutex<State>,
    /// Driverless Tokio runtime every poll runs inside (see the module docs),
    /// built on the first poll so a replay can still choose its seed.
    tokio: Mutex<Option<Arc<::tokio::runtime::Runtime>>>,
    watch: Arc<watchdog::Watch>,
}

impl Drop for Shared {
    fn drop(&mut self) {
        self.shutdown_tokio();
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
            tokio: Mutex::new(None),
            watch: watchdog::Watch::register(seed),
            state: Mutex::new(State {
                spin_limit: std::env::var("SPAWNED_SIM_SPIN")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(DEFAULT_SPIN_LIMIT),
                streak: 0,
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
                spin: 0,
                hash: 0xcbf2_9ce4_8422_2325,
                trace: None,
                tape: None,
                script: None,
                plan: None,
                entropy: 0,
                foreign: None,
                running: false,
                runtimes: 0,
                tokio_seed: seed,
                turn_budget: turn_budget(),
                turns: BTreeMap::new(),
                overrun: None,
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
        let runtime = self.tokio_runtime();
        let spawned = runtime.metrics().spawned_tasks_count();
        // Work handed to Tokio directly runs outside the simulated schedule:
        // a `tokio::spawn` (async task) or `tokio::task::spawn_blocking` (an OS
        // thread from Tokio's blocking pool).
        let escaped = |runtime: &::tokio::runtime::Runtime| {
            let metrics = runtime.metrics();
            metrics.spawned_tasks_count() != spawned
                || metrics.num_blocking_threads() > 0
                || metrics.blocking_queue_depth() > 0
        };
        // Checked even if the poll unwinds, so the escaped work is shut down.
        struct Check<'a, E: Fn(&::tokio::runtime::Runtime) -> bool> {
            shared: &'a Shared,
            runtime: &'a ::tokio::runtime::Runtime,
            escaped: E,
            id: u64,
        }
        impl<E: Fn(&::tokio::runtime::Runtime) -> bool> Drop for Check<'_, E> {
            fn drop(&mut self) {
                if (self.escaped)(self.runtime) {
                    let mut state = lock(&self.shared.state);
                    if state.foreign.is_none() {
                        state.foreign = Some(format!(
                            "task {} handed work to Tokio directly (tokio::spawn or \
                             tokio::task::spawn_blocking); use spawned_rt::tasks::spawn",
                            self.id
                        ));
                    }
                }
            }
        }
        let _check = Check {
            shared: self,
            runtime: &runtime,
            escaped,
            id,
        };
        let mut f = Some(f);
        let mut out = None;
        let _busy = self.watch.busy(id);
        runtime.block_on(std::future::poll_fn(|cx| match f.take() {
            Some(f) => {
                out = Some(f());
                if escaped(&runtime) {
                    // Return before Tokio's scheduler tick could run it.
                    Poll::Ready(())
                } else {
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            }
            None => Poll::Ready(()),
        }));
        out.expect("spawned_sim: poll did not run")
    }

    /// This world's Tokio runtime, built on first use with `tokio_seed`.
    fn tokio_runtime(&self) -> Arc<::tokio::runtime::Runtime> {
        let seed = lock(&self.state).tokio_seed;
        lock(&self.tokio)
            .get_or_insert_with(|| {
                Arc::new(
                    ::tokio::runtime::Builder::new_current_thread()
                        .rng_seed(::tokio::runtime::RngSeed::from_bytes(&seed.to_le_bytes()))
                        .build()
                        .expect("spawned_sim: cannot build the per-world Tokio runtime"),
                )
            })
            .clone()
    }

    /// Drop this world's Tokio runtime and any work that escaped onto it
    /// (which may hold this world alive). Safe inside another world's poll.
    fn shutdown_tokio(&self) {
        let runtime = lock(&self.tokio).take();
        if let Some(runtime) = runtime {
            if let Ok(runtime) = Arc::try_unwrap(runtime) {
                runtime.shutdown_background();
            }
        }
    }

    /// Strict replay: a run that finishes with tape decisions left over did
    /// not reproduce the recording.
    fn check_tape_used(&self) {
        let left = {
            let state = lock(&self.state);
            match &state.script {
                Some(script) if script.mode == ReplayMode::Strict => {
                    script.decisions.len().saturating_sub(script.pos)
                }
                _ => 0,
            }
        };
        if left > 0 {
            panic!(
                "spawned_sim: replay diverged: the run finished with {left} tape decisions unused"
            );
        }
    }

    /// Fail the run if a determinism leak was recorded.
    fn check_leaks(&self) {
        let foreign = lock(&self.state).foreign.take();
        if let Some(foreign) = foreign {
            self.shutdown_tokio();
            panic!("spawned_sim: determinism leak: {foreign}");
        }
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
            self.check_leaks();
            self.check_turns();
            let mut state = lock(&self.state);
            let strict = matches!(&state.script, Some(s) if s.mode == ReplayMode::Strict);
            let action = if strict {
                match scripted(&mut state) {
                    Ok(action) => action,
                    Err(reason) => {
                        let (seed, now) = (state.seed, state.now);
                        drop(state);
                        panic!(
                            "spawned_sim: replay diverged: {reason} (seed {seed}, virtual time {now:?})"
                        );
                    }
                }
            } else {
                // The busy-poll threshold halves every CREEPS_PER_HALVING
                // creeps without an idle moment, down to one poll.
                let halvings = (state.streak / CREEPS_PER_HALVING).min(63) as u32;
                let threshold = (state.spin_limit >> halvings).max(1);
                let starving = state.spin >= threshold && !state.timers.is_empty();
                if !state.ready.is_empty() && !starving {
                    Some(Action::Poll(choose(&mut state)))
                } else if let Some(&(earliest, _)) = state.timers.keys().next() {
                    // Idle: jump to the earliest timer. Starving (tasks still
                    // ready): creep forward by one quantum, as wall time would
                    // under a spin.
                    Some(Action::Fire(if starving {
                        state.streak += 1;
                        earliest.min(state.now + SPIN_QUANTUM)
                    } else {
                        state.streak = 0;
                        earliest
                    }))
                } else {
                    None
                }
            };
            match action? {
                Action::Poll(index) => {
                    state.spin += 1;
                    let canonical = index == canonical(&state.ready);
                    let id = state.ready.swap_remove(index);
                    if let Some(tape) = &mut state.tape {
                        tape.push(if canonical {
                            Decision::Canonical(Some(id))
                        } else {
                            Decision::Poll(id)
                        });
                    }
                    state.queued.remove(&id);
                    state.steps += 1;
                    let now = state.now;
                    state.hash = (state.hash ^ id).wrapping_mul(0x0100_0000_01b3);
                    state.hash =
                        (state.hash ^ now.as_nanos() as u64).wrapping_mul(0x0100_0000_01b3);
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
                Action::Fire(deadline) => {
                    state.spin = 0;
                    state.now = state.now.max(deadline);
                    if let Some(tape) = &mut state.tape {
                        tape.push(Decision::Fire(deadline));
                    }
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
        }
    }

    fn poll_task(self: &Arc<Self>, id: u64) {
        let (slot, budget, resumed) = {
            let state = lock(&self.state);
            let Some(slot) = state.tasks.get(&id).cloned() else {
                return;
            };
            (slot, state.turn_budget, state.turns.get(&id).copied())
        };
        let Some(mut task) = lock(&slot).take() else {
            return;
        };
        let waker = Waker::from(Arc::new(TaskWaker {
            id,
            shared: Arc::downgrade(self),
        }));
        // Ends the turn timing even if the poll unwinds.
        struct Timed<'a> {
            shared: &'a Shared,
            id: u64,
            outer: Option<Option<turn::Poll>>,
        }
        impl Drop for Timed<'_> {
            fn drop(&mut self) {
                if let Some(outer) = self.outer.take() {
                    self.shared.end_turn_timing(self.id, outer);
                }
            }
        }
        let timed = Timed {
            shared: self,
            id,
            outer: budget.map(|budget| turn::enter(budget, resumed)),
        };
        let done = self.run_poll(id, || {
            task.as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_ready()
        });
        drop(timed);
        if done {
            self.retire(id);
            drop(task);
        } else {
            *lock(&slot) = Some(task);
        }
    }

    /// Record the turn task `id` left suspended at an `.await` and the
    /// first turn of this poll that ran past the budget.
    fn end_turn_timing(&self, id: u64, outer: Option<turn::Poll>) {
        let (open, overrun) = turn::leave(outer);
        let mut state = lock(&self.state);
        match open {
            Some(label) => state.turns.insert(id, label),
            None => state.turns.remove(&id),
        };
        if let (Some(overrun), None, Some(budget)) =
            (overrun, &state.overrun, state.turn_budget)
        {
            let turn::Overrun { label, took } = overrun;
            // The measured time goes to stderr, not into the failure: the
            // shrinker matches failures by their text.
            eprintln!(
                "spawned_sim: actor `{}` handling `{}` ran {} ms in one poll (task {id}, seed {})",
                label.actor,
                label.message,
                took.as_millis(),
                state.seed
            );
            state.overrun = Some(format!(
                "turn over budget: actor `{}` handling `{}` ran for more than {} ms in one \
                 poll without yielding (a blocking call or a loop with no .await stalls every \
                 actor on its worker; SPAWNED_SIM_TURN_BUDGET=ms sets the budget, 0 disables it)",
                label.actor,
                label.message,
                budget.as_millis()
            ));
        }
    }

    /// Fail the run if an actor turn ran past the budget.
    fn check_turns(&self) {
        let overrun = lock(&self.state).overrun.take();
        if let Some(overrun) = overrun {
            panic!("spawned_sim: {overrun}");
        }
    }

    /// Forget a finished task, including a wakeup it gave itself during its
    /// final poll (otherwise a dead id stays in the ready list).
    fn retire(&self, id: u64) {
        let mut state = lock(&self.state);
        state.tasks.remove(&id);
        state.turns.remove(&id);
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
        // Boxed in an Option so it can be dropped before ROOT is retired: a
        // destructor that wakes ROOT must not leave it in the ready list.
        let mut future = Some(Box::pin(future));
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
                    let root = future.as_mut().expect("root polled after completion");
                    match root.as_mut().poll(&mut Context::from_waker(&waker)) {
                        Poll::Ready(output) => {
                            future = None;
                            Poll::Ready(output)
                        }
                        Poll::Pending => Poll::Pending,
                    }
                });
                if let Poll::Ready(output) = polled {
                    self.retire(ROOT);
                    // Work that escaped to Tokio during the final poll fails
                    // the run instead of being accepted.
                    self.check_leaks();
                    self.check_tape_used();
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
        // Tokio tasks that escaped may hold this world (a cycle the task
        // table does not see): shutting the runtime down drops them.
        self.shutdown_tokio();
        let mut state = lock(&self.state);
        state.ready.clear();
        state.queued.clear();
        state.timers.clear();
        state.turns.clear();
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
    /// Record every scheduling decision as a [`Tape`] from now on.
    pub fn record_tape(&self) {
        lock(&self.handle.shared.state).tape = Some(Vec::new());
    }
    /// The decisions recorded since [`Runtime::record_tape`].
    pub fn tape(&self) -> Tape {
        let state = lock(&self.handle.shared.state);
        Tape {
            seed: state.seed,
            spin: Some(state.spin_limit),
            decisions: state.tape.clone().unwrap_or_default(),
        }
    }
    /// Reproduce the run recorded in `tape` exactly ([`ReplayMode::Strict`]):
    /// call before the first `block_on`. Tokio's RNG is seeded from the tape's
    /// seed, and any departure from the tape fails the run with a
    /// `spawned_sim: replay diverged` panic.
    pub fn replay(&self, tape: &Tape) {
        self.replay_with(tape, ReplayMode::Strict);
    }
    /// Follow `tape` in the given mode; see [`ReplayMode`].
    pub fn replay_with(&self, tape: &Tape, mode: ReplayMode) {
        let mut state = lock(&self.handle.shared.state);
        if state.tokio_seed != tape.seed {
            state.tokio_seed = tape.seed;
            drop(state);
            // A Tokio runtime already built (this world ran before) carries
            // the old seed's RNG: rebuild it on the next poll.
            self.handle.shared.shutdown_tokio();
            state = lock(&self.handle.shared.state);
        }
        if let Some(spin) = tape.spin {
            state.spin_limit = spin;
        }
        state.script = Some(Script {
            decisions: tape.decisions.clone(),
            pos: 0,
            mode,
            misses: 0,
        });
    }
    /// Busy polls after which virtual time creeps (`SPAWNED_SIM_SPIN`); a
    /// replayed tape sets the recorded value.
    pub fn set_spin_limit(&self, polls: u64) {
        lock(&self.handle.shared.state).spin_limit = polls.max(1);
    }
    /// Wall-clock budget of one actor turn inside a poll
    /// (`SPAWNED_SIM_TURN_BUDGET`, default 250 ms); `None` disables the check.
    pub fn set_turn_budget(&self, budget: Option<Duration>) {
        lock(&self.handle.shared.state).turn_budget = budget.filter(|b| !b.is_zero());
    }
    pub fn stats(&self) -> Stats {
        self.handle.shared.stats()
    }
}
impl Shared {
    fn stats(&self) -> Stats {
        let state = lock(&self.state);
        Stats {
            seed: state.seed,
            steps: state.steps,
            schedule_hash: state.hash,
            now: state.now,
            tasks: state.tasks.len(),
            ready: state.ready.len(),
            tape_misses: state.script.as_ref().map_or(0, |s| s.misses),
            tape_left: state
                .script
                .as_ref()
                .map_or(0, |s| s.decisions.len().saturating_sub(s.pos)),
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
    /// Tasks waiting to be polled (0 between `block_on` calls unless a
    /// spawned task is runnable).
    pub ready: usize,
    /// Permissive replay only: scripted picks whose task was not ready.
    pub tape_misses: u64,
    /// Replay only: tape decisions not (yet) used.
    pub tape_left: usize,
}

/// The handle used by execution groups and the system control runtime: the
/// running simulation, else this thread's ambient one.
pub fn ambient_handle() -> Handle {
    Handle {
        shared: current_or_ambient(),
    }
}

/// [`Runtime::stats`] of the running simulation, for bodies that do not own
/// the `Runtime` (those run by [`run_once`] or [`explore`]); the scenario
/// fuzzer reads the task count from it to check for leaked tasks.
pub fn stats() -> Stats {
    current_or_ambient().stats()
}

/// [`Runtime::set_turn_budget`] for the running simulation, for bodies that
/// do not own the `Runtime` (those run by [`run_once`] or [`explore`]).
pub fn set_turn_budget(budget: Option<Duration>) {
    lock(&current_or_ambient().state).turn_budget = budget.filter(|b| !b.is_zero());
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
            detached: false,
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
    /// The `JoinHandle` was dropped: nobody will read the result.
    detached: bool,
}

/// Drop a task's unread result, containing a panicking destructor (as Tokio
/// does) so it cannot unwind through the executor or a `JoinHandle` drop.
fn discard<T>(result: Option<Result<T, JoinError>>) {
    let _ = catch_unwind(AssertUnwindSafe(move || drop(result)));
}
struct JoinState<T> {
    inner: Mutex<JoinInner<T>>,
}
impl<T> JoinState<T> {
    fn complete(&self, result: Result<T, JoinError>) {
        let waker = {
            let mut inner = lock(&self.inner);
            if inner.finished {
                drop(inner);
                discard(Some(result));
                return;
            }
            inner.finished = true;
            if inner.detached {
                drop(inner);
                discard(Some(result));
                return;
            }
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
impl<T> Drop for JoinHandle<T> {
    fn drop(&mut self) {
        let result = {
            let mut inner = lock(&self.join.inner);
            inner.detached = true;
            inner.result.take()
        };
        discard(result);
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
            // Aborted during its own poll: cancelled right after it, as on
            // Tokio, not at its next turn.
            Ok(Poll::Pending) if lock(&self.join.inner).aborted => {
                self.cancel();
                return Poll::Ready(());
            }
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

// ---------------------------------------------------------------------------
// Decision tape
// ---------------------------------------------------------------------------

/// Every scheduling decision of one run. Replaying a tape picks tasks by id,
/// not by generator draws, so a schedule survives code changes that shift how
/// many random numbers a run consumes.
///
/// Text form (`# spawned-sim tape v2`): a header with `seed <n>` and
/// `spin <polls>` (the spin limit the run used), then one decision per line:
/// `c` (canonical: smallest ready id), `p <task id>`, `t <virtual nanos>`
/// (the clock jumped there and fired the timers due). A strict replay
/// follows every line, clock jumps included. Version 1 tapes (no `spin`)
/// still load.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tape {
    pub seed: u64,
    /// Spin limit of the recorded run (`SPAWNED_SIM_SPIN`); `None` for a v1
    /// tape, which replays with the replaying process's limit.
    pub spin: Option<u64>,
    pub decisions: Vec<Decision>,
}

impl Tape {
    /// Polls that were not the canonical choice.
    pub fn deviations(&self) -> usize {
        self.decisions
            .iter()
            .filter(|d| matches!(d, Decision::Poll(_)))
            .count()
    }
    /// Number of polls.
    pub fn polls(&self) -> usize {
        self.decisions
            .iter()
            .filter(|d| !matches!(d, Decision::Fire(_)))
            .count()
    }
    fn picks(&self) -> Vec<Option<u64>> {
        self.decisions
            .iter()
            .filter_map(|d| match d {
                Decision::Canonical(_) => Some(None),
                Decision::Poll(id) => Some(Some(*id)),
                Decision::Fire(_) => None,
            })
            .collect()
    }
    pub fn to_text(&self) -> String {
        let mut out = format!("# spawned-sim tape v2\nseed {}\n", self.seed);
        if let Some(spin) = self.spin {
            out.push_str(&format!("spin {spin}\n"));
        }
        for d in &self.decisions {
            match d {
                Decision::Canonical(Some(id)) => out.push_str(&format!("c {id}\n")),
                Decision::Canonical(None) => out.push_str("c\n"),
                Decision::Poll(id) => out.push_str(&format!("p {id}\n")),
                Decision::Fire(at) => out.push_str(&format!("t {}\n", at.as_nanos())),
            }
        }
        out
    }
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut tape = Tape::default();
        for (n, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (op, arg) = line.split_once(' ').unwrap_or((line, ""));
            let number = || {
                arg.trim()
                    .parse::<u64>()
                    .map_err(|e| format!("tape line {}: {e}: {line:?}", n + 1))
            };
            match op {
                "seed" => tape.seed = number()?,
                "spin" => tape.spin = Some(number()?),
                "c" if arg.trim().is_empty() => tape.decisions.push(Decision::Canonical(None)),
                "c" => tape.decisions.push(Decision::Canonical(Some(number()?))),
                "p" => tape.decisions.push(Decision::Poll(number()?)),
                "t" => {
                    // Written from `Duration::as_nanos`, a u128.
                    let nanos = arg
                        .trim()
                        .parse::<u128>()
                        .map_err(|e| format!("tape line {}: {e}: {line:?}", n + 1))?;
                    let secs = u64::try_from(nanos / 1_000_000_000)
                        .map_err(|e| format!("tape line {}: {e}: {line:?}", n + 1))?;
                    let at = Duration::new(secs, (nanos % 1_000_000_000) as u32);
                    tape.decisions.push(Decision::Fire(at));
                }
                _ => return Err(format!("tape line {}: unknown entry {line:?}", n + 1)),
            }
        }
        Ok(tape)
    }
    pub fn save(&self, path: &std::path::Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, self.to_text())
    }
    pub fn load(path: &std::path::Path) -> std::io::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        Self::parse(&text).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }
}

// ---------------------------------------------------------------------------
// Running and shrinking one test body
// ---------------------------------------------------------------------------

/// Result of [`run_once`].
#[derive(Debug, Clone)]
pub struct Outcome {
    /// The panic message, if the body (or teardown) panicked.
    pub failure: Option<String>,
    pub tape: Tape,
    pub stats: Stats,
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "Box<dyn Any>".into())
}

/// Run `body(seed)` in a fresh simulation, recording its tape; with
/// `script`, reproduce that tape exactly ([`ReplayMode::Strict`]) instead of
/// scheduling from the seed's generator.
pub fn run_once<F, Fut>(seed: u64, script: Option<&Tape>, body: &F) -> Outcome
where
    F: Fn(u64) -> Fut,
    Fut: Future<Output = ()>,
{
    run_with(seed, script.map(|tape| (tape, ReplayMode::Strict)), body)
}

/// [`run_once`] with the replay mode chosen.
pub fn run_with<F, Fut>(seed: u64, script: Option<(&Tape, ReplayMode)>, body: &F) -> Outcome
where
    F: Fn(u64) -> Fut,
    Fut: Future<Output = ()>,
{
    run_planned(seed, script, None, body).0
}

/// [`run_with`] with an exploration plan choosing the tasks (a script, if
/// given, still wins). Returns the plan back so its record can be read.
fn run_planned<F, Fut>(
    seed: u64,
    script: Option<(&Tape, ReplayMode)>,
    plan: Option<explore::Plan>,
    body: &F,
) -> (Outcome, Option<explore::Plan>)
where
    F: Fn(u64) -> Fut,
    Fut: Future<Output = ()>,
{
    let rt = Runtime::with_seed(seed);
    rt.record_tape();
    let strict = matches!(script, Some((_, ReplayMode::Strict)));
    if let Some((tape, mode)) = script {
        rt.replay_with(tape, mode);
    }
    lock(&rt.handle.shared.state).plan = plan;
    let result = catch_unwind(AssertUnwindSafe(|| rt.block_on(body(seed))));
    let tape = rt.tape();
    let stats = rt.stats();
    // Drain explicitly: a task that kept its own `Runtime` handle would
    // otherwise keep the world (and every parked task) alive.
    let plan = lock(&rt.handle.shared.state).plan.take();
    let shared = rt.handle.shared.clone();
    let dropped = catch_unwind(AssertUnwindSafe(move || {
        shared.drain();
        drop(rt)
    }));
    let failure = match (result, dropped) {
        (Err(payload), _) | (Ok(()), Err(payload)) => Some(panic_message(&*payload)),
        (Ok(()), Ok(())) => None,
    };
    // A strict replay that stops (by failing) with decisions left over did
    // not reproduce the recorded run, whatever its panic says.
    let failure = match failure {
        Some(failure) if strict && stats.tape_left > 0 && !failure.contains("replay diverged") => {
            Some(format!(
                "spawned_sim: replay diverged: the run stopped with {} tape decisions unused \
                 (it failed with: {failure})",
                stats.tape_left
            ))
        }
        failure => failure,
    };
    (
        Outcome {
            failure,
            tape,
            stats,
        },
        plan,
    )
}

thread_local! {
    static QUIET: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Run `f` with panic messages from this thread suppressed.
fn quietly<R>(f: impl FnOnce() -> R) -> R {
    static HOOK: std::sync::Once = std::sync::Once::new();
    HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if !QUIET.with(|q| q.get()) {
                previous(info);
            }
        }));
    });
    struct Reset(bool);
    impl Drop for Reset {
        fn drop(&mut self) {
            QUIET.with(|q| q.set(self.0));
        }
    }
    let _reset = Reset(QUIET.with(|q| q.replace(true)));
    f()
}

fn script_tape(seed: u64, spin: Option<u64>, picks: &[Option<u64>]) -> Tape {
    Tape {
        seed,
        spin,
        decisions: picks
            .iter()
            .map(|p| p.map_or(Decision::Canonical(None), Decision::Poll))
            .collect(),
    }
}

/// Shrink a failing tape toward the canonical schedule (always poll the
/// smallest ready id) while `body` keeps failing with exactly the same panic
/// message (a looser match can drift to a different bug):
/// first the shortest scripted prefix (the rest canonical), then removing
/// non-canonical picks in halving chunks. Candidates run in
/// [`ReplayMode::Permissive`]; a candidate is kept only if its recorded tape
/// then fails the same way in a strict replay, so the result always
/// reproduces. Spends at most `budget` runs (both kinds) and returns the
/// smallest failing tape found and the runs spent.
pub fn shrink<F, Fut>(failing: &Tape, failure: &str, body: &F, budget: usize) -> (Tape, usize)
where
    F: Fn(u64) -> Fut,
    Fut: Future<Output = ()>,
{
    let seed = failing.seed;
    let runs = std::cell::Cell::new(0);
    let attempt = |picks: &[Option<u64>]| -> Option<Tape> {
        // A candidate costs two runs (permissive, then strict): reserve both.
        if runs.get() + 2 > budget {
            return None;
        }
        runs.set(runs.get() + 1);
        let candidate = script_tape(seed, failing.spin, picks);
        let outcome = quietly(|| run_with(seed, Some((&candidate, ReplayMode::Permissive)), body));
        if outcome.failure.as_deref() != Some(failure) {
            return None;
        }
        runs.set(runs.get() + 1);
        let check = quietly(|| run_once(seed, Some(&outcome.tape), body));
        (check.failure.as_deref() == Some(failure)).then_some(outcome.tape)
    };
    let mut best = failing.clone();

    // 1. Shortest failing prefix, by bisection.
    let picks = best.picks();
    let (mut lo, mut hi) = (0, picks.len());
    while lo < hi {
        let mid = (lo + hi) / 2;
        match attempt(&picks[..mid]) {
            Some(tape) => {
                hi = mid;
                // A shorter script can still reach the failure through a
                // longer canonical suffix; keep only a smaller tape.
                if (tape.deviations(), tape.decisions.len())
                    <= (best.deviations(), best.decisions.len())
                {
                    best = tape;
                }
            }
            None => lo = mid + 1,
        }
    }

    // 2. Replace non-canonical picks by canonical ones, in halving chunks.
    let mut chunk = best.deviations().div_ceil(2).max(1);
    loop {
        let mut progressed = false;
        let mut start = 0;
        loop {
            let picks = best.picks();
            let deviations: Vec<usize> = picks
                .iter()
                .enumerate()
                .filter_map(|(i, p)| p.map(|_| i))
                .collect();
            if start >= deviations.len() {
                break;
            }
            let mut candidate = picks.clone();
            for &i in &deviations[start..(start + chunk).min(deviations.len())] {
                candidate[i] = None;
            }
            match attempt(&candidate) {
                Some(tape) if tape.deviations() < best.deviations() => {
                    best = tape;
                    progressed = true;
                }
                _ => start += chunk,
            }
            if runs.get() + 2 > budget {
                return (best, runs.get());
            }
        }
        if chunk == 1 && !progressed {
            break;
        }
        chunk = (chunk / 2).max(1);
    }
    (best, runs.get())
}

// ---------------------------------------------------------------------------
// Clock and entropy seams (`spawned_rt::time` under the cfg)
// ---------------------------------------------------------------------------

fn running_world() -> Option<Arc<Shared>> {
    current().or_else(|| AMBIENT.with(|a| a.borrow().upgrade()))
}

/// Virtual-time instant: the simulation's elapsed virtual time.
///
/// Read on a thread with no live simulation (a native actor's OS thread, or
/// before any runtime exists) it falls back to real time elapsed since the
/// process's first such read, so instants from different worlds must not be
/// compared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Instant(Duration);

impl Instant {
    pub fn now() -> Self {
        match running_world() {
            Some(shared) => Self(lock(&shared.state).now),
            None => {
                static BASE: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
                Self(BASE.get_or_init(std::time::Instant::now).elapsed())
            }
        }
    }
    pub fn elapsed(&self) -> Duration {
        Self::now().saturating_duration_since(*self)
    }
    pub fn duration_since(&self, earlier: Self) -> Duration {
        self.saturating_duration_since(earlier)
    }
    pub fn saturating_duration_since(&self, earlier: Self) -> Duration {
        self.0.saturating_sub(earlier.0)
    }
    pub fn checked_duration_since(&self, earlier: Self) -> Option<Duration> {
        self.0.checked_sub(earlier.0)
    }
    pub fn checked_add(&self, duration: Duration) -> Option<Self> {
        self.0.checked_add(duration).map(Self)
    }
    pub fn checked_sub(&self, duration: Duration) -> Option<Self> {
        self.0.checked_sub(duration).map(Self)
    }
}
impl std::ops::Add<Duration> for Instant {
    type Output = Self;
    fn add(self, rhs: Duration) -> Self {
        Self(self.0 + rhs)
    }
}
impl std::ops::AddAssign<Duration> for Instant {
    fn add_assign(&mut self, rhs: Duration) {
        self.0 += rhs;
    }
}
impl std::ops::Sub<Duration> for Instant {
    type Output = Self;
    fn sub(self, rhs: Duration) -> Self {
        Self(self.0.saturating_sub(rhs))
    }
}
impl std::ops::SubAssign<Duration> for Instant {
    fn sub_assign(&mut self, rhs: Duration) {
        self.0 = self.0.saturating_sub(rhs);
    }
}
impl std::ops::Sub<Instant> for Instant {
    type Output = Duration;
    fn sub(self, rhs: Instant) -> Duration {
        self.saturating_duration_since(rhs)
    }
}

/// Deterministic stand-in for wall-clock entropy: a stream derived from the
/// running simulation's seed, separate from its scheduling generator, so
/// drawing from it does not shift the schedule.
pub fn entropy() -> u64 {
    match running_world() {
        Some(shared) => {
            let mut state = lock(&shared.state);
            state.entropy += 1;
            let mut x = state.seed ^ state.entropy.wrapping_mul(0xD1B5_4A32_D192_ED03);
            splitmix(&mut x)
        }
        None => {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
            let mut x = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            splitmix(&mut x)
        }
    }
}

#[cfg(test)]
mod turn_budget_tests {
    use super::{parse_turn_budget, Duration, DEFAULT_TURN_BUDGET_MS};

    #[test]
    fn turn_budget_env_values() {
        let default = Some(Duration::from_millis(DEFAULT_TURN_BUDGET_MS));
        assert_eq!(parse_turn_budget(None), default);
        assert_eq!(parse_turn_budget(Some("not a number")), default);
        assert_eq!(parse_turn_budget(Some(" 40 ")), Some(Duration::from_millis(40)));
        assert_eq!(parse_turn_budget(Some("0")), None);
    }
}
