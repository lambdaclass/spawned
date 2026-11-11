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
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

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

pub use super::job_core::JobGone;
use super::job_core::{Job, JobCore, JobSetCore};
use crate::lifecycle_trace as trace;

/// One job's shared state: the core protocol plus the async wake-ups.
pub(crate) struct JobState {
    owner: ActorId,
    core: JobCore,
    /// The job's number in lifecycle traces.
    #[cfg(feature = "lifecycle-trace")]
    trace: u64,
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
            core: JobCore::new(emit_credits),
            #[cfg(feature = "lifecycle-trace")]
            trace: crate::lifecycle_trace::next_job(),
            #[cfg(feature = "async")]
            cancel: spawned_rt::tasks::CancellationToken::new(),
            #[cfg(feature = "async")]
            emit_wakers: Mutex::new(Vec::new()),
        })
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.core.is_cancelled()
    }

    #[cfg(feature = "async")]
    pub(crate) fn cancellation(&self) -> &spawned_rt::tasks::CancellationToken {
        &self.cancel
    }

    /// Record a `DeferredJobs.tla` action of this job (lifecycle traces).
    #[cfg_attr(not(feature = "lifecycle-trace"), allow(unused_variables))]
    fn record(&self, event: impl FnOnce(u64) -> trace::JobEvent) {
        #[cfg(feature = "lifecycle-trace")]
        trace::job(self.owner.raw(), || event(self.trace));
    }

    /// Set the cancellation flag: suppresses a pending notification and
    /// wakes blocked emitters. Never undoes effects. Recorded as the model's
    /// `Cancel` (the owner's exit and key replacement cancel through the job
    /// set instead, as part of `Exit` and `Start`).
    pub(crate) fn cancel(&self) {
        trace::atomic(|| {
            if self.core.cancel() {
                self.record(|job| trace::JobEvent::new("Cancel", job));
            }
        });
        self.wake_cancelled();
    }

    fn cancel_quietly(&self) {
        self.core.cancel();
        self.wake_cancelled();
    }

    fn wake_cancelled(&self) {
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
    /// lock (see [`JobCore::notify`]). `deadline` tells the job deadline from
    /// the end of execution (the model's `JobDeadline` and `Finish`).
    pub(crate) fn notify<X>(
        &self,
        deadline: bool,
        enqueue: impl FnOnce() -> Option<X>,
    ) -> (bool, Option<X>) {
        let (taken, unsent) = trace::atomic(|| {
            let (taken, unsent) = self.core.notify(enqueue);
            if !deadline {
                self.record(|job| trace::JobEvent {
                    x: if taken { "notified" } else { "quiet" },
                    ..trace::JobEvent::new("Finish", job)
                });
            } else if taken {
                self.record(|job| trace::JobEvent::new("JobDeadline", job));
            }
            (taken, unsent)
        });
        #[cfg(feature = "async")]
        if taken {
            self.wake_emitters();
        }
        (taken, unsent)
    }

    pub(crate) fn is_running(&self) -> bool {
        self.core.is_running()
    }

    /// Execution of a cancelled async job ended without a notification (the
    /// model's `Finish` when the notification was suppressed).
    #[cfg(feature = "async")]
    pub(crate) fn ended_cancelled(&self) {
        self.record(|job| trace::JobEvent {
            x: "quiet",
            ..trace::JobEvent::new("Finish", job)
        });
    }

    /// The dispatch fence: a message of this job dispatched by instance `at`
    /// is dropped when the job is cancelled or belongs to another instance.
    /// `kind` is `emit`, `done` or `deadline` (the model's `Dispatch`).
    pub(crate) fn fenced(&self, at: ActorId, kind: &'static str) -> bool {
        trace::atomic(|| {
            let fenced = self.is_cancelled() || self.owner != at;
            self.record(|job| trace::JobEvent {
                x: kind,
                fenced,
                ..trace::JobEvent::new("Dispatch", job)
            });
            fenced
        })
    }

    /// Emit step 1 (blocking).
    pub(crate) fn acquire_blocking(&self) -> Result<(), JobGone> {
        self.core.acquire_blocking()
    }

    /// Emit step 1 (async).
    #[cfg(feature = "async")]
    pub(crate) fn poll_acquire(
        &self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), JobGone>> {
        let acquired = self.core.try_acquire(|| {
            let mut wakers = lock(&self.emit_wakers);
            if !wakers.iter().any(|w| w.will_wake(cx.waker())) {
                wakers.push(cx.waker().clone());
            }
        });
        match acquired {
            Some(result) => std::task::Poll::Ready(result),
            None => std::task::Poll::Pending,
        }
    }

    /// Emit step 2 (see [`JobCore::enqueue_emit`]).
    pub(crate) fn enqueue_emit<X>(
        &self,
        enqueue: impl FnOnce() -> Result<(), X>,
    ) -> Result<(), (JobGone, Option<X>)> {
        trace::atomic(|| {
            let result = self.core.enqueue_emit(enqueue);
            if result.is_ok() {
                self.record(|job| trace::JobEvent::new("Emit", job));
            }
            result
        })
    }

    /// Emit step 3: a credit comes back when its message is dispatched,
    /// fenced or dropped with the mailbox.
    pub(crate) fn return_emit_credit(&self) {
        self.core.return_emit_credit();
        #[cfg(feature = "async")]
        self.wake_emitters();
    }
}

