//! The reply slot: one packed atomic word per request
//! (docs/design/deferred-replies.md §1, "Slot synchronization protocol").
//!
//! ```text
//! word:  state:4 | TOKEN:1 | REG:1 | JOBS:16 | PARKED:1
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
//! No user code runs under a slot lock: values and wakers are moved out and
//! dropped or woken after it is released.
//!
//! The slot lock guards the value and the caller's waker (a thread blocked
//! in `recv_blocking` registers its unparker there: no condvar). Deferred
//! token state (the credit pool and the token's watchers) is created by
//! `defer()`, so a plain request never allocates it and its drop is cheap.
//! (An `AtomicWaker` beside a value mutex measured more instructions per
//! request than one lock for both: two more read-modify-writes.)

use super::sync::{yield_now, Arc, AtomicU64, AtomicUsize, Mutex, Ordering};
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
/// The registered waker unparks a thread blocked in `recv_blocking` (no
/// user code): the death point's discharge wakes it before any other
/// waker. Set under the slot lock, before the waker is stored; no
/// transition reads or clears it otherwise (`with_state`, `JOBS` arithmetic
/// and `credits` leave it alone). A word bit, not a field: it keeps the
/// slot within the allocator's small size classes.
pub(crate) const PARKED: u64 = 1 << (JOBS_SHIFT + 16);
/// The deadline was put in the owner's heap at send (a raw request with a
/// deadline): an awaited receiver waits for the heap's expiry instead of
/// arming its own timer. Set before the slot is shared; never changes.
pub(crate) const TRACKED: u64 = PARKED << 1;

/// Deadline value meaning "never" (`Deadline::Never`).
pub(crate) const NEVER: u64 = u64::MAX;

#[inline]
pub(crate) fn state(word: u64) -> u64 {
    word & STATE_MASK
}
#[inline]
pub(crate) fn jobs(word: u64) -> u64 {
    (word & JOBS_MASK) >> JOBS_SHIFT
}
#[inline]
fn live(state: u64) -> bool {
    state == OPEN || state == WRITING
}
/// Credits a slot holds in its owner's pool.
#[inline]
pub(crate) fn credits(word: u64) -> u64 {
    let token = (word & TOKEN != 0 && live(state(word))) as u64;
    jobs(word).max(token)
}
#[inline]
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

/// Rarely needed state, created on demand: by `defer()` (which installs the
/// pool before its CAS can take a credit) and by the owner's close at its
/// death point (the final reason). A plain request never allocates it.
#[derive(Default)]
struct Side {
    /// The owner's pool, where this slot's credits return.
    pool: Option<Arc<CreditPool>>,
    /// Token-side waiters: `ReplyTo::closed()` and caller-gone job
    /// cancellation. Several may wait at once.
    watchers: Vec<Waker>,
    /// The owner's final exit reason, stored by its close before the slot
    /// becomes `STOPPED`, so the caller reads it from the slot.
    reason: Option<super::sync::Reason>,
}

/// What the slot lock guards. Its drop is two `None` checks and a null
/// check for a plain request.
struct Inner<R> {
    /// Written only by the reply that won the word, before the CAS that
    /// publishes `REPLIED`; taken by the receiver after it reads `REPLIED`.
    value: Option<R>,
    /// The caller's receiver: an async task's waker, or the unparker of a
    /// thread blocked in `recv_blocking` (then the word has `PARKED`).
    waker: Option<Waker>,
    side: Option<Box<Side>>,
}

pub(crate) struct Slot<R> {
    /// The receiving instance (production: its registry), whose deadline
    /// heap expires an awaited raw request.
    #[cfg_attr(not(feature = "async"), allow(dead_code))]
    pub(crate) owner: super::sync::SlotOwner,
    word: AtomicU64,
    deadline: AtomicU64,
    /// The slot lock: the value and waker handoff, and the order of a
    /// deadline shortening against a reply's deadline read.
    inner: Mutex<Inner<R>>,
}

#[inline]
fn lock<T>(mutex: &Mutex<T>) -> super::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}

/// What a winning transition must do after its CAS (the credit has already
/// returned): wake the receiver and the token's watchers.
pub(crate) struct Wake {
    waker: Option<Waker>,
    /// `waker` unparks a thread blocked in `recv_blocking`.
    parked: bool,
    watchers: Vec<Waker>,
}

impl Wake {
    const NONE: Self = Self {
        waker: None,
        parked: false,
        watchers: Vec::new(),
    };
    /// Wake a parked blocking receiver (no user code). Done first, so a
    /// parked `recv()` never waits behind an arbitrary waker.
    fn notify_parked(&mut self) {
        if std::mem::take(&mut self.parked) {
            if let Some(waker) = self.waker.take() {
                waker.wake();
            }
        }
    }
    #[inline]
    fn run(self) {
        if let Some(waker) = self.waker {
            waker.wake();
        }
        if !self.watchers.is_empty() {
            for watcher in self.watchers {
                watcher.wake();
            }
        }
    }
}

