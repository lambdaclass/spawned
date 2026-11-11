//! The racy core of pipe-to-self jobs (docs/design/deferred-replies.md §8,
//! §9; `verification/tla/DeferredJobs.tla`): one job's cancellation flag,
//! one-shot terminal notification and emit credits, and the owner's job set
//! (key map, running count, native budget).
//!
//! Shared with the Loom model (`tests/job_core_loom.rs`), which supplies the
//! synchronization types through `super::sync`, including a model of
//! `Arc`/`Weak` that observes the final destruction of a job. The protocol
//! never runs user code under its locks: `enqueue` closures only push an
//! envelope, and every strong reference a lock section takes (a replaced key,
//! an upgraded `Weak`) is dropped after the lock is released.

use super::sync::{
    AtomicBool, AtomicUsize, Condvar, JobArc as Arc, JobWeak as Weak, Mutex, MutexGuard, Ordering,
};
use std::collections::HashMap;
use std::hash::Hash;

/// The job is cancelled or already notified: an emit was not enqueued.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the job is gone (cancelled, replaced or past its terminal notification)")]
pub struct JobGone;

/// The terminal notification: a one-shot, taken by the result, a caught
/// panic or the job deadline, and suppressed by cancellation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Notification {
    Pending,
    Notified,
    Suppressed,
}

/// A terminal notification held back until the job's scheduled emits have
/// fired (`JobCtx::emit_at`); run outside the lock.
pub(crate) type HeldTerminal = Box<dyn FnOnce() + Send>;

struct Inner {
    cancelled: bool,
    notification: Notification,
    running: bool,
    emit_credits: u16,
    /// Delayed emits (`emit_at`) holding a credit, not yet fired.
    scheduled: u16,
    /// The work's result, waiting for `scheduled` to reach 0 so the terminal
    /// message stays last.
    held: Option<HeldTerminal>,
}

impl Inner {
    /// The terminal notification can still be taken.
    fn open(&self) -> bool {
        !self.cancelled && self.notification == Notification::Pending
    }
    /// New emits are accepted: open, and the work has not handed in its
    /// result (a held terminal).
    fn accepting(&self) -> bool {
        self.open() && self.held.is_none()
    }
}

/// An emit's step 2: enqueued, or `JobGone` with the envelope the mailbox
/// refused (to drop outside locks).
pub(crate) type EmitResult<X> = Result<(), (JobGone, Option<X>)>;

/// What [`JobCore::finish`] did with the work's result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Finish {
    /// Notified now (the enqueue ran).
    Taken,
    /// Held until the scheduled emits fire.
    Held,
    /// Cancelled or already notified (deadline): nothing to deliver.
    Refused,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}

/// One job's flag, notification and emit credits.
pub(crate) struct JobCore {
    inner: Mutex<Inner>,
    emitters: Condvar,
    cancelled_fast: AtomicBool,
}

impl JobCore {
    pub(crate) fn new(emit_credits: u16) -> Self {
        Self {
            inner: Mutex::new(Inner {
                cancelled: false,
                notification: Notification::Pending,
                running: true,
                emit_credits,
                scheduled: 0,
                held: None,
            }),
            emitters: Condvar::new(),
            cancelled_fast: AtomicBool::new(false),
        }
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled_fast.load(Ordering::Acquire)
    }

    /// Set the cancellation flag: suppresses a pending notification and
    /// wakes blocked emitters. Never undoes effects. Returns whether this
    /// call set the flag. The caller wakes async emitters afterwards.
    pub(crate) fn cancel(&self) -> bool {
        let (newly, held) = {
            let mut inner = lock(&self.inner);
            let newly = !inner.cancelled;
            inner.cancelled = true;
            if inner.notification == Notification::Pending {
                inner.notification = Notification::Suppressed;
            }
            // Set under the lock, so a dispatch that sees an enqueued
            // message's job as not cancelled ran before this cancel.
            self.cancelled_fast.store(true, Ordering::Release);
            (newly, inner.held.take())
        };
        // A held result is suppressed; it drops outside the lock.
        drop(held);
        self.emitters.notify_all();
        newly
    }

