//! The reply slot: one packed atomic word per request
//! (docs/design/deferred-replies.md §1, "Slot synchronization protocol").
//!
//! ```text
//! word:  state:4 | TOKEN:1 | REG:1 | JOBS:16
//! credits(word) = max(JOBS, TOKEN and state is live ? 1 : 0)
//! ```
//!
//! Every change of the state, `TOKEN`, `REG` or `JOBS` is one compare-and-swap
//! on the word. The owner's credit pool changes only by
//! `credits(new) - credits(old)` of a successful CAS: a positive delta is
//! reserved before the CAS and rolled back if the CAS fails; a negative delta
//! is returned after it, before the receiver is woken.
//!
//! `WRITING` counts as live for `credits`: it is the transient state of a
//! reply between its linearization point (`OPEN -> WRITING`) and the
//! publication of the value (`WRITING -> REPLIED`) or the deadline check
//! (`WRITING -> TIMED_OUT`), so the token's credit returns at that second CAS.
//!
//! This file is shared with the Loom model (`tests/reply_slot_loom.rs`), which
//! supplies the synchronization types through `super::sync`. Time is passed in
//! as a `now` closure returning clock ticks, so the model controls the clock.
//! No user code runs under the slot mutex: values and wakers are moved out and
//! dropped or woken after it is released.

use super::sync::{wait_ticks, yield_now, Arc, AtomicU64, AtomicUsize, Condvar, Mutex, Ordering};
use std::task::{Poll, Waker};

pub(crate) const OPEN: u64 = 0;
pub(crate) const WRITING: u64 = 1;
pub(crate) const REPLIED: u64 = 2;
pub(crate) const DROPPED: u64 = 3;
pub(crate) const OVERLOADED: u64 = 4;
pub(crate) const TIMED_OUT: u64 = 5;
pub(crate) const STOPPED: u64 = 6;
pub(crate) const ABANDONED: u64 = 7;

const STATE_MASK: u64 = 0xF;
pub(crate) const TOKEN: u64 = 1 << 4;
pub(crate) const REG: u64 = 1 << 5;
const JOBS_SHIFT: u32 = 6;
const JOB_ONE: u64 = 1 << JOBS_SHIFT;
/// Largest running linked-job count per slot; `MAX_OUTSTANDING` is capped at
/// this value at configuration so the field cannot overflow.
pub(crate) const MAX_JOBS: u64 = 0xFFFF;
const JOBS_MASK: u64 = MAX_JOBS << JOBS_SHIFT;

/// Deadline value meaning "never" (`Deadline::Never`).
pub(crate) const NEVER: u64 = u64::MAX;

pub(crate) fn state(word: u64) -> u64 {
    word & STATE_MASK
}
pub(crate) fn jobs(word: u64) -> u64 {
    (word & JOBS_MASK) >> JOBS_SHIFT
}
fn live(state: u64) -> bool {
    state == OPEN || state == WRITING
}
/// Credits a slot holds in its owner's pool.
pub(crate) fn credits(word: u64) -> u64 {
    let token = (word & TOKEN != 0 && live(state(word))) as u64;
    jobs(word).max(token)
}
fn with_state(word: u64, to: u64) -> u64 {
    (word & !STATE_MASK) | to
}

/// Terminal outcome of a request, as the caller sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Terminal {
    Replied,
    Dropped,
    Overloaded,
    TimedOut,
    Stopped,
    Abandoned,
}

/// The model's name (`DeferredReplies.tla`) of the terminal state a slot
/// leaves `OPEN` for; `WRITING` is the reply's linearization point.
fn trace_name(state: u64) -> &'static str {
    match state {
        WRITING | REPLIED => "ok",
        DROPPED => "dropped",
        OVERLOADED => "overloaded",
        TIMED_OUT => "timeout",
        STOPPED => "stopped",
        ABANDONED => "abandoned",
        _ => "unknown",
    }
}

impl Terminal {
    pub(crate) fn of(state: u64) -> Option<Self> {
        Some(match state {
            REPLIED => Self::Replied,
            DROPPED => Self::Dropped,
            OVERLOADED => Self::Overloaded,
            TIMED_OUT => Self::TimedOut,
            STOPPED => Self::Stopped,
            ABANDONED => Self::Abandoned,
            _ => return None,
        })
    }
    fn state(self) -> u64 {
        match self {
            Self::Replied => REPLIED,
            Self::Dropped => DROPPED,
            Self::Overloaded => OVERLOADED,
            Self::TimedOut => TIMED_OUT,
            Self::Stopped => STOPPED,
            Self::Abandoned => ABANDONED,
        }
    }
}