impl<R> Slot<R> {
    /// A fresh `OPEN` slot whose deadline is `deadline` ticks (`NEVER` for none).
    #[allow(dead_code)] // the Loom model and unit tests build ownerless slots
    #[allow(clippy::unit_arg)] // the owner is `()` outside trace builds
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
                side: None,
            }),
        }
    }

    #[inline]
    pub(crate) fn word(&self) -> u64 {
        self.word.load(Ordering::Acquire)
    }

    #[inline]
    pub(crate) fn deadline(&self) -> u64 {
        self.deadline.load(Ordering::Acquire)
    }

    #[inline]
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

    /// After a winning CAS `old -> new`: return the credit delta now and,
    /// when the state became terminal, collect the wake-up, which the caller
    /// runs after every lock is released.
    #[inline]
    fn after(&self, old: u64, new: u64) -> Wake {
        let release = credits(old).saturating_sub(credits(new));
        let terminal = live(state(old)) && !live(state(new));
        if release == 0 && !terminal {
            return Wake::NONE;
        }
        let mut inner = lock(&self.inner);
        Self::finish(&mut inner, release, terminal, new)
    }

    /// Under the slot lock: return `release` credits (an atomic, no user
    /// code) and, for a terminal transition, take the wake-up.
    #[inline]
    fn finish(inner: &mut Inner<R>, release: u64, terminal: bool, new: u64) -> Wake {
        if release > 0 {
            if let Some(pool) = inner.side.as_ref().and_then(|side| side.pool.as_ref()) {
                pool.release(release);
            }
        }
        if !terminal {
            return Wake::NONE;
        }
        Wake {
            waker: inner.waker.take(),
            // `PARKED` is set under this lock before a thread stores its
            // waker, so a CAS that raced it failed and retried with it.
            parked: new & PARKED != 0,
            watchers: match &mut inner.side {
                Some(side) => std::mem::take(&mut side.watchers),
                None => Vec::new(),
            },
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
                self.after(old, new).run();
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
                wake.run();
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
                // The credit returns with the transition; only wakes wait.
                return Some(self.after(old, new));
            }
            if state(self.word()) != WRITING {
                return None;
            }
            yield_now();
        }
    }

    /// The owner's close records its final reason before the `STOPPED`
    /// transition can publish it (a caller reads it after seeing `STOPPED`).
    pub(crate) fn set_stopped_reason(&self, reason: super::sync::Reason) {
        if self.is_terminal() {
            return;
        }
        lock(&self.inner)
            .side
            .get_or_insert_with(Default::default)
            .reason = Some(reason);
    }

    /// Mark the deadline as tracked by the owner's heap (before sharing).
    #[allow(dead_code)] // the Loom model has no heap
    pub(crate) fn mark_tracked(&self) {
        self.word.fetch_or(TRACKED, Ordering::Release);
    }

    /// The final reason the owner's close recorded, if it closed this slot.
    #[allow(dead_code)] // the Loom model has no reasons to read
    pub(crate) fn stopped_reason(&self) -> Option<super::sync::Reason> {
        lock(&self.inner)
            .side
            .as_ref()
            .and_then(|side| side.reason.clone())
    }

    /// Run the wake-up work [`close_collect`](Self::close_collect) returned.
    pub(crate) fn run_wake(&self, wake: Wake) {
        wake.run();
    }

    /// The parked-receiver part of `wake`, run ahead of every waker of a
    /// batch (see `Registry::end_discharge`).
    pub(crate) fn notify_parked(&self, wake: &mut Wake) {
        wake.notify_parked();
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
        let (lowered, watchers, waker) = {
            let mut inner = lock(&self.inner);
            let lower = state(self.word()) == OPEN && at < self.deadline();
            if lower {
                self.deadline.store(at, Ordering::Release);
                super::sync::shortened(&self.owner, self as *const Self as usize);
                // Moved out, not cloned: no executor code under the lock. A
                // woken receiver re-registers when it polls again.
                let watchers = match &mut inner.side {
                    Some(side) => std::mem::take(&mut side.watchers),
                    None => Vec::new(),
                };
                (true, watchers, inner.waker.take())
            } else {
                (false, Vec::new(), None)
            }
        };
        // Both sides re-arm their timers for the earlier deadline: the
        // token's waiters and the caller's receiver (a parked `recv_blocking`
        // thread recomputes its wait).
        if let Some(waker) = waker {
            waker.wake();
        }
        for watcher in watchers {
            watcher.wake();
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
        // The side state (the pool credits return to) exists before the CAS
        // can take a credit.
        lock(&self.inner)
            .side
            .get_or_insert_with(Default::default)
            .pool
            .get_or_insert_with(|| pool.clone());
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
        let pool = lock(&self.inner)
            .side
            .as_ref()
            .and_then(|side| side.pool.clone());
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
            Ok((old, new)) => self.after(old, new).run(),
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

    /// A reply from the owner's own turn (`PendingReply::reply`): nothing
    /// shortens the deadline concurrently (only a deferred token can) and the
    /// owner's death point cannot run during its own turn, so the only
    /// concurrent transitions are the caller's (abandonment, its timer's
    /// expiry), each one CAS out of `OPEN`. With the deadline still ahead,
    /// the value is written under the slot lock and published by one CAS
    /// `OPEN -> REPLIED` (the same linearization point as `OPEN -> WRITING`:
    /// a receiver reads the value under that lock); a lost CAS hands the
    /// value back. A reply at or past the deadline takes [`reply`](Self::reply).
    #[inline]
    pub(crate) fn reply_in_turn(&self, value: R, now: &impl Fn() -> u64) -> (Delivery, Option<R>) {
        let deadline = self.deadline();
        if deadline != NEVER && now() >= deadline {
            return self.reply(value, now);
        }
        let mut inner = lock(&self.inner);
        inner.value = Some(value);
        match self.cas(None, |w| {
            if state(w) == OPEN {
                Ok(with_state(w, REPLIED))
            } else {
                Err(state(w))
            }
        }) {
            Ok((old, new)) => {
                let wake = Self::finish(
                    &mut inner,
                    credits(old).saturating_sub(credits(new)),
                    true,
                    new,
                );
                drop(inner);
                wake.run();
                (Delivery::Delivered, None)
            }
            Err(lost) => {
                let value = inner.value.take();
                drop(inner);
                let delivery = match lost {
                    Some(TIMED_OUT) => Delivery::Expired,
                    Some(STOPPED) => Delivery::OwnerExited,
                    _ => Delivery::CallerGone,
                };
                (delivery, value)
            }
        }
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
        let wake = Self::finish(
            &mut inner,
            credits(old).saturating_sub(credits(new)),
            true,
            new,
        );
        drop(inner);
        wake.run();
        (delivery, unsent)
    }

    /// Receiver side: take the outcome of a terminal word. `Replied` with no
    /// value means it was already taken.
    #[inline]
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
    #[inline]
    pub(crate) fn poll(&self, waker: &Waker) -> Poll<Result<R, Terminal>> {
        if let Some(result) = self.take(state(self.word())) {
            return Poll::Ready(result);
        }
        // `Waker::clone` and dropping a waker run executor code, so neither
        // happens under the slot lock. The first registration clones before
        // locking (one lock section instead of two); a later poll with the
        // same waker finds it registered and drops the copy.
        let mut fresh: Option<Waker> = Some(waker.clone());
        loop {
            if let Some(result) = self.take(state(self.word())) {
                drop(fresh);
                return Poll::Ready(result);
            }
            let mut inner = lock(&self.inner);
            // Checked under the lock a terminal transition takes the waker
            // under: a waker stored here is taken by it.
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
    /// which expires it). Registers `waker` as a watcher otherwise.
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
            let Some(side) = &mut inner.side else {
                // Only a deferred token (or its linked job) watches its slot.
                debug_assert!(false, "poll_closed on a slot that was never deferred");
                return Poll::Ready(());
            };
            if side.watchers.iter().any(|w| w.will_wake(waker)) {
                drop(inner);
                drop(fresh);
                return Poll::Pending;
            }
            let Some(new) = fresh.take() else {
                drop(inner);
                fresh = Some(waker.clone());
                continue;
            };
            side.watchers.push(new);
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
            let deadline = self.deadline();
            let remaining = (deadline != NEVER).then(|| deadline.saturating_sub(now()));
            if remaining == Some(0) {
                continue;
            }
            {
                let mut inner = lock(&self.inner);
                if !live(state(self.word())) {
                    continue;
                }
                // Before the waker is stored (see `PARKED`).
                self.word.fetch_or(PARKED, Ordering::AcqRel);
                // The thread's own unparker (no user code to clone or drop).
                super::sync::with_thread_waker(|waker| {
                    if !inner.waker.as_ref().is_some_and(|w| w.will_wake(waker)) {
                        inner.waker = Some(waker.clone());
                    }
                });
            }
            super::sync::parked();
            // A stale unpark or a spurious return only loops.
            super::sync::park(remaining);
        }
    }

    /// The reply of a `REPLIED` slot the caller now owns alone: moved out
    /// without taking the lock (the publishing CAS, read by the caller,
    /// ordered the value's write before it, and owning the slot excludes
    /// every other access). `None` if it was already taken.
    #[allow(dead_code)] // the Loom model takes through the lock
    pub(crate) fn into_reply(self) -> Option<R> {
        self.inner
            .into_inner()
            .unwrap_or_else(|p| p.into_inner())
            .value
    }

    /// The caller dropped its receiver: `OPEN -> ABANDONED`.
    pub(crate) fn abandon(&self) -> bool {
        self.terminate(Terminal::Abandoned)
    }
}
