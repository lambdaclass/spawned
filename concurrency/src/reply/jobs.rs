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

/// Per-instance overrides of `Actor::MAX_OUTSTANDING` and
/// `Actor::MAX_NATIVE_JOBS` (set per `ChildSpec`); `None` keeps the actor's
/// constant.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Bounds {
    pub(crate) max_outstanding: Option<usize>,
    pub(crate) max_native_jobs: Option<usize>,
}

impl Bounds {
    /// The effective `(max_outstanding, max_native_jobs)` for an actor whose
    /// constants are `outstanding` and `native`.
    pub(crate) fn resolve(self, outstanding: usize, native: Option<usize>) -> (usize, usize) {
        (
            self.max_outstanding.unwrap_or(outstanding),
            self.max_native_jobs
                .or(native)
                .unwrap_or_else(default_max_native_jobs),
        )
    }
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

impl JobOptions {
    /// Set [`deadline`](Self::deadline) to `timeout` from now, on the
    /// runtime's clock (the simulated one under `spawned_sim`):
    /// `JobOptions::default().deadline_after(Duration::from_secs(2))`.
    ///
    /// The deadline is hard: when it passes, `done` receives
    /// `JobError::DeadlineExceeded`, the job's later emits fail with
    /// [`JobGone`], and its eventual result is dropped. For a soft deadline
    /// (stop starting new work, finish what is in hand), check the time
    /// inside the work instead.
    pub fn deadline_after(mut self, timeout: std::time::Duration) -> Self {
        self.deadline = Some(spawned_rt::time::Instant::now() + timeout);
        self
    }
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
#[non_exhaustive]
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
#[non_exhaustive]
pub enum JobError {
    /// The work panicked; caught at the job boundary.
    #[error("job panicked: {0}")]
    Panicked(String),
    /// `JobOptions::deadline` passed first. The deadline is hard: the job's
    /// later emits fail and its eventual result is dropped. The work may
    /// still be running (cooperative cancellation).
    #[error("job deadline exceeded")]
    DeadlineExceeded,
}

/// Why a job was not spawned.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
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
use super::job_core::{Finish, HeldTerminal, Job, JobCore, JobSetCore};
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
    /// Delayed emits (`JobCtx::emit_at`) not yet fired: their strong
    /// references (the deadline heap holds `Weak`s). Cleared at cancellation.
    scheduled: Mutex<Vec<Arc<ScheduledEmit>>>,
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
            scheduled: Mutex::new(Vec::new()),
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
                self.count_cancel();
                self.record(|job| trace::JobEvent::new("Cancel", job));
            }
        });
        self.wake_cancelled();
    }

    /// A permit used after the owner began exiting: the job never runs.
    pub(crate) fn never_started(&self) {
        self.core.cancel();
        self.core.finished();
    }

    fn cancel_quietly(&self) {
        if self.core.cancel() {
            self.count_cancel();
        }
        self.wake_cancelled();
    }

    /// `spawned_jobs_cancelled_total` counts jobs cancelled while running.
    fn count_cancel(&self) {
        if self.core.is_running() {
            crate::instrumentation::hooks::job_cancelled();
        }
    }

    fn wake_cancelled(&self) {
        // Scheduled emits will never be delivered: drop them now (their
        // messages and credits), outside every lock.
        let scheduled = std::mem::take(&mut *lock(&self.scheduled));
        for emit in scheduled {
            emit.discard(self);
        }
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
    /// lock (see [`JobCore::notify`]). `end` is `ok`, `panicked` (the end
    /// of execution, the model's `Finish`) or `deadline` (`JobDeadline`).
    pub(crate) fn notify<X>(
        &self,
        end: &'static str,
        enqueue: impl FnOnce() -> Option<X>,
    ) -> (bool, Option<X>) {
        let deadline = end == "deadline";
        let (taken, unsent) = trace::atomic(|| {
            let (taken, unsent) = self.core.notify(enqueue);
            if taken {
                crate::instrumentation::hooks::job_notified(end);
            }
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

    /// The work's result (see [`JobCore::finish`]): taken now, or held
    /// until the job's scheduled emits have fired (`later` then runs
    /// `notify`). Returns the refused envelope, to drop outside locks.
    pub(crate) fn finish<X>(
        &self,
        end: &'static str,
        enqueue: impl FnOnce() -> Option<X>,
        later: impl FnOnce() -> HeldTerminal,
    ) -> Option<X> {
        let mut enqueue = Some(enqueue);
        let (finish, unsent) = trace::atomic(|| {
            let (finish, unsent) = self
                .core
                .finish(|| enqueue.take().and_then(|enqueue| enqueue()), later);
            match finish {
                Finish::Taken => {
                    crate::instrumentation::hooks::job_notified(end);
                    self.record(|job| trace::JobEvent {
                        x: "notified",
                        ..trace::JobEvent::new("Finish", job)
                    });
                }
                Finish::Refused => self.record(|job| trace::JobEvent {
                    x: "quiet",
                    ..trace::JobEvent::new("Finish", job)
                }),
                Finish::Held => {}
            }
            (finish, unsent)
        });
        #[cfg(feature = "async")]
        if finish == Finish::Taken {
            self.wake_emitters();
        }
        #[cfg(not(feature = "async"))]
        let _ = finish;
        unsent
    }

    /// Register a delayed emit at `at` (ticks): `send` enqueues its envelope
    /// when the deadline fires, fenced like any emit, and the job's terminal
    /// message waits for it. `track` arms the owner's deadline heap. On
    /// `Err` the caller's credit guard (inside `send`) returns the credit.
    pub(crate) fn schedule_emit(
        self: &Arc<Self>,
        at: u64,
        send: SendScheduled,
        track: impl FnOnce(u64, std::sync::Weak<dyn Expirable>),
    ) -> Result<(), JobGone> {
        self.core.schedule()?;
        let emit = Arc::new(ScheduledEmit {
            at,
            job: Arc::downgrade(self),
            send: Mutex::new(Some(send)),
        });
        lock(&self.scheduled).push(emit.clone());
        if self.is_cancelled() {
            // Cancelled between the check and the registration: the cancel
            // may have missed it.
            for emit in std::mem::take(&mut *lock(&self.scheduled)) {
                emit.discard(self);
            }
            return Ok(());
        }
        if at <= super::now_ticks() {
            emit.expire(at);
        } else {
            let weak: std::sync::Weak<dyn Expirable> = Arc::downgrade(&emit) as _;
            track(at, weak);
        }
        Ok(())
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
            if fenced {
                crate::instrumentation::hooks::job_fenced();
            }
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
                crate::instrumentation::hooks::job_emitted();
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
    fn is_live(&self) -> bool {
        // Scheduled emits keep a finished job reachable for `cancel_all`.
        self.core.is_running() || self.core.scheduled() > 0
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
        let reserved = self.core.reserve_native();
        if !reserved {
            crate::instrumentation::hooks::job_rejected();
        }
        reserved
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
            crate::instrumentation::hooks::job_started();
            state.record(|job| trace::JobEvent {
                keyed,
                deadline,
                ..trace::JobEvent::new("Start", job)
            });
        });
    }

    /// A job's execution ended.
    pub(crate) fn ended(&self, state: &Arc<JobState>) {
        crate::instrumentation::hooks::job_ended();
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
        crate::instrumentation::hooks::jobs_unfinished(unfinished);
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
        let credit = pool.reserve(1).then(|| Self(Some(pool)));
        if credit.is_none() {
            crate::instrumentation::hooks::job_rejected();
        }
        credit
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

/// Enqueues a delayed emit's envelope; a refused envelope comes back erased,
/// to be dropped outside locks.
pub(crate) type SendScheduled = Box<dyn FnOnce() -> Result<(), Box<dyn Send>> + Send>;

/// One delayed emit (`JobCtx::emit_at`), serviced by the owner's deadline
/// heap. Holds its message and emit credit until it fires or is discarded.
pub(crate) struct ScheduledEmit {
    at: u64,
    job: std::sync::Weak<JobState>,
    send: Mutex<Option<SendScheduled>>,
}

impl ScheduledEmit {
    /// Fire (`deliver`) or discard: step 2 of the emit protocol under the
    /// job's lock, then remove it from the job, then run a held terminal.
    fn settle(&self, job: &JobState, deliver: bool) {
        let Some(send) = lock(&self.send).take() else {
            return;
        };
        // A discarded message (and its credit guard) drops at the end,
        // outside the trace section and every lock.
        let (deliver, _discarded) = if deliver {
            (Some(send), None)
        } else {
            (None, Some(send))
        };
        let (result, held) = trace::atomic(|| {
            let (result, held) = job.core.fire(deliver);
            if result.is_ok() {
                crate::instrumentation::hooks::job_emitted();
                job.record(|id| trace::JobEvent::new("Emit", id));
            }
            (result, held)
        });
        // A refused envelope (and its credit guard) drops here, unlocked.
        drop(result);
        lock(&job.scheduled).retain(|emit| !std::ptr::eq(Arc::as_ptr(emit), self));
        if let Some(held) = held {
            held();
        }
        #[cfg(feature = "async")]
        job.wake_emitters();
    }

    fn discard(&self, job: &JobState) {
        self.settle(job, false);
    }
}

impl Expirable for ScheduledEmit {
    fn pending_deadline(&self) -> Option<u64> {
        lock(&self.send).is_some().then_some(self.at)
    }
    fn expire(&self, _now: u64) {
        if let Some(job) = self.job.upgrade() {
            self.settle(&job, true);
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
