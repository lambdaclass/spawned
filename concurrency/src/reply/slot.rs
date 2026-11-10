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

/// Outcome of a reply, as the replier sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    pub(crate) fn max(&self) -> usize {
        self.max
    }
}

struct Inner<R> {
    /// Written only by the reply that moved the word to `WRITING`, and
    /// published by `WRITING -> REPLIED` under this lock.
    value: Option<R>,
    waker: Option<Waker>,
    /// The owner's pool, set by `defer()` before its CAS installs `TOKEN`.
    pool: Option<Arc<CreditPool>>,
    /// Threads parked in `recv_blocking`.
    parked: usize,
}

pub(crate) struct Slot<R> {
    word: AtomicU64,
    deadline: AtomicU64,
    inner: Mutex<Inner<R>>,
    parked: Condvar,
}

fn lock<T>(mutex: &Mutex<T>) -> super::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}

/// What a winning transition must do after its CAS: return credits, then wake.
struct Wake {
    pool: Option<Arc<CreditPool>>,
    release: u64,
    waker: Option<Waker>,
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
        if self.notify {
            slot_parked.notify_all();
        }
    }
}

impl<R> Slot<R> {
    /// A fresh `OPEN` slot whose deadline is `deadline` ticks (`NEVER` for none).
    pub(crate) fn new(deadline: u64) -> Self {
        Self {
            word: AtomicU64::new(OPEN),
            deadline: AtomicU64::new(deadline),
            inner: Mutex::new(Inner {
                value: None,
                waker: None,
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
                    _ => return Err(None),
                }
            }
            match self
                .word
                .compare_exchange(current, next, Ordering::AcqRel, Ordering::Acquire)
            {
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
                notify: false,
            };
        }
        let mut inner = lock(&self.inner);
        Wake {
            pool: if release > 0 { inner.pool.clone() } else { None },
            release,
            waker: if terminal { inner.waker.take() } else { None },
            notify: terminal && inner.parked > 0,
        }
    }

    /// `OPEN -> to` (drop, overloaded, expiry, abandonment, registry close).
    /// Returns whether this call won the transition.
    pub(crate) fn terminate(&self, to: Terminal) -> bool {
        debug_assert!(to != Terminal::Replied);
        let target = to.state();
        match self.cas::<()>(None, |w| {
            if state(w) == OPEN {
                Ok(with_state(w, target))
            } else {
                Err(())
            }
        }) {
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
        loop {
            if self.terminate(Terminal::Stopped) {
                return true;
            }
            if state(self.word()) != WRITING {
                return false;
            }
            yield_now();
        }
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
    pub(crate) fn shorten(&self, at: u64, now: &impl Fn() -> u64) {
        let _ = self
            .deadline
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |d| (at < d).then_some(at));
        self.check_deadline(now);
    }

    /// `defer()`: `OPEN, TOKEN=0 -> OPEN, TOKEN=1, REG=1`, reserving the
    /// token's credit when no linked job holds one. At the bound the caller
    /// is answered `Overloaded` in the same step.
    pub(crate) fn defer(&self, pool: &Arc<CreditPool>, now: &impl Fn() -> u64) -> Result<(), Refused> {
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
            Err(None) => {
                self.terminate(Terminal::Overloaded);
                Err(Refused::Overloaded)
            }
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
        if let Err(Some(s)) = self.cas(None, |w| {
            if state(w) == OPEN {
                Ok(with_state(w, WRITING))
            } else {
                Err(state(w))
            }
        }) {
            let delivery = match s {
                TIMED_OUT => Delivery::Expired,
                STOPPED => Delivery::OwnerExited,
                _ => Delivery::CallerGone,
            };
            return (delivery, Some(value));
        }
        let deadline = self.deadline();
        if deadline != NEVER && now() >= deadline {
            let (old, new) = self.resolve_writing(TIMED_OUT);
            self.after(old, new).run(&self.parked);
            return (Delivery::Expired, Some(value));
        }
        let wake = {
            let mut inner = lock(&self.inner);
            inner.value = Some(value);
            let (old, new) = self.resolve_writing(REPLIED);
            let release = credits(old).saturating_sub(credits(new));
            Wake {
                pool: if release > 0 { inner.pool.clone() } else { None },
                release,
                waker: inner.waker.take(),
                notify: inner.parked > 0,
            }
        };
        wake.run(&self.parked);
        (Delivery::Delivered, None)
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

    /// Poll from the caller's receiver. Expires the slot when its deadline has
    /// passed; otherwise registers `waker` and returns `Pending`.
    pub(crate) fn poll(&self, waker: &Waker, now: &impl Fn() -> u64) -> Poll<Result<R, Terminal>> {
        loop {
            if let Some(result) = self.take(state(self.word())) {
                return Poll::Ready(result);
            }
            if self.check_deadline(now) {
                continue;
            }
            let mut inner = lock(&self.inner);
            let w = self.word();
            if !live(state(w)) {
                drop(inner);
                continue;
            }
            match &inner.waker {
                Some(old) if old.will_wake(waker) => {}
                _ => {
                    let old = inner.waker.replace(waker.clone());
                    drop(inner);
                    drop(old);
                }
            }
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
