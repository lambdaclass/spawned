//! `tasks` mode pipe-to-self jobs (docs/design/deferred-replies.md §9):
//! `ctx.pipe*` and `ctx.pipe_reply*`, their envelopes and `JobCtx`.
//!
//! A child module of `tasks::actor`, so it reaches the private envelope and
//! shared-state types.

use super::{Actor, Context, Envelope, Handler};
use crate::message::Message;
use crate::reply::deadline::Expirable;
use crate::reply::jobs::{
    JobCredit, JobDeadline, JobError, JobGone, JobHandle, JobKey, JobOptions, JobOutcome, JobState,
    PipeError, PipeErrorKind, PipeReplyError, SendScheduled,
};
use crate::reply::slot::{Refused, Slot};
use crate::reply::{now_ticks, PendingReply, ReplyTo};
use futures::future::{self, FutureExt as _};
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::{pin, Pin};
use std::sync::{Arc, Mutex, Weak};

// ---------------------------------------------------------------------------
// Envelopes
// ---------------------------------------------------------------------------

/// Returns one emit credit when the emitted message is dispatched, fenced or
/// dropped with the mailbox.
struct EmitCredit(Arc<JobState>);
impl Drop for EmitCredit {
    fn drop(&mut self) {
        self.0.return_emit_credit();
    }
}

/// A message emitted by a job; fenced at dispatch when the job is cancelled
/// or belongs to another instance.
struct EmitEnvelope<M> {
    msg: M,
    credit: EmitCredit,
}

impl<A, M> Envelope<A> for EmitEnvelope<M>
where
    A: Actor + Handler<M>,
    M: Message,
{
    #[cfg(feature = "diagnostics")]
    fn message_type(&self) -> &'static str {
        std::any::type_name::<M>()
    }
    fn handle<'a>(
        self: Box<Self>,
        actor: &'a mut A,
        ctx: &'a Context<A>,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        let EmitEnvelope { msg, credit } = *self;
        let fenced = credit.0.fenced(ctx.id(), "emit");
        // The credit returns at dispatch (or here, when fenced).
        drop(credit);
        if fenced {
            ctx.shared.sender.failures().discard(msg);
            return Box::pin(async {});
        }
        Box::pin(actor.handle(msg, PendingReply::detached(), ctx))
    }
}

type Done<T, M> = Box<dyn FnOnce(JobOutcome<T>) -> M + Send>;

/// A job's terminal message: `done(outcome)` runs in the owner's turn and the
/// produced message is dispatched in the same turn.
struct TerminalEnvelope<T, M> {
    job: Arc<JobState>,
    done: Done<T, M>,
    outcome: JobOutcome<T>,
}

impl<A, T, M> Envelope<A> for TerminalEnvelope<T, M>
where
    A: Actor + Handler<M>,
    M: Message,
    T: Send + 'static,
{
    #[cfg(feature = "diagnostics")]
    fn message_type(&self) -> &'static str {
        std::any::type_name::<M>()
    }
    fn handle<'a>(
        self: Box<Self>,
        actor: &'a mut A,
        ctx: &'a Context<A>,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        let TerminalEnvelope { job, done, outcome } = *self;
        let kind = if matches!(outcome, JobOutcome::Err(JobError::DeadlineExceeded)) {
            "deadline"
        } else {
            "done"
        };
        // Everything runs inside the returned future, so a panicking `done`
        // is a handler panic caught by the run loop (`stopped()`, turn end
        // and job cancellation still run).
        Box::pin(async move {
            if job.fenced(ctx.id(), kind) {
                // Fenced: `done` and its captures (a linked token) are
                // discarded in the turn, through the mailbox drop path.
                ctx.shared.sender.failures().discard((done, outcome));
                return;
            }
            let msg = done(outcome);
            actor.handle(msg, PendingReply::detached(), ctx).await
        })
    }
}

// ---------------------------------------------------------------------------
// Job runs
// ---------------------------------------------------------------------------

/// The parts shared by a job's completion path and its deadline.
struct Run<A: Actor, T, M> {
    state: Arc<JobState>,
    ctx: Context<A>,
    done: Mutex<Option<Done<T, M>>>,
    /// The job's deadline (the heap holds a `Weak`): owned until the run is
    /// dropped, so it still fires while a result waits for scheduled emits.
    deadline: std::sync::OnceLock<Arc<JobDeadline>>,
    /// What execution held (its credit, or its place in a linked slot),
    /// handed over when execution ends and released when the terminal
    /// message is enqueued (or the run is dropped): a result waiting for
    /// scheduled emits keeps it, so `MAX_OUTSTANDING` still bounds it.
    hold: Mutex<Option<Box<dyn Send>>>,
}

