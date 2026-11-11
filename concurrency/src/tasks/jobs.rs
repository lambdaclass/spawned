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
    PipeError, PipeErrorKind, PipeReplyError,
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
        let fenced = credit.0.is_cancelled() || credit.0.owner() != ctx.id();
        // The credit returns at dispatch (or here, when fenced).
        drop(credit);
        if fenced {
            drop(msg);
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
        if job.is_cancelled() || job.owner() != ctx.id() {
            // Fenced: `done` and its captures (a linked token) drop in the
            // turn, by the drop rules.
            drop((done, outcome));
            return Box::pin(async {});
        }
        let msg = done(outcome);
        Box::pin(actor.handle(msg, PendingReply::detached(), ctx))
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
}

impl<A, T, M> Run<A, T, M>
where
    A: Actor + Handler<M>,
    M: Message,
    T: Send + 'static,
{
    /// Take the terminal notification and enqueue `(done, outcome)`.
    fn notify(&self, outcome: JobOutcome<T>) {
        let (_, unsent) = self.state.notify(|| {
            let done = self.done.lock().unwrap_or_else(|p| p.into_inner()).take()?;
            let envelope: Box<dyn Envelope<A> + Send> = Box::new(TerminalEnvelope {
                job: self.state.clone(),
                done,
                outcome,
            });
            self.ctx.shared.sender.send(envelope).err()
        });
        drop(unsent);
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

impl<A: Actor> Context<A> {
    fn admit(&self, opts: &JobOptions) -> Result<Arc<JobState>, PipeErrorKind> {
        if self.shared.replies.is_exiting() {
            return Err(PipeErrorKind::OwnerStopping);
        }
        Ok(JobState::new(self.id(), opts.emit_credits))
    }

    fn arm_deadline<T, M>(
        &self,
        opts: &JobOptions,
        run: &Arc<Run<A, T, M>>,
    ) -> Option<Arc<JobDeadline>>
    where
        A: Handler<M>,
        M: Message,
        T: Send + 'static,
    {
        let at = spawned_rt::time::to_ticks(opts.deadline?);
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
        if at <= now_ticks() {
            deadline.expire(at);
        } else {
            let item: Weak<dyn Expirable> = Arc::downgrade(&deadline) as Weak<dyn Expirable>;
            self.shared.replies.track(at, item);
        }
        Some(deadline)
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
        self.shared.jobs.admit(&state, opts.key.clone());
        let run = Arc::new(Run {
            state: state.clone(),
            ctx: self.clone(),
            done: Mutex::new(Some(done)),
        });
        let deadline = self.arm_deadline(&opts, &run);
        let ended = Ended(self.clone(), state.clone());
        let handle = JobHandle::new(state.clone());
        self.spawn_framework(async move {
            let _ended = ended;
            let _hold = hold;
            let _deadline = deadline;
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
                future::Either::Left((result, _)) => run.notify(caught(result)),
                // Cancelled (or the opt-in caller give-up): the work future is
                // dropped here, and with it any downstream receiver.
                future::Either::Right(_) => state.cancel(),
            }
        });
        handle
    }

    fn launch_blocking<T, M, R, W>(
        &self,
        opts: JobOptions,
        state: Arc<JobState>,
        hold: Hold<R>,
        work: W,
        done: Done<T, M>,
    ) -> Result<JobHandle, PipeErrorKind>
    where
        A: Handler<M>,
        M: Message,
        T: Send + 'static,
        R: Send + 'static,
        W: FnOnce(&JobCtx<A>) -> T + Send + 'static,
    {
        if !self.shared.jobs.reserve_native() {
            return Err(PipeErrorKind::NativeBudget);
        }
        let native = NativeSlot(self.clone());
        let run = Arc::new(Run {
            state: state.clone(),
            ctx: self.clone(),
            done: Mutex::new(Some(done)),
        });
        let thread_run = run.clone();
        let job = JobCtx {
            state: state.clone(),
            ctx: self.clone(),
        };
        // Admission happens only once the thread exists; the thread waits
        // for it, so a spawn failure leaves no trace in the job set.
        type Start<A> = (Ended<A>, Option<Arc<JobDeadline>>);
        let (start, started) = std::sync::mpsc::channel::<Start<A>>();
        let spawned = std::thread::Builder::new()
            .name("spawned-job".into())
            .spawn(move || {
                let _native = native;
                let _hold = hold;
                let Ok((ended, deadline)) = started.recv() else {
                    return;
                };
                let _ended = ended;
                let _deadline = deadline;
                let result = std::panic::catch_unwind(AssertUnwindSafe(|| work(&job)));
                thread_run.notify(caught(result));
            });
        match spawned {
            Ok(_thread) => {
                self.shared.jobs.admit(&state, opts.key.clone());
                let deadline = self.arm_deadline(&opts, &run);
                let ended = Ended(self.clone(), state.clone());
                let _ = start.send((ended, deadline));
                Ok(JobHandle::new(state))
            }
            // The closure (native slot, hold, `done`) dropped with the failed
            // spawn, releasing what was reserved.
            Err(error) => Err(PipeErrorKind::SpawnFailed(error)),
        }
    }

    /// Run `work` as a job owned by this actor instance and deliver
    /// `done(outcome)` as one message in a later turn (`handle_info`). A
    /// panic arrives as `JobOutcome::Err(JobError::Panicked)`. Holds one
    /// `MAX_OUTSTANDING` credit while it executes. Rejects at once at a
    /// bound; never waits.
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
        let state = self.admit(&opts).map_err(|kind| PipeError { kind })?;
        let credit = JobCredit::reserve(&self.shared.replies).ok_or(PipeError {
            kind: PipeErrorKind::Overloaded,
        })?;
        Ok(self.launch_async::<T, M, (), W>(
            opts,
            state,
            Hold::Credit(credit),
            work,
            Box::new(done),
            None,
        ))
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
        let state = self.admit(&opts).map_err(|kind| PipeError { kind })?;
        let credit = JobCredit::reserve(&self.shared.replies).ok_or(PipeError {
            kind: PipeErrorKind::Overloaded,
        })?;
        let job = JobCtx {
            state: state.clone(),
            ctx: self.clone(),
        };
        let work = work(job);
        Ok(self.launch_async::<T, M, (), W>(
            opts,
            state,
            Hold::Credit(credit),
            work,
            Box::new(done),
            None,
        ))
    }

    /// Run blocking `work` on an owned native thread, under the actor's
    /// `MAX_NATIVE_JOBS` budget. `work` gets a [`JobCtx`] to check
    /// cancellation and `emit_blocking` fenced messages.
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
        let state = self.admit(&opts).map_err(|kind| PipeError { kind })?;
        let credit = JobCredit::reserve(&self.shared.replies).ok_or(PipeError {
            kind: PipeErrorKind::Overloaded,
        })?;
        self.launch_blocking::<T, M, (), W>(opts, state, Hold::Credit(credit), work, Box::new(done))
            .map_err(|kind| PipeError { kind })
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

    /// Blocking [`pipe_reply`](Self::pipe_reply) on an owned native thread.
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
        let hold = Hold::Linked(LinkedJob(slot));
        let cell = Arc::new(Mutex::new(Some(token)));
        let shared = cell.clone();
        let done: Done<T, M> = Box::new(move |outcome| {
            let token = shared
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .take()
                .expect("the token is taken once, by done or by a spawn failure");
            done(outcome, token)
        });
        self.launch_blocking(opts, state, hold, work, done)
            .map_err(|kind| {
                let token = cell
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .take()
                    .expect("done never ran");
                PipeReplyError::Failed { token, kind }
            })
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
        let state = match self.admit(opts) {
            Ok(state) => state,
            Err(kind) => return Err(PipeReplyError::Failed { token, kind }),
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