/// Outcome of a reply, as the replier sees it. More outcomes may be added.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Delivery {
    /// The value was published to the caller.
    Delivered,
    /// The caller left (abandoned the receiver) or the slot was already
    /// answered (dropped, overloaded).
    CallerGone,
    /// The request's deadline had passed; the value was dropped.
    Expired,
    /// The owner died and its registry closed the slot (`ActorStopped`).
    OwnerExited,
}

/// Why `defer()` or a linked-job attach failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refused {
    /// The slot is no longer open (answered, expired, abandoned, closed).
    Closed,
    /// The owner's credit pool is exhausted. For `defer()` the caller has
    /// already been answered `Overloaded`.
    Overloaded,
}

/// Owner-wide credit counter (`MAX_OUTSTANDING`).
pub(crate) struct CreditPool {
    used: AtomicUsize,
    max: usize,
}

impl CreditPool {
    pub(crate) fn new(max: usize) -> Self {
        Self {
            used: AtomicUsize::new(0),
            max,
        }
    }
    /// Reserve `n` credits, failing at the bound without waiting.
    pub(crate) fn reserve(&self, n: u64) -> bool {
        let n = n as usize;
        self.used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(n).filter(|total| *total <= self.max)
            })
            .is_ok()
    }
    pub(crate) fn release(&self, n: u64) {
        if n != 0 {
            let previous = self.used.fetch_sub(n as usize, Ordering::AcqRel);
            debug_assert!(previous >= n as usize, "credit pool underflow");
        }
    }
    pub(crate) fn used(&self) -> usize {
        self.used.load(Ordering::Acquire)
    }
    #[allow(dead_code)] // used by the Loom model
    pub(crate) fn max(&self) -> usize {
        self.max
    }
}

struct Inner<R> {
    /// Written only by the reply that moved the word to `WRITING`, and
    /// published by `WRITING -> REPLIED` under this lock.
    value: Option<R>,
    /// The caller's receiver.
    waker: Option<Waker>,
    /// Token-side waiters: `ReplyTo::closed()` and caller-gone job
    /// cancellation. Several may wait at once.
    watchers: Vec<Waker>,
    /// The owner's pool, set by `defer()` before its CAS installs `TOKEN`.
    pool: Option<Arc<CreditPool>>,
    /// Threads parked in `recv_blocking`.
    parked: usize,
}

pub(crate) struct Slot<R> {
    /// The receiving instance (production: its registry), whose deadline
    /// heap expires an awaited raw request.
    #[cfg_attr(not(feature = "async"), allow(dead_code))]
    pub(crate) owner: super::sync::SlotOwner,
    word: AtomicU64,
    deadline: AtomicU64,
    inner: Mutex<Inner<R>>,
    parked: Condvar,
}

fn lock<T>(mutex: &Mutex<T>) -> super::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}

/// What a winning transition must do after its CAS: return credits, then wake.
pub(crate) struct Wake {
    pool: Option<Arc<CreditPool>>,
    release: u64,
    waker: Option<Waker>,
    watchers: Vec<Waker>,
    notify: bool,
}

impl Wake {
    fn run(self, slot_parked: &Condvar) {
        if let Some(pool) = self.pool {
            pool.release(self.release);
        }
        if let Some(waker) = self.waker {
            waker.wake();
        }
        for watcher in self.watchers {
            watcher.wake();
        }
        if self.notify {
            slot_parked.notify_all();
        }
    }
}

impl<R> Slot<R> {
    /// A fresh `OPEN` slot whose deadline is `deadline` ticks (`NEVER` for none).
    #[allow(dead_code)] // the Loom model and unit tests build ownerless slots
    pub(crate) fn new(deadline: u64) -> Self {
        Self::with_owner(deadline, Default::default())
    }