fn end_of<T>(outcome: &JobOutcome<T>) -> &'static str {
    match outcome {
        JobOutcome::Ok(_) => "ok",
        JobOutcome::Err(JobError::Panicked(_)) => "panicked",
        JobOutcome::Err(JobError::DeadlineExceeded) => "deadline",
    }
}

impl<A, T, M> Run<A, T, M>
where
    A: Actor + Handler<M>,
    M: Message,
    T: Send + 'static,
{
    /// Take the terminal notification and enqueue `(done, outcome)`.
    fn notify(&self, outcome: JobOutcome<T>) {
        let (_, unsent) = self
            .state
            .notify(end_of(&outcome), || self.enqueue_terminal(outcome));
        // A refused terminal message (`done` and the outcome) may drop on
        // the deadline service thread: contained.
        self.ctx.shared.sender.failures().discard(unsent);
    }

    fn enqueue_terminal(&self, outcome: JobOutcome<T>) -> Option<Box<dyn Envelope<A> + Send>> {
        let done = self.done.lock().unwrap_or_else(|p| p.into_inner()).take()?;
        // Execution ended (or is still running past its deadline, and then
        // keeps its hold): return the credit before the terminal message
        // can be dispatched, so `done`'s turn can start the next job at the
        // bound. Only atomics run here (a credit or the slot's job count).
        let hold = self.hold.lock().unwrap_or_else(|p| p.into_inner()).take();
        drop(hold);
        let envelope: Box<dyn Envelope<A> + Send> = Box::new(TerminalEnvelope {
            job: self.state.clone(),
            done,
            outcome,
        });
        self.ctx.shared.sender.send(envelope).err().map(|e| e.0)
    }

    /// The work's result: notify now, or, while delayed emits are still
    /// scheduled, hold it for the last of them so the terminal message
    /// stays last. `hold` is what execution held; the terminal message
    /// releases it.
    fn finish(self: &Arc<Self>, outcome: JobOutcome<T>, hold: Box<dyn Send>) {
        *self.hold.lock().unwrap_or_else(|p| p.into_inner()) = Some(hold);
        let end = end_of(&outcome);
        let outcome = std::cell::Cell::new(Some(outcome));
        let unsent = self.state.finish(
            end,
            || self.enqueue_terminal(outcome.take()?),
            || {
                let run = self.clone();
                // Dropped unrun (a deadline or a cancel overtook it) possibly
                // on the deadline service thread: contained.
                let outcome = outcome
                    .take()
                    .map(|outcome| crate::reply::jobs::Contained::new(outcome, self.ctx.shared.sender.failures()));
                Box::new(move || {
                    if let Some(outcome) = outcome.and_then(|o| o.into_inner()) {
                        run.notify(outcome);
                    }
                })
            },
        );
        self.ctx.shared.sender.failures().discard(unsent);
    }
}

impl<A: Actor, T, M> Drop for Run<A, T, M> {
    /// The last reference may go on the deadline service thread (a deadline
    /// or a held result firing): `done` and its captures drop contained.
    fn drop(&mut self) {
        let done = self.done.get_mut().unwrap_or_else(|p| p.into_inner()).take();
        self.ctx.shared.sender.failures().discard(done);
    }
}

/// Holds a `pipe_reply` job's place in its slot's `JOBS` count.
struct LinkedJob<R: Send + 'static>(Arc<Slot<R>>);
impl<R: Send + 'static> Drop for LinkedJob<R> {
    fn drop(&mut self) {
        self.0.job_end();
    }
}