    /// Take the terminal notification and run `enqueue` under the job's
    /// lock. Returns whether it was taken; `enqueue` is not called when the
    /// job was cancelled or already notified. Whatever `enqueue` hands back
    /// (an envelope the mailbox refused) is returned to be dropped by the
    /// caller after the lock is released. When taken, the caller wakes async
    /// emitters.
    pub(crate) fn notify<X>(&self, enqueue: impl FnOnce() -> Option<X>) -> (bool, Option<X>) {
        let (unsent, held) = {
            let mut inner = lock(&self.inner);
            if !inner.open() {
                return (false, None);
            }
            inner.notification = Notification::Notified;
            // A deadline overtaking a held result: the result is dropped.
            (enqueue(), inner.held.take())
        };
        drop(held);
        self.emitters.notify_all();
        (true, unsent)
    }

    /// The work's result: take the notification as [`notify`](Self::notify)
    /// does, unless scheduled emits are still pending; then store `later`
    /// (which calls `notify` again) for the last of them to run, so the
    /// terminal message stays last.
    pub(crate) fn finish<X>(
        &self,
        enqueue: impl FnOnce() -> Option<X>,
        later: impl FnOnce() -> HeldTerminal,
    ) -> (Finish, Option<X>) {
        {
            let mut inner = lock(&self.inner);
            if !inner.open() {
                return (Finish::Refused, None);
            }
            if inner.scheduled > 0 {
                inner.held = Some(later());
                return (Finish::Held, None);
            }
        }
        // Nothing scheduled, and only the work schedules: `notify` takes it
        // unless a cancel or the deadline came in between.
        match self.notify(enqueue) {
            (true, unsent) => (Finish::Taken, unsent),
            (false, _) => (Finish::Refused, None),
        }
    }

    /// Whether the terminal notification can still be taken (the job
    /// deadline is still due).
    pub(crate) fn is_pending(&self) -> bool {
        lock(&self.inner).open()
    }

    #[allow(dead_code)] // used by the Loom model
    pub(crate) fn notification(&self) -> Notification {
        lock(&self.inner).notification
    }

    #[allow(dead_code)] // used by the Loom model
    pub(crate) fn emit_credits(&self) -> u16 {
        lock(&self.inner).emit_credits
    }

    /// Execution ended (physically).
    pub(crate) fn finished(&self) {
        lock(&self.inner).running = false;
    }

    pub(crate) fn is_running(&self) -> bool {
        lock(&self.inner).running
    }

    /// Emit step 1 (blocking): take an emit credit, waiting while all are in
    /// flight. Fails with `JobGone` once the job is cancelled or notified.
    pub(crate) fn acquire_blocking(&self) -> Result<(), JobGone> {
        let mut inner = lock(&self.inner);
        loop {
            if !inner.accepting() {
                return Err(JobGone);
            }
            if inner.emit_credits > 0 {
                inner.emit_credits -= 1;
                return Ok(());
            }
            inner = self.emitters.wait(inner).unwrap_or_else(|p| p.into_inner());
        }
    }

    /// Emit step 1 (async): take a credit, or call `register` (store the
    /// waker) under the job's lock and return `None`. Registration under the
    /// lock orders it against the credit return and the cancel, which wake
    /// the registered wakers after their update.
    #[cfg_attr(not(feature = "async"), allow(dead_code))]
    pub(crate) fn try_acquire(&self, register: impl FnOnce()) -> Option<Result<(), JobGone>> {
        let mut inner = lock(&self.inner);
        if !inner.accepting() {
            return Some(Err(JobGone));
        }
        if inner.emit_credits > 0 {
            inner.emit_credits -= 1;
            return Some(Ok(()));
        }
        register();
        None
    }

