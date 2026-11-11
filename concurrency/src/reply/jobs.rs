//! Pipe-to-self jobs: the mode-independent core
//! (docs/design/deferred-replies.md §8, §9).
//!
//! Each job has one shared [`JobState`] with three parts:
//!
//! - **execution** (running or finished), physical: cancelled or timed-out
//!   work may keep running, and keeps its credit (or native slot) until it
//!   ends;
//! - **cancellation**, a flag set by `JobHandle::cancel`, a newer job with the
//!   same key, owner exit, or (opt-in) the linked caller leaving; it wakes
//!   blocked emitters and suppresses later results;
//! - **terminal notification** (pending, notified, suppressed): a one-shot
//!   under the job's lock. The result, a caught panic, or the job deadline
//!   takes it and enqueues the terminal message; emits check the same lock,
//!   so no emit follows the terminal message.
//!
//! Messages a job produces carry its `JobState` and the owner's `ActorId` and
//! are fenced at dispatch when the job is cancelled.

use super::deadline::Expirable;
use super::registry::Registry;
use super::slot::CreditPool;
use crate::ActorId;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};

/// Default emit credits per job (`JobOptions::emit_credits`).
pub const DEFAULT_EMIT_CREDITS: u16 = 16;

/// Default `Actor::MAX_NATIVE_JOBS`: `min(4, available_parallelism)`.
pub fn default_max_native_jobs() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(4)
}

/// A key for cancel-previous jobs (`pipe_keyed`, `JobOptions::key`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct JobKey(std::borrow::Cow<'static, str>);

impl JobKey {
    pub const fn new(name: &'static str) -> Self {
        Self(std::borrow::Cow::Borrowed(name))
    }
    pub fn owned(name: String) -> Self {
        Self(std::borrow::Cow::Owned(name))
    }
}
impl From<&'static str> for JobKey {
    fn from(name: &'static str) -> Self {
        Self::new(name)
    }
}

/// Options for `pipe_with`, `pipe_blocking_with`, `pipe_reply_with` and
/// `pipe_reply_blocking_with`. Passed first; never chained afterwards.
#[derive(Debug, Clone)]
pub struct JobOptions {
    /// Deliver `JobError::DeadlineExceeded` at this instant if the job has
    /// not finished. The work keeps running and keeps its credit.
    pub deadline: Option<spawned_rt::time::Instant>,
    /// Cancel the previous job with this key (whatever its notification
    /// state) and fence its queued messages.
    pub key: Option<JobKey>,
    /// `pipe_reply` only: cancel the job when its caller leaves or its
    /// deadline passes. Off by default, as in OTP.
    pub cancel_on_caller_gone: bool,
    /// Emits in flight before `emit` waits (blocks in `emit_blocking`).
    pub emit_credits: u16,
}

impl Default for JobOptions {
    fn default() -> Self {
        Self {
            deadline: None,
            key: None,
            cancel_on_caller_gone: false,
            emit_credits: DEFAULT_EMIT_CREDITS,
        }
    }
}

/// How a job ended, delivered to `done` in the owner's turn.
#[derive(Debug)]
pub enum JobOutcome<T> {
    Ok(T),
    Err(JobError),
}

impl<T> JobOutcome<T> {
    pub fn into_result(self) -> Result<T, JobError> {
        match self {
            Self::Ok(value) => Ok(value),
            Self::Err(error) => Err(error),
        }
    }
}

/// Why a job produced no value.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum JobError {
    /// The work panicked; caught at the job boundary.
    #[error("job panicked: {0}")]
    Panicked(String),
    /// `JobOptions::deadline` passed first. The work may still be running.
    #[error("job deadline exceeded")]
    DeadlineExceeded,
}

/// Why a job was not spawned.
#[derive(Debug, thiserror::Error)]
pub enum PipeErrorKind {
    /// The owner's `MAX_OUTSTANDING` credits are in use.
    #[error("deferred credits at the bound")]
    Overloaded,
    /// The owner's `MAX_NATIVE_JOBS` native jobs are running.
    #[error("native job budget at the bound")]
    NativeBudget,
    /// The native thread could not be spawned.
    #[error("spawning the job failed: {0}")]
    SpawnFailed(std::io::Error),
    /// The owner is exiting.
    #[error("the owner is stopping")]
    OwnerStopping,
}

/// `pipe*` failed; no job was spawned and any reserved credit was released.
#[derive(Debug, thiserror::Error)]
#[error("{kind}")]
pub struct PipeError {
    pub kind: PipeErrorKind,
}

/// `pipe_reply*` failed; the token comes back in every case.
#[derive(Debug)]
pub enum PipeReplyError<R: Send + 'static> {
    Failed {
        token: super::ReplyTo<R>,
        kind: PipeErrorKind,
    },
    /// The request is no longer open (or past its deadline).
    Closed(super::ReplyTo<R>),
    /// `token.owner()` is not this actor.
    WrongOwner(super::ReplyTo<R>),
}