/// What a job holds while it executes; released when execution ends.
enum Hold<R: Send + 'static> {
    Credit(#[allow(dead_code)] JobCredit),
    Linked(#[allow(dead_code)] LinkedJob<R>),
}

/// Native slot (`MAX_NATIVE_JOBS`) held by a blocking job.
struct NativeSlot<A: Actor>(Context<A>);
impl<A: Actor> Drop for NativeSlot<A> {
    fn drop(&mut self) {
        self.0.shared.jobs.release_native();
    }
}

/// Marks the job ended in the owner's job set when execution ends.
struct Ended<A: Actor>(Context<A>, Arc<JobState>);
impl<A: Actor> Drop for Ended<A> {
    fn drop(&mut self) {
        self.0.shared.jobs.ended(&self.1);
    }
}

fn caught<T>(result: std::thread::Result<T>) -> JobOutcome<T> {
    match result {
        Ok(value) => JobOutcome::Ok(value),
        Err(panic) => {
            let message = crate::error::panic_message(&*panic);
            crate::mailbox::dispose_panic(panic);
            JobOutcome::Err(JobError::Panicked(message))
        }
    }
}

/// The job's handle on its owner while it runs.
pub struct JobCtx<A: Actor> {
    state: Arc<JobState>,
    ctx: Context<A>,
}

impl<A: Actor> std::fmt::Debug for JobCtx<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobCtx")
            .field("cancelled", &self.state.is_cancelled())
            .finish_non_exhaustive()
    }
}

impl<A: Actor> JobCtx<A> {
    /// Whether the job was cancelled (handle, key replacement, owner exit,
    /// or opt-in caller give-up). Cancellation suppresses later results.
    pub fn is_cancelled(&self) -> bool {
        self.state.is_cancelled()
    }

    /// Resolves when the job is cancelled.
    pub async fn cancelled(&self) {
        self.state.cancellation().cancelled().await;
    }

    /// Send `msg` to the owner as one of this job's messages, fenced like the
    /// terminal one and never after it. Waits while `emit_credits` messages
    /// are in flight; fails with [`JobGone`] once the job is cancelled or its
    /// terminal notification was taken.
    pub async fn emit<M>(&self, msg: M) -> Result<(), JobGone>
    where
        A: Handler<M>,
        M: Message,
    {
        std::future::poll_fn(|cx| self.state.poll_acquire(cx)).await?;
        self.enqueue(msg)
    }

    /// [`emit`](Self::emit) delivered at `at` instead of now (OTP's
    /// `erlang:send_after` to the owner, fenced with the job). Takes an emit
    /// credit now (waiting while all are in flight) and keeps it until the
    /// message is dispatched. At `at` the message is enqueued like an
    /// `emit`: dropped if the job was cancelled or replaced, or its deadline
    /// passed. The job's terminal message waits for its scheduled emits, so
    /// it stays last even when the work finishes first. Fails with
    /// [`JobGone`] at the call once the job is cancelled or its notification
    /// was taken.
    pub async fn emit_at<M>(&self, at: crate::Instant, msg: M) -> Result<(), JobGone>
    where
        A: Handler<M>,
        M: Message,
    {
        std::future::poll_fn(|cx| self.state.poll_acquire(cx)).await?;
        self.schedule(at, msg)
    }

    /// Blocking [`emit_at`](Self::emit_at), for `pipe_blocking*` jobs.
    pub fn emit_at_blocking<M>(&self, at: crate::Instant, msg: M) -> Result<(), JobGone>
    where
        A: Handler<M>,
        M: Message,
    {
        self.state.acquire_blocking()?;
        self.schedule(at, msg)
    }

    fn schedule<M>(&self, at: crate::Instant, msg: M) -> Result<(), JobGone>
    where
        A: Handler<M>,
        M: Message,
    {
        let credit = EmitCredit(self.state.clone());
        let ctx = self.ctx.clone();
        let send: SendScheduled = Box::new(move || {
            let envelope: Box<dyn Envelope<A> + Send> = Box::new(EmitEnvelope { msg, credit });
            ctx.shared
                .sender
                .send(envelope)
                .map_err(|refused| Box::new(refused.0) as Box<dyn Send>)
        });
        JobState::schedule_emit(
            &self.state,
            spawned_rt::time::to_ticks(at),
            send,
            self.ctx.shared.sender.failures(),
            |at, item| self.ctx.shared.replies.track(at, item),
        )
    }

    /// Blocking [`emit`](Self::emit), for `pipe_blocking*` jobs.
    pub fn emit_blocking<M>(&self, msg: M) -> Result<(), JobGone>
    where
        A: Handler<M>,
        M: Message,
    {
        self.state.acquire_blocking()?;
        self.enqueue(msg)
    }

    fn enqueue<M>(&self, msg: M) -> Result<(), JobGone>
    where
        A: Handler<M>,
        M: Message,
    {
        let credit = EmitCredit(self.state.clone());
        self.state
            .enqueue_emit(|| {
                let envelope: Box<dyn Envelope<A> + Send> = Box::new(EmitEnvelope { msg, credit });
                self.ctx.shared.sender.send(envelope)
            })
            .map_err(|(gone, unsent)| {
                drop(unsent);
                gone
            })
    }
}