    pub(crate) fn with_owner(deadline: u64, owner: super::sync::SlotOwner) -> Self {
        Self {
            owner,
            word: AtomicU64::new(OPEN),
            deadline: AtomicU64::new(deadline),
            inner: Mutex::new(Inner {
                value: None,
                waker: None,
                watchers: Vec::new(),
                pool: None,
                parked: 0,
            }),
            parked: Condvar::new(),
        }
    }

    pub(crate) fn word(&self) -> u64 {
        self.word.load(Ordering::Acquire)
    }

    pub(crate) fn deadline(&self) -> u64 {
        self.deadline.load(Ordering::Acquire)
    }

    pub(crate) fn is_terminal(&self) -> bool {
        !live(state(self.word()))
    }

    /// One CAS with credit accounting. `step` maps the current word to the new
    /// one or aborts. A positive delta is reserved from `pool` first and rolled
    /// back on CAS failure; `Err(None)` reports that the reservation failed.
    /// Returns `(old, new)`; the caller returns any negative delta.
    fn cas<E>(
        &self,
        pool: Option<&CreditPool>,
        mut step: impl FnMut(u64) -> Result<u64, E>,
    ) -> Result<(u64, u64), Option<E>> {
        let mut current = self.word.load(Ordering::Acquire);
        loop {
            let next = step(current).map_err(Some)?;
            let up = credits(next).saturating_sub(credits(current));
            if up > 0 {
                match pool {
                    Some(pool) if pool.reserve(up) => {}
                    _ => {
                        // Report the bound only for the word it was computed
                        // from: a word that changed meanwhile (closed,
                        // abandoned) is re-evaluated, so a closed slot reports
                        // `Closed`, not `Overloaded`.
                        let actual = self.word.load(Ordering::Acquire);
                        if actual != current {
                            current = actual;
                            continue;
                        }
                        return Err(None);
                    }
                }
            }
            let exchanged = super::sync::section(|| {
                let exchanged =
                    self.word
                        .compare_exchange(current, next, Ordering::AcqRel, Ordering::Acquire);
                if exchanged.is_ok() && state(current) == OPEN && state(next) != OPEN {
                    super::sync::left_open(
                        &self.owner,
                        self as *const Self as usize,
                        trace_name(state(next)),
                        current & TOKEN != 0,
                    );
                }
                exchanged
            });
            match exchanged {
                Ok(_) => return Ok((current, next)),
                Err(actual) => {
                    if let (Some(pool), true) = (pool, up > 0) {
                        pool.release(up);
                    }
                    current = actual;
                }
            }
        }
    }

    /// After a winning CAS `old -> new`: collect the credit return and, when
    /// the state became terminal, the waker. The caller runs it after every
    /// lock is released.
    fn after(&self, old: u64, new: u64) -> Wake {
        let release = credits(old).saturating_sub(credits(new));
        let terminal = live(state(old)) && !live(state(new));
        if release == 0 && !terminal {
            return Wake {
                pool: None,
                release: 0,
                waker: None,
                watchers: Vec::new(),
                notify: false,
            };
        }
        let mut inner = lock(&self.inner);
        Self::collect(&mut inner, release, terminal)
    }

    /// The wake-up work of a transition, taken under the slot lock.
    fn collect(inner: &mut Inner<R>, release: u64, terminal: bool) -> Wake {
        Wake {
            pool: if release > 0 {
                inner.pool.clone()
            } else {
                None
            },
            release,
            waker: if terminal { inner.waker.take() } else { None },
            watchers: if terminal {
                std::mem::take(&mut inner.watchers)
            } else {
                Vec::new()
            },
            notify: terminal && inner.parked > 0,
        }
    }

    /// `OPEN -> to` (drop, overloaded, expiry, abandonment, registry close).
    /// Returns whether this call won the transition.
    pub(crate) fn terminate(&self, to: Terminal) -> bool {
        self.terminate_admitted(to, || Some(()))
    }

    /// [`terminate`](Self::terminate) admitted by the owner: `admit`'s
    /// permit covers only the transition's CAS; the credit return and the
    /// wakes (executor code) run after it is released. Refused (`None`), the
    /// slot is left to the owner's close.
    pub(crate) fn terminate_admitted<P>(
        &self,
        to: Terminal,
        admit: impl FnOnce() -> Option<P>,
    ) -> bool {
        debug_assert!(to != Terminal::Replied);
        let Some(permit) = admit() else {
            return false;
        };
        let target = to.state();
        let transition = self.cas::<()>(None, |w| {
            if state(w) == OPEN {
                Ok(with_state(w, target))
            } else {
                Err(())
            }
        });
        drop(permit);
        match transition {
            Ok((old, new)) => {
                self.after(old, new).run(&self.parked);
                true
            }
            Err(_) => false,
        }
    }