impl<R: Send + 'static> PipeReplyError<R> {
    /// The token handed back.
    pub fn into_token(self) -> super::ReplyTo<R> {
        match self {
            Self::Failed { token, .. } | Self::Closed(token) | Self::WrongOwner(token) => token,
        }
    }
}

/// The job is cancelled or already notified: an emit was not enqueued.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the job is gone (cancelled, replaced or past its terminal notification)")]
pub struct JobGone;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Notification {
    Pending,
    Notified,
    Suppressed,
}

struct Inner {
    cancelled: bool,
    notification: Notification,
    running: bool,
    emit_credits: u16,
}

/// One job's shared state.
pub(crate) struct JobState {
    owner: ActorId,
    inner: Mutex<Inner>,
    emitters: Condvar,
    cancelled_fast: AtomicBool,
    /// Async cancellation for `tasks` jobs (drops the work future).
    #[cfg(feature = "async")]
    cancel: spawned_rt::tasks::CancellationToken,
    /// Wakers of async emitters waiting for a credit.
    #[cfg(feature = "async")]
    emit_wakers: Mutex<Vec<std::task::Waker>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}

impl JobState {
    pub(crate) fn new(owner: ActorId, emit_credits: u16) -> Arc<Self> {
        Arc::new(Self {
            owner,
            inner: Mutex::new(Inner {
                cancelled: false,
                notification: Notification::Pending,
                running: true,
                emit_credits,
            }),
            emitters: Condvar::new(),
            cancelled_fast: AtomicBool::new(false),
            #[cfg(feature = "async")]
            cancel: spawned_rt::tasks::CancellationToken::new(),
            #[cfg(feature = "async")]
            emit_wakers: Mutex::new(Vec::new()),
        })
    }

    pub(crate) fn owner(&self) -> ActorId {
        self.owner
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled_fast.load(Ordering::Acquire)
    }

    #[cfg(feature = "async")]
    pub(crate) fn cancellation(&self) -> &spawned_rt::tasks::CancellationToken {
        &self.cancel
    }

    /// Set the cancellation flag: suppresses a pending notification and
    /// wakes blocked emitters. Never undoes effects.
    pub(crate) fn cancel(&self) {
        {
            let mut inner = lock(&self.inner);
            inner.cancelled = true;
            if inner.notification == Notification::Pending {
                inner.notification = Notification::Suppressed;
            }
        }
        self.cancelled_fast.store(true, Ordering::Release);
        self.emitters.notify_all();
        #[cfg(feature = "async")]
        {
            self.cancel.cancel();
            self.wake_emitters();
        }
    }

    #[cfg(feature = "async")]
    fn wake_emitters(&self) {
        let wakers = std::mem::take(&mut *lock(&self.emit_wakers));
        for waker in wakers {
            waker.wake();
        }
    }

    /// Take the terminal notification and run `enqueue` under the job's
    /// lock. Returns whether it was taken; `enqueue` is not called when the
    /// job was cancelled or already notified. Whatever `enqueue` hands back
    /// (an envelope the mailbox refused) is returned to be dropped by the
    /// caller after the lock is released.
    pub(crate) fn notify<X>(&self, enqueue: impl FnOnce() -> Option<X>) -> (bool, Option<X>) {
        let unsent = {
            let mut inner = lock(&self.inner);
            if inner.cancelled || inner.notification != Notification::Pending {
                return (false, None);
            }
            inner.notification = Notification::Notified;
            enqueue()
        };
        self.emitters.notify_all();
        #[cfg(feature = "async")]
        self.wake_emitters();
        (true, unsent)
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
            if inner.cancelled || inner.notification != Notification::Pending {
                return Err(JobGone);
            }
            if inner.emit_credits > 0 {
                inner.emit_credits -= 1;
                return Ok(());
            }
            inner = self.emitters.wait(inner).unwrap_or_else(|p| p.into_inner());
        }
    }

    /// Emit step 1 (async).
    #[cfg(feature = "async")]
    pub(crate) fn poll_acquire(
        &self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), JobGone>> {
        let mut inner = lock(&self.inner);
        if inner.cancelled || inner.notification != Notification::Pending {
            return std::task::Poll::Ready(Err(JobGone));
        }
        if inner.emit_credits > 0 {
            inner.emit_credits -= 1;
            return std::task::Poll::Ready(Ok(()));
        }
        lock(&self.emit_wakers).push(cx.waker().clone());
        std::task::Poll::Pending
    }

    /// Emit step 2: under the job's lock, enqueue unless the job is
    /// cancelled or notified (then the credit returns here). An envelope the
    /// mailbox refused comes back to be dropped after the lock is released;
    /// its credit guard returns the credit then.
    pub(crate) fn enqueue_emit<X>(
        &self,
        enqueue: impl FnOnce() -> Result<(), X>,
    ) -> Result<(), (JobGone, Option<X>)> {
        let mut inner = lock(&self.inner);
        if inner.cancelled || inner.notification != Notification::Pending {
            inner.emit_credits += 1;
            drop(inner);
            self.emitters.notify_all();
            return Err((JobGone, None));
        }
        enqueue().map_err(|unsent| (JobGone, Some(unsent)))
    }

    /// Emit step 3: a credit comes back when its message is dispatched,
    /// fenced or dropped with the mailbox.
    pub(crate) fn return_emit_credit(&self) {
        lock(&self.inner).emit_credits += 1;
        self.emitters.notify_all();
        #[cfg(feature = "async")]
        self.wake_emitters();
    }
}