// ---------------------------------------------------------------------------
// Context API
// ---------------------------------------------------------------------------

/// A job admission reserved ahead of building the work: `ctx.reserve_job()`
/// takes the credit (and checks the owner is not stopping) without consuming
/// anything, so a caller that is refused still holds what it would have
/// moved into `work` and `done`. Spawning from a permit cannot fail.
/// Dropping an unused permit returns its credit.
#[must_use = "an unused permit only holds a credit; spawn the job with it or drop it"]
pub struct JobPermit<A: Actor> {
    ctx: Context<A>,
    credit: JobCredit,
}

/// [`JobPermit`] for a blocking job: it also holds a native slot
/// (`MAX_NATIVE_JOBS`). From `ctx.reserve_blocking_job()`.
#[must_use = "an unused permit only holds a credit and a native slot; spawn the job with it or drop it"]
pub struct BlockingJobPermit<A: Actor> {
    ctx: Context<A>,
    credit: JobCredit,
    native: NativeSlot<A>,
}

impl<A: Actor> std::fmt::Debug for JobPermit<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobPermit").finish_non_exhaustive()
    }
}

impl<A: Actor> std::fmt::Debug for BlockingJobPermit<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlockingJobPermit").finish_non_exhaustive()
    }
}

impl<A: Actor> JobPermit<A> {
    /// [`Context::pipe`] with this permit's credit.
    pub fn pipe<T, M, W, D>(self, work: W, done: D) -> JobHandle
    where
        A: Handler<M>,
        M: Message,
        T: Send + 'static,
        W: Future<Output = T> + Send + 'static,
        D: FnOnce(JobOutcome<T>) -> M + Send + 'static,
    {
        self.pipe_with(JobOptions::default(), work, done)
    }

    /// [`Context::pipe_with`] with this permit's credit. If the owner began
    /// exiting since the reservation, nothing runs: `work` and `done` are
    /// dropped and the handle reports the job cancelled (as owner exit
    /// cancels every job).
    pub fn pipe_with<T, M, W, D>(self, opts: JobOptions, work: W, done: D) -> JobHandle
    where
        A: Handler<M>,
        M: Message,
        T: Send + 'static,
        W: Future<Output = T> + Send + 'static,
        D: FnOnce(JobOutcome<T>) -> M + Send + 'static,
    {
        let JobPermit { ctx, credit } = self;
        let Some(state) = ctx.start_job(&opts) else {
            return ctx.refused_job(&opts, (work, done));
        };
        ctx.launch_async::<T, M, (), W>(
            opts,
            state,
            Hold::Credit(credit),
            work,
            Box::new(done),
            None,
        )
    }

    /// [`Context::pipe_with_ctx`] with this permit's credit.
    pub fn pipe_with_ctx<T, M, F, W, D>(self, opts: JobOptions, work: F, done: D) -> JobHandle
    where
        A: Handler<M>,
        M: Message,
        T: Send + 'static,
        F: FnOnce(JobCtx<A>) -> W,
        W: Future<Output = T> + Send + 'static,
        D: FnOnce(JobOutcome<T>) -> M + Send + 'static,
    {
        let JobPermit { ctx, credit } = self;
        let Some(state) = ctx.start_job(&opts) else {
            return ctx.refused_job(&opts, (work, done));
        };
        let job = JobCtx {
            state: state.clone(),
            ctx: ctx.clone(),
        };
        let work = work(job);
        ctx.launch_async::<T, M, (), W>(
            opts,
            state,
            Hold::Credit(credit),
            work,
            Box::new(done),
            None,
        )
    }
}

impl<A: Actor> BlockingJobPermit<A> {
    /// [`Context::pipe_blocking_with`] with this permit's credit and native
    /// slot; cannot fail. If the owner began exiting since the reservation,
    /// nothing runs (see [`JobPermit::pipe_with`]).
    pub fn pipe_blocking_with<T, M, W, D>(self, opts: JobOptions, work: W, done: D) -> JobHandle
    where
        A: Handler<M>,
        M: Message,
        T: Send + 'static,
        W: FnOnce(&JobCtx<A>) -> T + Send + 'static,
        D: FnOnce(JobOutcome<T>) -> M + Send + 'static,
    {
        let BlockingJobPermit {
            ctx,
            credit,
            native,
        } = self;
        let Some(state) = ctx.start_job(&opts) else {
            return ctx.refused_job(&opts, (work, done, native));
        };
        ctx.launch_blocking::<T, M, (), W>(
            opts,
            state,
            Hold::Credit(credit),
            native,
            work,
            Box::new(done),
        )
    }
}