    /// Emit step 2: under the job's lock, enqueue unless the job is
    /// cancelled or notified. The caller's credit guard is the only owner of
    /// the credit's return: it travels inside `enqueue` and returns the
    /// credit when that closure is dropped uncalled (refused here) or when
    /// an envelope the mailbox refused is dropped after the lock is released.
    pub(crate) fn enqueue_emit<X>(
        &self,
        enqueue: impl FnOnce() -> Result<(), X>,
    ) -> Result<(), (JobGone, Option<X>)> {
        let inner = lock(&self.inner);
        if !inner.accepting() {
            drop(inner);
            // `enqueue` (and the credit guard it owns) drops here, unlocked.
            drop(enqueue);
            return Err((JobGone, None));
        }
        enqueue().map_err(|unsent| (JobGone, Some(unsent)))
    }

    /// Delayed emit, step 2 at the call: with a credit taken, register a
    /// scheduled emit unless the job is cancelled, notified or finished.
    /// The caller's credit guard returns the credit on failure.
    pub(crate) fn schedule(&self) -> Result<(), JobGone> {
        let mut inner = lock(&self.inner);
        if !inner.accepting() {
            return Err(JobGone);
        }
        inner.scheduled += 1;
        Ok(())
    }

    /// Delayed emit, step 2 at its deadline, or when dropped unfired
    /// (`enqueue` is `None`): under the job's lock, enqueue if the
    /// notification is still pending. A message not enqueued (the refused
    /// envelope, or the uncalled `enqueue` handed back as the third value)
    /// returns its credit through its guard when the caller drops it, after
    /// the lock: it is user payload. The last scheduled emit hands back a
    /// held terminal, to run outside the lock.
    pub(crate) fn fire<X, F: FnOnce() -> Result<(), X>>(
        &self,
        enqueue: Option<F>,
    ) -> (EmitResult<X>, Option<HeldTerminal>, Option<F>) {
        let mut inner = lock(&self.inner);
        debug_assert!(inner.scheduled > 0, "scheduled emit underflow");
        inner.scheduled = inner.scheduled.saturating_sub(1);
        let (result, unused) = match enqueue {
            Some(enqueue) if inner.open() => {
                (enqueue().map_err(|unsent| (JobGone, Some(unsent))), None)
            }
            unused => (Err((JobGone, None)), unused),
        };
        let held = if inner.scheduled == 0 {
            inner.held.take()
        } else {
            None
        };
        drop(inner);
        self.emitters.notify_all();
        (result, held, unused)
    }

    #[allow(dead_code)] // used by the Loom model
    pub(crate) fn scheduled(&self) -> u16 {
        lock(&self.inner).scheduled
    }

    /// Emit step 3: a credit comes back when its message is dispatched,
    /// fenced or dropped with the mailbox. The caller wakes async emitters.
    pub(crate) fn return_emit_credit(&self) {
        lock(&self.inner).emit_credits += 1;
        self.emitters.notify_all();
    }
}

/// What the job set needs from a job.
pub(crate) trait Job {
    fn cancel(&self);
    fn is_running(&self) -> bool;
    fn finished(&self);
    /// Kept in the job list at compaction (so `cancel_all` reaches it):
    /// running, or with work left such as scheduled emits.
    fn is_live(&self) -> bool {
        self.is_running()
    }
}

/// Compact the weak job list when it reaches this length (and every power of
/// two above it).
const COMPACT_AT: usize = 32;

/// An owner instance's jobs: key map, running count, native budget.
pub(crate) struct JobSetCore<J: 'static, K> {
    jobs: Mutex<Vec<Weak<J>>>,
    keys: Mutex<HashMap<K, Arc<J>>>,
    running: Mutex<usize>,
    idle: Condvar,
    native: AtomicUsize,
    max_native: usize,
}