impl Job for JobState {
    fn cancel(&self) {
        self.cancel_quietly();
    }
    fn is_running(&self) -> bool {
        self.core.is_running()
    }
    fn finished(&self) {
        self.core.finished();
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
    core: JobSetCore<JobState, JobKey>,
    /// Whether the owner's exit was recorded (lifecycle traces).
    #[cfg(feature = "lifecycle-trace")]
    exited: std::sync::atomic::AtomicBool,
    #[cfg(feature = "async")]
    idle_async: Mutex<Vec<std::task::Waker>>,
}

impl JobSet {
    pub(crate) fn new(owner: ActorId, max_native: usize) -> Self {
        Self {
            owner,
            core: JobSetCore::new(max_native),
            #[cfg(feature = "lifecycle-trace")]
            exited: std::sync::atomic::AtomicBool::new(false),
            #[cfg(feature = "async")]
            idle_async: Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn owner(&self) -> ActorId {
        self.owner
    }

    /// Reserve a native slot (`MAX_NATIVE_JOBS`), rejecting at the bound.
    pub(crate) fn reserve_native(&self) -> bool {
        self.core.reserve_native()
    }

    pub(crate) fn release_native(&self) {
        self.core.release_native();
    }

    /// Admit a job: replace (cancel) the previous one with the same key and
    /// count it as running (the model's `Start`).
    pub(crate) fn admit(&self, state: &Arc<JobState>, key: Option<JobKey>, deadline: bool) {
        trace::atomic(|| {
            let keyed = key.is_some();
            self.core.admit(state, key);
            state.record(|job| trace::JobEvent {
                keyed,
                deadline,
                ..trace::JobEvent::new("Start", job)
            });
        });
    }

    /// A job's execution ended.
    pub(crate) fn ended(&self, state: &Arc<JobState>) {
        if self.core.ended(state) {
            #[cfg(feature = "async")]
            for waker in std::mem::take(&mut *lock(&self.idle_async)) {
                waker.wake();
            }
        }
    }

    pub(crate) fn running(&self) -> usize {
        self.core.running()
    }

    /// Owner exit: cancel every job (flag set, blocked emitters woken).
    pub(crate) fn cancel_all(&self) {
        trace::atomic(|| {
            self.core.cancel_all();
            #[cfg(feature = "lifecycle-trace")]
            if !self.exited.swap(true, std::sync::atomic::Ordering::AcqRel) {
                trace::job(self.owner.raw(), || trace::JobEvent::new("Exit", 0));
            }
        });
    }

    /// Join the cancelled jobs, bounded by `deadline`; returns how many are
    /// still running (unfinished at stop).
    pub(crate) fn join_blocking(&self, deadline: std::time::Instant) -> usize {
        self.core.join_with(|idle, running| {
            let now = std::time::Instant::now();
            if now >= deadline {
                return Err(running);
            }
            Ok(idle
                .wait_timeout(running, deadline - now)
                .unwrap_or_else(|p| p.into_inner())
                .0)
        })
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
        self.state.core.is_pending().then_some(self.at)
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
        assert_eq!(
            registry.outstanding(),
            1,
            "a rejected job released a credit"
        );
        drop(held);
        assert_eq!(registry.outstanding(), 0);
    }

    /// Review P1: a finished keyed job stays replaceable, so its queued
    /// messages are fenced when a newer job takes the key.
    #[test]
    fn replacement_cancels_a_finished_keyed_job() {
        let set = JobSet::new(ActorId::next(), 1);
        let old = JobState::new(set.owner(), 1);
        set.admit(&old, Some(JobKey::new("k")), false);
        set.ended(&old);
        let new = JobState::new(set.owner(), 1);
        set.admit(&new, Some(JobKey::new("k")), false);
        assert!(old.is_cancelled(), "the finished job escaped replacement");
        assert!(!new.is_cancelled());
    }
}