impl<A: Actor> Context<A> {
    /// The job's state, or `None` once the owner is exiting.
    fn start_job(&self, opts: &JobOptions) -> Option<Arc<JobState>> {
        (!self.shared.replies.is_exiting()).then(|| JobState::new(self.id(), opts.emit_credits))
    }

    /// A permit used after the owner began exiting: nothing runs, the parts
    /// drop here (outside locks), and the handle reports cancelled.
    fn refused_job<P>(&self, opts: &JobOptions, parts: P) -> JobHandle {
        drop(parts);
        let state = JobState::new(self.id(), opts.emit_credits);
        state.never_started();
        JobHandle::new(state)
    }

    fn reserve_credit(&self) -> Result<JobCredit, PipeError> {
        if self.shared.replies.is_exiting() {
            return Err(PipeError {
                kind: PipeErrorKind::OwnerStopping,
            });
        }
        JobCredit::reserve(&self.shared.replies).ok_or(PipeError {
            kind: PipeErrorKind::Overloaded,
        })
    }

    fn reserve_native(&self) -> Result<NativeSlot<A>, PipeErrorKind> {
        if !self.shared.jobs.reserve_native() {
            return Err(PipeErrorKind::NativeBudget);
        }
        Ok(NativeSlot(self.clone()))
    }

    /// Reserve an unlinked job's admission (the owner is running, one
    /// `MAX_OUTSTANDING` credit) before building `work` and `done`: a
    /// refusal leaves the caller everything it would have moved into them,
    /// and spawning from the permit cannot fail.
    ///
    /// ```ignore
    /// let Ok(permit) = ctx.reserve_job() else {
    ///     return self.report(id, Verdict::Overloaded); // `id` was not moved
    /// };
    /// permit.pipe(fetch(object), move |res| Fetched { id, res });
    /// ```
    pub fn reserve_job(&self) -> Result<JobPermit<A>, PipeError> {
        Ok(JobPermit {
            ctx: self.clone(),
            credit: self.reserve_credit()?,
        })
    }

    /// [`reserve_job`](Self::reserve_job) for a blocking job: also takes a
    /// native slot (`MAX_NATIVE_JOBS`, `NativeBudget` at the bound).
    pub fn reserve_blocking_job(&self) -> Result<BlockingJobPermit<A>, PipeError> {
        let credit = self.reserve_credit()?;
        let native = self.reserve_native().map_err(|kind| PipeError { kind })?;
        Ok(BlockingJobPermit {
            ctx: self.clone(),
            credit,
            native,
        })
    }

    fn arm_deadline<T, M>(&self, opts: &JobOptions, run: &Arc<Run<A, T, M>>)
    where
        A: Handler<M>,
        M: Message,
        T: Send + 'static,
    {
        let Some(at) = opts.deadline.map(spawned_rt::time::to_ticks) else {
            return;
        };
        let weak = Arc::downgrade(run);
        let deadline = Arc::new(JobDeadline {
            at,
            state: run.state.clone(),
            notify: Mutex::new(Some(Box::new(move || {
                if let Some(run) = weak.upgrade() {
                    run.notify(JobOutcome::Err(JobError::DeadlineExceeded));
                }
            }))),
        });
        // The run owns it; the heap only refers to it.
        let _ = run.deadline.set(deadline.clone());
        if at <= now_ticks() {
            deadline.expire(at);
        } else {
            let item: Weak<dyn Expirable> = Arc::downgrade(&deadline) as Weak<dyn Expirable>;
            self.shared.replies.track(at, item);
        }
    }