impl<J: Job + 'static, K: Hash + Eq> JobSetCore<J, K> {
    pub(crate) fn new(max_native: usize) -> Self {
        Self {
            jobs: Mutex::new(Vec::new()),
            keys: Mutex::new(HashMap::new()),
            running: Mutex::new(0),
            idle: Condvar::new(),
            native: AtomicUsize::new(0),
            max_native,
        }
    }

    /// Reserve a native slot (`MAX_NATIVE_JOBS`), rejecting at the bound.
    pub(crate) fn reserve_native(&self) -> bool {
        self.native
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < self.max_native).then_some(n + 1)
            })
            .is_ok()
    }

    pub(crate) fn release_native(&self) {
        let previous = self.native.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "native job budget underflow");
    }

    #[allow(dead_code)] // used by the Loom model
    pub(crate) fn native(&self) -> usize {
        self.native.load(Ordering::Acquire)
    }

    /// Admit a job: replace (cancel) the previous one with the same key,
    /// whatever its notification state, and count it as running. The
    /// replaced job and the references compaction upgraded are dropped after
    /// the locks are released.
    pub(crate) fn admit(&self, state: &Arc<J>, key: Option<K>) {
        let previous = key.and_then(|key| lock(&self.keys).insert(key, state.clone()));
        if let Some(previous) = &previous {
            previous.cancel();
        }
        let mut upgraded: Vec<Arc<J>> = Vec::new();
        {
            let mut jobs = lock(&self.jobs);
            if jobs.len() >= COMPACT_AT && jobs.len().is_power_of_two() {
                jobs.retain(|job| match job.upgrade() {
                    Some(job) => {
                        let keep = job.is_live();
                        upgraded.push(job);
                        keep
                    }
                    None => false,
                });
            }
            jobs.push(Arc::downgrade(state));
        }
        *lock(&self.running) += 1;
        drop(upgraded);
        drop(previous);
    }

    /// A job's execution ended. Returns whether the set became idle (the
    /// caller wakes async joiners).
    pub(crate) fn ended(&self, state: &J) -> bool {
        state.finished();
        // The key keeps its finished job until replaced: a replacement must
        // still cancel it, so its queued terminal message and emits are
        // fenced (`NoStaleKeyed`). The map holds one job per key.
        let idle = {
            let mut running = lock(&self.running);
            *running -= 1;
            *running == 0
        };
        if idle {
            self.idle.notify_all();
        }
        idle
    }

    pub(crate) fn running(&self) -> usize {
        *lock(&self.running)
    }

    /// Owner exit: cancel every job (flag set, blocked emitters woken). The
    /// key map's references and the upgraded jobs drop outside the locks.
    pub(crate) fn cancel_all(&self) {
        let jobs = std::mem::take(&mut *lock(&self.jobs));
        let keys = std::mem::take(&mut *lock(&self.keys));
        for job in jobs.iter().filter_map(Weak::upgrade) {
            job.cancel();
        }
        drop(keys);
    }

    /// Wait until no job is running. `wait` parks on the idle condition
    /// variable and returns `Err` with the guard to give up (the cleanup
    /// deadline passed). Returns how many jobs are still running.
    pub(crate) fn join_with<'a>(
        &'a self,
        mut wait: impl FnMut(
            &'a Condvar,
            MutexGuard<'a, usize>,
        ) -> Result<MutexGuard<'a, usize>, MutexGuard<'a, usize>>,
    ) -> usize {
        let mut running = lock(&self.running);
        while *running > 0 {
            match wait(&self.idle, running) {
                Ok(guard) => running = guard,
                Err(guard) => return *guard,
            }
        }
        0
    }

    /// Take and release the set's locks: the Loom model calls it from a
    /// job's final destruction, which deadlocks if that destruction runs
    /// under one of them.
    #[allow(dead_code)] // used by the Loom model
    pub(crate) fn probe_locks(&self) {
        drop(lock(&self.jobs));
        drop(lock(&self.keys));
        drop(lock(&self.running));
    }
}