/// A handle to a spawned job. Dropping it leaves the job owned by the
/// actor's job set (it is not detached).
#[derive(Clone)]
pub struct JobHandle {
    state: Arc<JobState>,
}

impl std::fmt::Debug for JobHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobHandle")
            .field("owner", &self.state.owner)
            .field("cancelled", &self.state.is_cancelled())
            .finish_non_exhaustive()
    }
}

impl JobHandle {
    pub(crate) fn new(state: Arc<JobState>) -> Self {
        Self { state }
    }
    /// Cancel the job: its later results and queued messages are fenced, and
    /// blocked emitters return `JobGone`. Work already done is not undone.
    pub fn cancel(&self) {
        self.state.cancel();
    }
    pub fn is_cancelled(&self) -> bool {
        self.state.is_cancelled()
    }
    /// Whether the job's work is still executing (cancelled work may still
    /// run until it returns).
    pub fn is_running(&self) -> bool {
        self.state.is_running()
    }
}

// ---------------------------------------------------------------------------
// Job set
// ---------------------------------------------------------------------------

/// An owner instance's jobs: key map, running count, native budget.
pub(crate) struct JobSet {
    owner: ActorId,
    jobs: Mutex<Vec<Weak<JobState>>>,
    keys: Mutex<HashMap<JobKey, Arc<JobState>>>,
    running: Mutex<usize>,
    idle: Condvar,
    native: AtomicUsize,
    max_native: usize,
    #[cfg(feature = "async")]
    idle_async: Mutex<Vec<std::task::Waker>>,
}