    fn launch_async<T, M, R, W>(
        &self,
        opts: JobOptions,
        state: Arc<JobState>,
        hold: Hold<R>,
        work: W,
        done: Done<T, M>,
        caller_gone: Option<Arc<Slot<R>>>,
    ) -> JobHandle
    where
        A: Handler<M>,
        M: Message,
        T: Send + 'static,
        R: Send + 'static,
        W: Future<Output = T> + Send + 'static,
    {
        self.shared
            .jobs
            .admit(&state, opts.key.clone(), opts.deadline.is_some());
        let run = Arc::new(Run {
            state: state.clone(),
            ctx: self.clone(),
            done: Mutex::new(Some(done)),
            deadline: std::sync::OnceLock::new(),
            hold: Mutex::new(None),
        });
        self.arm_deadline(&opts, &run);
        let ended = Ended(self.clone(), state.clone());
        let handle = JobHandle::new(state.clone());
        self.spawn_framework(async move {
            let _ended = ended;
            let cancelled = pin!(state.cancellation().cancelled());
            let gone = pin!(async {
                match &caller_gone {
                    Some(slot) => {
                        std::future::poll_fn(|cx| slot.poll_closed(cx.waker(), &now_ticks)).await
                    }
                    None => std::future::pending().await,
                }
            });
            let work = pin!(AssertUnwindSafe(work).catch_unwind());
            let stop = future::select(cancelled, gone);
            match future::select(work, stop).await {
                // Execution ended: its credit (and linked-job count) goes to
                // the run, which returns it when the terminal message is
                // enqueued.
                future::Either::Left((result, _)) => run.finish(caught(result), Box::new(hold)),
                // Cancelled (or the opt-in caller give-up): the work future is
                // dropped here, and with it any downstream receiver.
                future::Either::Right(_) => {
                    state.cancel();
                    state.ended_cancelled();
                }
            }
        });
        handle
    }

    /// Run a blocking job's body on Tokio's blocking pool (the managed
    /// actor's group runtime, or the current one). Outside a Tokio runtime,
    /// and under the simulator, on an owned thread.
    fn spawn_native(&self, task: impl FnOnce() + Send + 'static) {
        #[cfg(not(spawned_sim))]
        {
            let handle = match &self.shared.managed {
                Some(scope) => Some(scope.handle.clone()),
                None => spawned_rt::tasks::Handle::try_current().ok(),
            };
            if let Some(handle) = handle {
                drop(handle.spawn_blocking(task));
                return;
            }
        }
        if let Err(error) = std::thread::Builder::new()
            .name("spawned-job".into())
            .spawn(task)
        {
            // The task (its native slot, credit and `done`) dropped with the
            // failed spawn: the job ends without a terminal message.
            tracing::error!(%error, actor = %self.id(), "could not spawn a native job thread");
        }
    }

    fn launch_blocking<T, M, R, W>(
        &self,
        opts: JobOptions,
        state: Arc<JobState>,
        hold: Hold<R>,
        native: NativeSlot<A>,
        work: W,
        done: Done<T, M>,
    ) -> JobHandle
    where
        A: Handler<M>,
        M: Message,
        T: Send + 'static,
        R: Send + 'static,
        W: FnOnce(&JobCtx<A>) -> T + Send + 'static,
    {
        self.shared
            .jobs
            .admit(&state, opts.key.clone(), opts.deadline.is_some());
        let run = Arc::new(Run {
            state: state.clone(),
            ctx: self.clone(),
            done: Mutex::new(Some(done)),
            deadline: std::sync::OnceLock::new(),
            hold: Mutex::new(None),
        });
        self.arm_deadline(&opts, &run);
        let ended = Ended(self.clone(), state.clone());
        let job = JobCtx {
            state: state.clone(),
            ctx: self.clone(),
        };
        self.spawn_native(move || {
            let _ended = ended;
            let result = std::panic::catch_unwind(AssertUnwindSafe(|| work(&job)));
            // Execution ended: release the native slot now; the run returns
            // the credit when the terminal message is enqueued.
            drop(native);
            run.finish(caught(result), Box::new(hold));
        });
        JobHandle::new(state)
    }

    /// Run `work` as a job owned by this actor instance and deliver
    /// `done(outcome)` as one message in a later turn (`handle_info`). A
    /// panic arrives as `JobOutcome::Err(JobError::Panicked)`. Holds one
    /// `MAX_OUTSTANDING` credit while it executes. Rejects at once at a
    /// bound; never waits. On `Err`, `work` and `done` are dropped: use
    /// [`reserve_job`](Self::reserve_job) to keep them.
    pub fn pipe<T, M, W, D>(&self, work: W, done: D) -> Result<JobHandle, PipeError>
    where
        A: Handler<M>,
        M: Message,
        T: Send + 'static,
        W: Future<Output = T> + Send + 'static,
        D: FnOnce(JobOutcome<T>) -> M + Send + 'static,
    {
        self.pipe_with(JobOptions::default(), work, done)
    }