    /// Registry close at the owner's death: every slot still `OPEN` becomes
    /// `STOPPED`. A reply in `WRITING` is a value write with no user code in
    /// it, so the close waits for it to resolve.
    pub(crate) fn close(&self) -> bool {
        match self.close_collect() {
            Some(wake) => {
                wake.run(&self.parked);
                true
            }
            None => false,
        }
    }

    /// [`close`](Self::close) without the wake-up: the transition (and its
    /// credit return) happens now and the caller runs the returned work with
    /// [`run_wake`](Self::run_wake) once a whole batch is closed, so no
    /// waker (executor or user code) runs between two closes.
    pub(crate) fn close_collect(&self) -> Option<Wake> {
        loop {
            if let Ok((old, new)) = self.cas::<()>(None, |w| {
                if state(w) == OPEN {
                    Ok(with_state(w, STOPPED))
                } else {
                    Err(())
                }
            }) {
                let mut wake = self.after(old, new);
                // The credit returns with the transition; only wakes wait.
                if let Some(pool) = wake.pool.take() {
                    pool.release(wake.release);
                }
                return Some(wake);
            }
            if state(self.word()) != WRITING {
                return None;
            }
            yield_now();
        }
    }

    /// Run the wake-up work [`close_collect`](Self::close_collect) returned.
    pub(crate) fn run_wake(&self, wake: Wake) {
        wake.run(&self.parked);
    }

    /// `[deadline]`: expire the slot if `now >= deadline`. Returns whether the
    /// deadline has passed (whoever won the transition).
    pub(crate) fn check_deadline(&self, now: &impl Fn() -> u64) -> bool {
        let deadline = self.deadline();
        if deadline != NEVER && now() >= deadline {
            self.terminate(Terminal::TimedOut);
            true
        } else {
            false
        }
    }

    /// `is_closed()`: not `OPEN`, or past its deadline (then expired).
    pub(crate) fn is_closed(&self, now: &impl Fn() -> u64) -> bool {
        state(self.word()) != OPEN || self.check_deadline(now)
    }

    /// `with_deadline(at)`: shorten only, then the `[deadline]` check.
    /// Returns whether the deadline was lowered.
    ///
    /// The reply's `WRITING` CAS is the linearization point: the reply reads
    /// the deadline under the slot lock, and a shortening changes it only
    /// under that lock and only while the word is `OPEN`. A shortening that
    /// finds `WRITING` (or later) is a no-op; one that lowers the deadline is
    /// seen by any reply that has not read it yet.
    pub(crate) fn shorten(&self, at: u64, now: &impl Fn() -> u64) -> bool {
        let (lowered, watchers, waker, parked) = {
            let mut inner = lock(&self.inner);
            let lower = state(self.word()) == OPEN && at < self.deadline();
            if lower {
                self.deadline.store(at, Ordering::Release);
                // Moved out, not cloned: no executor code under the lock. A
                // woken receiver re-registers when it polls again.
                (
                    true,
                    std::mem::take(&mut inner.watchers),
                    inner.waker.take(),
                    inner.parked > 0,
                )
            } else {
                (false, Vec::new(), None, false)
            }
        };
        // Both sides re-arm their timers for the earlier deadline: the
        // token's waiters, the caller's receiver and parked `recv_blocking`
        // threads (which recompute their wait).
        for watcher in watchers {
            watcher.wake();
        }
        if let Some(waker) = waker {
            waker.wake();
        }
        if parked {
            self.parked.notify_all();
        }
        self.check_deadline(now);
        lowered
    }