impl JobSet {
    pub(crate) fn new(owner: ActorId, max_native: usize) -> Self {
        Self {
            owner,
            jobs: Mutex::new(Vec::new()),
            keys: Mutex::new(HashMap::new()),
            running: Mutex::new(0),
            idle: Condvar::new(),
            native: AtomicUsize::new(0),
            max_native,
            #[cfg(feature = "async")]
            idle_async: Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn owner(&self) -> ActorId {
        self.owner
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
        self.native.fetch_sub(1, Ordering::AcqRel);
    }

    /// Admit a job: replace (cancel) the previous one with the same key and
    /// count it as running.
    pub(crate) fn admit(&self, state: &Arc<JobState>, key: Option<JobKey>) {
        if let Some(key) = key {
            let previous = lock(&self.keys).insert(key, state.clone());
            if let Some(previous) = previous {
                previous.cancel();
            }
        }
        {
            let mut jobs = lock(&self.jobs);
            if jobs.len() >= 32 && jobs.len().is_power_of_two() {
                jobs.retain(|j| j.upgrade().is_some_and(|j| j.is_running()));
            }
            jobs.push(Arc::downgrade(state));
        }
        *lock(&self.running) += 1;
    }

    /// A job's execution ended.
    pub(crate) fn ended(&self, state: &Arc<JobState>) {
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
            #[cfg(feature = "async")]
            for waker in std::mem::take(&mut *lock(&self.idle_async)) {
                waker.wake();
            }
        }
    }

    pub(crate) fn running(&self) -> usize {
        *lock(&self.running)
    }

    /// Owner exit: cancel every job (flag set, blocked emitters woken).
    pub(crate) fn cancel_all(&self) {
        let jobs = std::mem::take(&mut *lock(&self.jobs));
        lock(&self.keys).clear();
        for job in jobs.iter().filter_map(Weak::upgrade) {
            job.cancel();
        }
    }

    /// Join the cancelled jobs, bounded by `deadline`; returns how many are
    /// still running (unfinished at stop).
    pub(crate) fn join_blocking(&self, deadline: std::time::Instant) -> usize {
        let mut running = lock(&self.running);
        while *running > 0 {
            let now = std::time::Instant::now();
            if now >= deadline {
                break;
            }
            running = self
                .idle
                .wait_timeout(running, deadline - now)
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
        *running
    }

    /// Async join, bounded by `timeout` on the runtime's clock.
    #[cfg(feature = "async")]
    pub(crate) async fn join(&self, timeout: std::time::Duration) -> usize {
        let idle = std::future::poll_fn(|cx| {
            if self.running() == 0 {
                return std::task::Poll::Ready(());
            }
            lock(&self.idle_async).push(cx.waker().clone());
            if self.running() == 0 {
                return std::task::Poll::Ready(());
            }
            std::task::Poll::Pending
        });
        let _ = spawned_rt::tasks::timeout(timeout, idle).await;
        self.running()
    }
}

/// Report jobs still running when the owner's cleanup deadline passed.
pub(crate) fn report_unfinished(owner: ActorId, unfinished: usize) {
    if unfinished > 0 {
        tracing::warn!(
            event = "unfinished_at_stop",
            actor = %owner,
            unfinished,
            "job cleanup incomplete: jobs still running at the cleanup deadline"
        );
    }
}

// ---------------------------------------------------------------------------
// Credits and deadlines
// ---------------------------------------------------------------------------

/// The credit an unlinked job holds while it executes.
pub(crate) struct JobCredit(Option<Arc<CreditPool>>);

impl JobCredit {
    pub(crate) fn reserve(registry: &Registry) -> Option<Self> {
        let pool = registry.pool().clone();
        // `then`, not `then_some`: the guard must exist only when reserved.
        pool.reserve(1).then(|| Self(Some(pool)))
    }
}

impl Drop for JobCredit {
    fn drop(&mut self) {
        if let Some(pool) = self.0.take() {
            pool.release(1);
        }
    }
}

/// The deadline notification of one job, serviced by the owner's heap.
pub(crate) struct JobDeadline {
    pub(crate) at: u64,
    pub(crate) state: Arc<JobState>,
    pub(crate) notify: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl Expirable for JobDeadline {
    fn pending_deadline(&self) -> Option<u64> {
        let pending = {
            let inner = lock(&self.state.inner);
            !inner.cancelled && inner.notification == Notification::Pending
        };
        pending.then_some(self.at)
    }
    fn expire(&self, _now: u64) {
        let notify = lock(&self.notify).take();
        if let Some(notify) = notify {
            notify();
        }
    }
}

/// Threads-mode cleanup completion (death plus the bounded job join), for
/// `join()`. Set when the guard drops, so a panicking cleanup also releases
/// joiners.
#[derive(Default)]
pub(crate) struct Cleaned {
    done: Mutex<bool>,
    changed: Condvar,
}

impl Cleaned {
    pub(crate) fn wait(&self) {
        let mut done = lock(&self.done);
        while !*done {
            done = self.changed.wait(done).unwrap_or_else(|p| p.into_inner());
        }
    }
}

pub(crate) struct CleanedGuard(pub(crate) Arc<Cleaned>);

impl Drop for CleanedGuard {
    fn drop(&mut self) {
        *lock(&self.0.done) = true;
        self.0.changed.notify_all();
    }
}

/// Opt-in caller-give-up cancellation for jobs that cannot await
/// (`pipe_reply_blocking*` with `cancel_on_caller_gone`): a watcher waker on
/// the linked slot cancels the job when the slot closes.
pub(crate) fn cancel_when_closed<R>(slot: &super::slot::Slot<R>, state: &Arc<JobState>) {
    struct CancelJob(Arc<JobState>);
    impl std::task::Wake for CancelJob {
        fn wake(self: Arc<Self>) {
            self.0.cancel();
        }
    }
    let waker = std::task::Waker::from(Arc::new(CancelJob(state.clone())));
    if slot.poll_closed(&waker, &super::now_ticks).is_ready() {
        state.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reply::registry::Registry;

    /// Review P1: a rejected reservation must not build a guard whose drop
    /// releases a credit that was never taken.
    #[test]
    fn a_rejected_credit_releases_nothing() {
        let registry = Registry::new(ActorId::next(), 1);
        let held = JobCredit::reserve(&registry).expect("first credit");
        assert!(JobCredit::reserve(&registry).is_none());
        assert_eq!(registry.outstanding(), 1, "a rejected job released a credit");
        drop(held);
        assert_eq!(registry.outstanding(), 0);
    }

    /// Review P1: a finished keyed job stays replaceable, so its queued
    /// messages are fenced when a newer job takes the key.
    #[test]
    fn replacement_cancels_a_finished_keyed_job() {
        let set = JobSet::new(ActorId::next(), 1);
        let old = JobState::new(set.owner(), 1);
        set.admit(&old, Some(JobKey::new("k")));
        set.ended(&old);
        let new = JobState::new(set.owner(), 1);
        set.admit(&new, Some(JobKey::new("k")));
        assert!(old.is_cancelled(), "the finished job escaped replacement");
        assert!(!new.is_cancelled());
    }
}