    /// [`pipe`](Self::pipe) that first cancels the previous job with `key`
    /// (whatever its notification state) and fences its queued messages.
    pub fn pipe_keyed<T, M, W, D>(
        &self,
        key: impl Into<JobKey>,
        work: W,
        done: D,
    ) -> Result<JobHandle, PipeError>
    where
        A: Handler<M>,
        M: Message,
        T: Send + 'static,
        W: Future<Output = T> + Send + 'static,
        D: FnOnce(JobOutcome<T>) -> M + Send + 'static,
    {
        let opts = JobOptions {
            key: Some(key.into()),
            ..JobOptions::default()
        };
        self.pipe_with(opts, work, done)
    }

    /// [`pipe`](Self::pipe) with options (deadline, key, emit credits).
    pub fn pipe_with<T, M, W, D>(
        &self,
        opts: JobOptions,
        work: W,
        done: D,
    ) -> Result<JobHandle, PipeError>
    where
        A: Handler<M>,
        M: Message,
        T: Send + 'static,
        W: Future<Output = T> + Send + 'static,
        D: FnOnce(JobOutcome<T>) -> M + Send + 'static,
    {
        Ok(self.reserve_job()?.pipe_with(opts, work, done))
    }

    /// [`pipe_with`](Self::pipe_with) for async work that needs its
    /// [`JobCtx`]: `work` receives it to check cancellation and `emit` fenced
    /// messages (bounded by `JobOptions::emit_credits`; the terminal message
    /// is always last).
    pub fn pipe_with_ctx<T, M, F, W, D>(
        &self,
        opts: JobOptions,
        work: F,
        done: D,
    ) -> Result<JobHandle, PipeError>
    where
        A: Handler<M>,
        M: Message,
        T: Send + 'static,
        F: FnOnce(JobCtx<A>) -> W,
        W: Future<Output = T> + Send + 'static,
        D: FnOnce(JobOutcome<T>) -> M + Send + 'static,
    {
        Ok(self.reserve_job()?.pipe_with_ctx(opts, work, done))
    }

    /// Run blocking `work` on Tokio's blocking pool, under the actor's
    /// `MAX_NATIVE_JOBS` budget (cancellation stays cooperative: the job
    /// checks its [`JobCtx`]). `work` gets a [`JobCtx`] to check
    /// cancellation and `emit_blocking` fenced messages; a panic arrives as
    /// `JobError::Panicked`. On `Err`, `work` and `done` are dropped: use
    /// [`reserve_blocking_job`](Self::reserve_blocking_job) to keep them.
    pub fn pipe_blocking_with<T, M, W, D>(
        &self,
        opts: JobOptions,
        work: W,
        done: D,
    ) -> Result<JobHandle, PipeError>
    where
        A: Handler<M>,
        M: Message,
        T: Send + 'static,
        W: FnOnce(&JobCtx<A>) -> T + Send + 'static,
        D: FnOnce(JobOutcome<T>) -> M + Send + 'static,
    {
        Ok(self
            .reserve_blocking_job()?
            .pipe_blocking_with(opts, work, done))
    }

    /// Delegate a deferred reply: run `work` as a job linked to `token`'s
    /// request and deliver `done(outcome, token)` in a later turn. The
    /// obligation and credit stay in this actor: the token and its first
    /// linked job share one credit. On every error the token comes back.
    pub fn pipe_reply<R, T, M, W, D>(
        &self,
        token: ReplyTo<R>,
        work: W,
        done: D,
    ) -> Result<JobHandle, PipeReplyError<R>>
    where
        A: Handler<M>,
        M: Message,
        R: Send + 'static,
        T: Send + 'static,
        W: Future<Output = T> + Send + 'static,
        D: FnOnce(JobOutcome<T>, ReplyTo<R>) -> M + Send + 'static,
    {
        self.pipe_reply_with(JobOptions::default(), token, work, done)
    }

    /// [`pipe_reply`](Self::pipe_reply) with options. With
    /// `cancel_on_caller_gone` the job is cancelled when the caller leaves
    /// or the request's deadline passes (off by default, as in OTP).
    pub fn pipe_reply_with<R, T, M, W, D>(
        &self,
        opts: JobOptions,
        token: ReplyTo<R>,
        work: W,
        done: D,
    ) -> Result<JobHandle, PipeReplyError<R>>
    where
        A: Handler<M>,
        M: Message,
        R: Send + 'static,
        T: Send + 'static,
        W: Future<Output = T> + Send + 'static,
        D: FnOnce(JobOutcome<T>, ReplyTo<R>) -> M + Send + 'static,
    {
        let (state, slot, token) = self.link_reply(&opts, token)?;
        let watch = opts.cancel_on_caller_gone.then(|| slot.clone());
        let hold = Hold::Linked(LinkedJob(slot));
        Ok(self.launch_async(opts, state, hold, work, linked_done(token, done), watch))
    }