    /// `defer()`: `OPEN, TOKEN=0 -> OPEN, TOKEN=1, REG=1`, reserving the
    /// token's credit when no linked job holds one. At the bound the caller
    /// is answered `Overloaded` in the same step.
    pub(crate) fn defer(
        &self,
        pool: &Arc<CreditPool>,
        now: &impl Fn() -> u64,
    ) -> Result<(), Refused> {
        if self.check_deadline(now) {
            return Err(Refused::Closed);
        }
        lock(&self.inner).pool = Some(pool.clone());
        match self.cas(Some(pool), |w| {
            if state(w) == OPEN && w & TOKEN == 0 {
                Ok(w | TOKEN | REG)
            } else {
                Err(())
            }
        }) {
            Ok(_) => Ok(()),
            Err(Some(())) => Err(Refused::Closed),
            // At the bound the caller is answered `Overloaded` in the same
            // step; if it closed first (abandoned, expired), this is `Closed`.
            Err(None) if self.terminate(Terminal::Overloaded) => Err(Refused::Overloaded),
            Err(None) => Err(Refused::Closed),
        }
    }

    /// `pipe_reply` attach: requires `OPEN` and `TOKEN`; `JOBS + 1`. The first
    /// job shares the token's credit; a further one reserves a new credit.
    pub(crate) fn attach(&self, now: &impl Fn() -> u64) -> Result<(), Refused> {
        if self.check_deadline(now) {
            return Err(Refused::Closed);
        }
        let pool = lock(&self.inner).pool.clone();
        match self.cas(pool.as_deref(), |w| {
            if state(w) != OPEN || w & TOKEN == 0 {
                Err(Refused::Closed)
            } else if jobs(w) >= MAX_JOBS {
                Err(Refused::Overloaded)
            } else {
                Ok(w + JOB_ONE)
            }
        }) {
            Ok(_) => Ok(()),
            Err(Some(refused)) => Err(refused),
            Err(None) => Err(Refused::Overloaded),
        }
    }

    /// A linked job ended (in any state): `JOBS - 1`, returning a credit if
    /// this lowers `credits`.
    pub(crate) fn job_end(&self) {
        match self.cas::<()>(None, |w| {
            debug_assert!(jobs(w) >= 1, "job_end without a running job");
            Ok(w - JOB_ONE)
        }) {
            Ok((old, new)) => self.after(old, new).run(&self.parked),
            Err(_) => unreachable!("job_end never aborts or reserves"),
        }
    }

    /// Change only the state field of a `WRITING` word, retrying on concurrent
    /// `JOBS` changes (a whole-word store would lose a job-end decrement).
    fn resolve_writing(&self, to: u64) -> (u64, u64) {
        match self.cas::<()>(None, |w| {
            debug_assert_eq!(state(w), WRITING);
            Ok(with_state(w, to))
        }) {
            Ok(pair) => pair,
            Err(_) => unreachable!("resolving WRITING never aborts or reserves"),
        }
    }

    /// Reply: `OPEN -> WRITING`, then read the deadline: at or past it,
    /// `WRITING -> TIMED_OUT` and the value comes back as `Expired`;
    /// otherwise write the value and `WRITING -> REPLIED`. A value that was
    /// not published is returned so the caller drops it outside every lock.
    pub(crate) fn reply(&self, value: R, now: &impl Fn() -> u64) -> (Delivery, Option<R>) {
        self.reply_admitted(value, now, || Some(()))
    }

    /// [`reply`](Self::reply) admitted by the owner: `admit` returns a
    /// permit held across the linearizing `OPEN -> WRITING` CAS (and
    /// released before the value is written), or `None` once the owner is
    /// past its death point. Refused, the slot is left to the owner's close
    /// (`OwnerExited`).
    pub(crate) fn reply_admitted<P>(
        &self,
        value: R,
        now: &impl Fn() -> u64,
        admit: impl FnOnce() -> Option<P>,
    ) -> (Delivery, Option<R>) {
        let Some(permit) = admit() else {
            return (Delivery::OwnerExited, Some(value));
        };
        let linearized = self.cas(None, |w| {
            if state(w) == OPEN {
                Ok(with_state(w, WRITING))
            } else {
                Err(state(w))
            }
        });
        drop(permit);
        if let Err(Some(s)) = linearized {
            let delivery = match s {
                TIMED_OUT => Delivery::Expired,
                STOPPED => Delivery::OwnerExited,
                _ => Delivery::CallerGone,
            };
            return (delivery, Some(value));
        }
        // The deadline is read under the lock that shortening takes, so a
        // shortening either precedes this read or finds `WRITING` and does
        // nothing.
        let mut inner = lock(&self.inner);
        let deadline = self.deadline();
        let (delivery, unsent, to) = if deadline != NEVER && now() >= deadline {
            (Delivery::Expired, Some(value), TIMED_OUT)
        } else {
            inner.value = Some(value);
            (Delivery::Delivered, None, REPLIED)
        };
        let (old, new) = self.resolve_writing(to);
        let wake = Self::collect(&mut inner, credits(old).saturating_sub(credits(new)), true);
        drop(inner);
        wake.run(&self.parked);
        (delivery, unsent)
    }

    /// Receiver side: take the outcome of a terminal word. `Replied` with no
    /// value means it was already taken.
    fn take(&self, state: u64) -> Option<Result<R, Terminal>> {
        let terminal = Terminal::of(state)?;
        Some(match terminal {
            Terminal::Replied => match lock(&self.inner).value.take() {
                Some(value) => Ok(value),
                None => Err(Terminal::Replied),
            },
            other => Err(other),
        })
    }

    /// Poll from the caller's receiver: registers `waker` and returns
    /// `Pending` while the slot is open. The caller's timer expires the slot
    /// at its deadline (`ReplyReceiver` sleeps until it), so the poll itself
    /// reads no clock.
    #[cfg_attr(not(feature = "async"), allow(dead_code))]
    pub(crate) fn poll(&self, waker: &Waker) -> Poll<Result<R, Terminal>> {
        // `Waker::clone` and dropping a waker run executor code, so neither
        // happens under the slot lock.
        let mut fresh: Option<Waker> = None;
        loop {
            if let Some(result) = self.take(state(self.word())) {
                return Poll::Ready(result);
            }
            let mut inner = lock(&self.inner);
            if !live(state(self.word())) {
                continue;
            }
            if inner.waker.as_ref().is_some_and(|w| w.will_wake(waker)) {
                drop(inner);
                drop(fresh);
                return Poll::Pending;
            }
            let Some(new) = fresh.take() else {
                drop(inner);
                fresh = Some(waker.clone());
                continue;
            };
            let old = inner.waker.replace(new);
            drop(inner);
            drop(old);
            return Poll::Pending;
        }
    }

    /// Token side: ready once the slot is terminal (or its deadline passed,
    /// which expires it). Registers `waker` as the watcher otherwise.
    #[cfg_attr(not(feature = "async"), allow(dead_code))]
    pub(crate) fn poll_closed(&self, waker: &Waker, now: &impl Fn() -> u64) -> Poll<()> {
        let mut fresh: Option<Waker> = None;
        loop {
            if !live(state(self.word())) || self.check_deadline(now) {
                return Poll::Ready(());
            }
            let mut inner = lock(&self.inner);
            if !live(state(self.word())) {
                continue;
            }
            if inner.watchers.iter().any(|w| w.will_wake(waker)) {
                drop(inner);
                drop(fresh);
                return Poll::Pending;
            }
            let Some(new) = fresh.take() else {
                drop(inner);
                fresh = Some(waker.clone());
                continue;
            };
            inner.watchers.push(new);
            return Poll::Pending;
        }
    }

    /// Non-blocking check from the caller's receiver.
    pub(crate) fn try_take(&self, now: &impl Fn() -> u64) -> Option<Result<R, Terminal>> {
        if let Some(result) = self.take(state(self.word())) {
            return Some(result);
        }
        if self.check_deadline(now) {
            return self.take(state(self.word()));
        }
        None
    }

    /// Threads-mode receive: park until terminal, expiring the slot at its
    /// deadline.
    pub(crate) fn recv_blocking(&self, now: &impl Fn() -> u64) -> Result<R, Terminal> {
        loop {
            if let Some(result) = self.take(state(self.word())) {
                return result;
            }
            if self.check_deadline(now) {
                continue;
            }
            let mut inner = lock(&self.inner);
            if !live(state(self.word())) {
                continue;
            }
            let deadline = self.deadline();
            let remaining = (deadline != NEVER).then(|| deadline.saturating_sub(now()));
            if remaining == Some(0) {
                continue;
            }
            inner.parked += 1;
            let mut inner = wait_ticks(&self.parked, inner, remaining);
            inner.parked -= 1;
        }
    }

    /// The caller dropped its receiver: `OPEN -> ABANDONED`.
    pub(crate) fn abandon(&self) -> bool {
        self.terminate(Terminal::Abandoned)
    }
}