    /// Blocking [`pipe_reply`](Self::pipe_reply) on Tokio's blocking pool,
    /// under the `MAX_NATIVE_JOBS` budget.
    pub fn pipe_reply_blocking_with<R, T, M, W, D>(
        &self,
        opts: JobOptions,
        token: ReplyTo<R>,
        work: W,
        done: D,
    ) -> Result<JobHandle, PipeReplyError<R>>
    where
        A: Handler<M>,
        M: Message,
        R: Send + 'static,
        T: Send + 'static,
        W: FnOnce(&JobCtx<A>) -> T + Send + 'static,
        D: FnOnce(JobOutcome<T>, ReplyTo<R>) -> M + Send + 'static,
    {
        let (state, slot, token) = self.link_reply(&opts, token)?;
        let hold = Hold::Linked(LinkedJob(slot.clone()));
        let native = match self.reserve_native() {
            Ok(native) => native,
            // Dropping the hold detaches the linked job from the slot.
            Err(kind) => {
                drop(hold);
                return Err(PipeReplyError::Failed { token, kind });
            }
        };
        if opts.cancel_on_caller_gone {
            crate::reply::jobs::cancel_when_closed(&slot, &state);
        }
        Ok(self.launch_blocking(opts, state, hold, native, work, linked_done(token, done)))
    }

    /// Checks shared by the `pipe_reply*` calls: owner, owner exit, and the
    /// attach CAS on the token's slot (which reserves a credit only when a
    /// linked job already runs).
    #[allow(clippy::type_complexity)]
    fn link_reply<R: Send + 'static>(
        &self,
        opts: &JobOptions,
        token: ReplyTo<R>,
    ) -> Result<(Arc<JobState>, Arc<Slot<R>>, ReplyTo<R>), PipeReplyError<R>> {
        if token.owner() != self.id() {
            return Err(PipeReplyError::WrongOwner(token));
        }
        let Some(state) = self.start_job(opts) else {
            return Err(PipeReplyError::Failed {
                token,
                kind: PipeErrorKind::OwnerStopping,
            });
        };
        let slot = token.slot_arc().clone();
        match slot.attach(&now_ticks) {
            Ok(()) => Ok((state, slot, token)),
            Err(Refused::Closed) => Err(PipeReplyError::Closed(token)),
            Err(Refused::Overloaded) => Err(PipeReplyError::Failed {
                token,
                kind: PipeErrorKind::Overloaded,
            }),
        }
    }
}

fn linked_done<R, T, M, D>(token: ReplyTo<R>, done: D) -> Done<T, M>
where
    R: Send + 'static,
    T: Send + 'static,
    M: Message,
    D: FnOnce(JobOutcome<T>, ReplyTo<R>) -> M + Send + 'static,
{
    Box::new(move |outcome| done(outcome, token))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::ActorStart;

    struct Owner;
    impl Actor for Owner {}
    struct Note;
    impl Message for Note {
        type Result = ();
    }
    impl Handler<Note> for Owner {
        async fn handle(&mut self, _: Note, _: PendingReply<'_, ()>, _: &Context<Self>) {}
    }

    /// Review P1 (Codex 13): an emit refused after its credit was taken
    /// (the job was cancelled in between) returns that credit once, through
    /// its guard. It used to be returned by the refusal and again by the
    /// guard, so the job ended with more credits than it was given (and
    /// `u16::MAX` credits overflowed).
    #[tokio::test]
    async fn a_refused_emit_returns_its_credit_once() {
        let owner = Owner.start();
        for credits in [1, u16::MAX] {
            let state = JobState::new(owner.id(), credits);
            let job = JobCtx {
                state: state.clone(),
                ctx: owner.context(),
            };
            state.acquire_blocking().unwrap();
            state.cancel();
            assert_eq!(job.enqueue(Note), Err(JobGone));
            assert_eq!(state.emit_credits(), credits, "credit returned twice");
        }
        owner.context().stop();
        owner.join().await;
    }
}
