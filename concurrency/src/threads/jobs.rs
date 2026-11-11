//! `threads` mode pipe-to-self jobs (docs/design/deferred-replies.md §9):
//! one owned native thread per admitted job under `MAX_NATIVE_JOBS`
//! (`pipe_blocking`, `pipe_reply_blocking`), their envelopes and
//! `JobCtx`. The shared pool comes with rank 11b.
//!
//! A child module of `threads::actor`, so it reaches the private envelope and
//! context types.

use super::{Actor, Context, Envelope, Handler, MailboxItem};
use crate::message::Message;
use crate::reply::deadline::Expirable;
use crate::reply::jobs::{
    JobCredit, JobDeadline, JobError, JobGone, JobHandle, JobOptions, JobOutcome, JobState,
    PipeError, PipeReplyError, SendScheduled,
};
use crate::reply::slot::{Refused, Slot};
use crate::reply::{now_ticks, PendingReply, ReplyTo};
use std::panic::AssertUnwindSafe;
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
    fn handle(self: Box<Self>, actor: &mut A, ctx: &Context<A>) {
        let EmitEnvelope { msg, credit } = *self;
        let fenced = credit.0.fenced(ctx.id(), "emit");
        // The credit returns at dispatch (or here, when fenced).
        drop(credit);
        if fenced {
            ctx.sender.failures().discard(msg);
            return;
        }
        actor.handle(msg, PendingReply::detached(), ctx);
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
    fn handle(self: Box<Self>, actor: &mut A, ctx: &Context<A>) {
        let TerminalEnvelope { job, done, outcome } = *self;
        let kind = if matches!(outcome, Err(JobError::DeadlineExceeded)) {
            "deadline"
        } else {
            "done"
        };
        if job.fenced(ctx.id(), kind) {
            // Fenced: `done` and its captures (a linked token) drop in the
            // turn, by the drop rules.
            ctx.sender.failures().discard((done, outcome));
            return;
        }
        let msg = done(outcome);
        actor.handle(msg, PendingReply::detached(), ctx);
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
        Ok(_) => "ok",
        Err(JobError::Panicked(_)) => "panicked",
        Err(JobError::DeadlineExceeded) => "deadline",
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
        self.ctx.sender.failures().discard(unsent);
    }

    fn enqueue_terminal(&self, outcome: JobOutcome<T>) -> Option<MailboxItem<A>> {
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
        self.ctx
            .sender
            .send(MailboxItem::Message(envelope))
            .err()
            .map(|e| e.0)
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
                let outcome = outcome.take().map(|outcome| {
                    crate::reply::jobs::Contained::new(outcome, self.ctx.sender.failures())
                });
                Box::new(move || {
                    if let Some(outcome) = outcome.and_then(|o| o.into_inner()) {
                        run.notify(outcome);
                    }
                })
            },
        );
        self.ctx.sender.failures().discard(unsent);
    }
}

impl<A: Actor, T, M> Drop for Run<A, T, M> {
    /// The last reference may go on the deadline service thread (a deadline
    /// or a held result firing): `done` and its captures drop contained.
    fn drop(&mut self) {
        let done = self
            .done
            .get_mut()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        self.ctx.sender.failures().discard(done);
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
        self.0.jobs.release_native();
    }
}

/// Marks the job ended in the owner's job set when execution ends.
struct Ended<A: Actor>(Context<A>, Arc<JobState>);
impl<A: Actor> Drop for Ended<A> {
    fn drop(&mut self) {
        self.0.jobs.ended(&self.1);
    }
}

fn caught<T>(result: std::thread::Result<T>) -> JobOutcome<T> {
    result.map_err(|panic| {
        let message = crate::error::panic_message(&*panic);
        crate::mailbox::dispose_panic(panic);
        JobError::Panicked(message)
    })
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

    /// Send `msg` to the owner as one of this job's messages, fenced like the
    /// terminal one and never after it. Blocks while `emit_credits` messages
    /// are in flight; fails with [`JobGone`] once the job is cancelled or its
    /// terminal notification was taken.
    pub fn emit_blocking<M>(&self, msg: M) -> Result<(), JobGone>
    where
        A: Handler<M>,
        M: Message,
    {
        self.state.acquire_blocking()?;
        self.enqueue(msg)
    }

    /// [`emit_blocking`](Self::emit_blocking) delivered at `at` instead of
    /// now (OTP's `erlang:send_after` to the owner, fenced with the job).
    /// Takes an emit credit now (blocking while all are in flight) and keeps
    /// it until the message is dispatched. At `at` the message is enqueued
    /// like an emit: dropped if the job was cancelled or replaced, or its
    /// deadline passed. The job's terminal message waits for its scheduled
    /// emits, so it stays last even when the work finishes first.
    ///
    /// `at` is an instant of the monotonic deadline clock. One already
    /// reached (or in the past) is an immediate emit: the message is
    /// enqueued at the call, ahead of later emits, so there is no need to
    /// branch to [`emit_blocking`](Self::emit_blocking).
    pub fn emit_at_blocking<M>(&self, at: crate::Instant, msg: M) -> Result<(), JobGone>
    where
        A: Handler<M>,
        M: Message,
    {
        self.state.acquire_blocking()?;
        let credit = EmitCredit(self.state.clone());
        let ctx = self.ctx.clone();
        let send: SendScheduled = Box::new(move || {
            let envelope: Box<dyn Envelope<A> + Send> = Box::new(EmitEnvelope { msg, credit });
            ctx.sender
                .send(MailboxItem::Message(envelope))
                .map_err(|refused| Box::new(refused.0) as Box<dyn Send>)
        });
        JobState::schedule_emit(
            &self.state,
            spawned_rt::time::to_ticks(at),
            send,
            self.ctx.sender.failures(),
            |at, item| self.ctx.replies.track(at, item),
        )
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
                self.ctx.sender.send(MailboxItem::Message(envelope))
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

/// The task a native worker thread runs once its job is spawned.
type NativeTask = Box<dyn FnOnce() + Send>;

/// A native thread spawned at reservation and parked until its job is
/// spawned, so spawning from a permit cannot fail, with its native slot.
/// The slot stays here until the job is launched (it then moves into the
/// task): dropped unused, the worker returns the slot at once, on the
/// dropping thread, and the parked thread exits.
struct NativeWorker<A: Actor> {
    start: std::sync::mpsc::Sender<NativeTask>,
    native: NativeSlot<A>,
}

/// A blocking job's admission reserved ahead of building the work:
/// `ctx.reserve_blocking_job(opts)` checks the owner is running and takes a
/// `MAX_OUTSTANDING` credit, a native slot and the job's (parked) thread, so
/// a caller that is refused still holds what it would have moved into
/// `work` and `done`, and spawning from the permit cannot fail. Dropping an
/// unused permit returns everything.
#[must_use = "an unused permit only holds a credit and a native thread; spawn the job with it or drop it"]
pub struct BlockingJobPermit<A: Actor> {
    ctx: Context<A>,
    opts: JobOptions,
    credit: JobCredit,
    worker: NativeWorker<A>,
}

impl<A: Actor> std::fmt::Debug for BlockingJobPermit<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlockingJobPermit").finish_non_exhaustive()
    }
}

impl<A: Actor> BlockingJobPermit<A> {
    /// [`Context::pipe_blocking`] with this permit's options, credit and
    /// thread; cannot fail. If the owner began exiting since the
    /// reservation, nothing runs: `work` and `done` are dropped and the
    /// handle reports the job cancelled (as owner exit cancels every job).
    pub fn pipe_blocking<T, M, W, D>(self, work: W, done: D) -> JobHandle
    where
        A: Handler<M>,
        M: Message,
        T: Send + 'static,
        W: FnOnce(JobCtx<A>) -> T + Send + 'static,
        D: FnOnce(JobOutcome<T>) -> M + Send + 'static,
    {
        let BlockingJobPermit {
            ctx,
            opts,
            credit,
            worker,
        } = self;
        let state = JobState::new(ctx.id(), opts.emit_credits);
        // Refused once the owner began exiting, in one step with the exit's
        // close of admission (see `admit_job`).
        if ctx.replies.is_exiting() || !ctx.admit_job(&state, &opts) {
            drop((work, done, worker, credit));
            state.never_started();
            return JobHandle::new(state);
        }
        ctx.launch_blocking::<T, M, (), W>(
            opts,
            state,
            Hold::Credit(credit),
            worker,
            work,
            Box::new(done),
        )
    }
}

impl<A: Actor> Context<A> {
    /// Admit `state` into the owner's job set (the model's `Start`), after
    /// every reversible reservation and before anything that cannot be
    /// undone: refused once the owner's exit closed admission, in one step
    /// with that close, so an admitted job is always cancelled (and joined)
    /// by the owner's exit.
    fn admit_job(&self, state: &Arc<JobState>, opts: &JobOptions) -> bool {
        self.jobs
            .admit(state, opts.key.clone(), opts.deadline.is_some())
    }

    fn reserve_credit(&self) -> Result<JobCredit, PipeError> {
        if self.replies.is_exiting() {
            return Err(PipeError::OwnerStopping);
        }
        JobCredit::reserve(&self.replies).ok_or(PipeError::Overloaded)
    }

    /// Reserve a native slot and spawn its thread, parked until the job is
    /// spawned. A spawn failure releases the slot.
    fn spawn_worker(&self) -> Result<NativeWorker<A>, PipeError> {
        if !self.jobs.reserve_native() {
            return Err(PipeError::NativeBudget);
        }
        let native = NativeSlot(self.clone());
        let (start, started) = std::sync::mpsc::channel::<NativeTask>();
        std::thread::Builder::new()
            .name("spawned-job".into())
            .spawn(move || {
                if let Ok(task) = started.recv() {
                    task();
                }
            })
            .map_err(PipeError::SpawnFailed)?;
        Ok(NativeWorker { start, native })
    }

    /// Reserve a blocking job's admission (owner running, one credit, a
    /// native slot and its thread) before building `work` and `done`: a
    /// refusal leaves the caller everything it would have moved into them,
    /// and spawning from the permit cannot fail.
    ///
    /// ```ignore
    /// let Ok(permit) = ctx.reserve_blocking_job(JobOptions::new()) else {
    ///     return self.report(id, Verdict::Overloaded); // `id` was not moved
    /// };
    /// permit.pipe_blocking(move |_| check(&object), move |res| Checked { id, res });
    /// ```
    pub fn reserve_blocking_job(
        &self,
        opts: JobOptions,
    ) -> Result<BlockingJobPermit<A>, PipeError> {
        debug_assert!(
            !opts.cancel_on_caller_gone,
            "cancel_on_caller_gone applies to pipe_reply* only"
        );
        let credit = self.reserve_credit()?;
        let worker = self.spawn_worker()?;
        Ok(BlockingJobPermit {
            ctx: self.clone(),
            opts,
            credit,
            worker,
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
                    run.notify(Err(JobError::DeadlineExceeded));
                }
            }))),
        });
        // The run owns it; the heap only refers to it.
        let _ = run.deadline.set(deadline.clone());
        if at <= now_ticks() {
            deadline.expire(at);
        } else {
            let item: Weak<dyn Expirable> = Arc::downgrade(&deadline) as Weak<dyn Expirable>;
            self.replies.track(at, item);
        }
    }

    fn launch_blocking<T, M, R, W>(
        &self,
        opts: JobOptions,
        state: Arc<JobState>,
        hold: Hold<R>,
        worker: NativeWorker<A>,
        work: W,
        done: Done<T, M>,
    ) -> JobHandle
    where
        A: Handler<M>,
        M: Message,
        T: Send + 'static,
        R: Send + 'static,
        W: FnOnce(JobCtx<A>) -> T + Send + 'static,
    {
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
        let NativeWorker { start, native } = worker;
        let task: NativeTask = Box::new(move || {
            let _ended = ended;
            let result = std::panic::catch_unwind(AssertUnwindSafe(|| work(job)));
            // Execution ended: release the native slot now; the run returns
            // the credit when the terminal message is enqueued.
            drop(native);
            run.finish(caught(result), Box::new(hold));
        });
        // The worker thread is parked on `recv` until this send (or its
        // sender's drop); it cannot have exited.
        let _ = start.send(task);
        JobHandle::new(state)
    }

    /// Run blocking `work` as a job on an owned native thread, under the
    /// actor's `MAX_NATIVE_JOBS` budget, and deliver `done(outcome)` as one
    /// message in a later turn. A panic arrives as
    /// `Err(JobError::Panicked)`. `work` gets the [`JobCtx`] to
    /// check cancellation and `emit_blocking` fenced messages. Rejects at
    /// once at a bound; never waits. On `Err`, `work` and `done` are
    /// dropped: use [`reserve_blocking_job`](Self::reserve_blocking_job) to
    /// keep them.
    pub fn pipe_blocking<T, M, W, D>(
        &self,
        opts: JobOptions,
        work: W,
        done: D,
    ) -> Result<JobHandle, PipeError>
    where
        A: Handler<M>,
        M: Message,
        T: Send + 'static,
        W: FnOnce(JobCtx<A>) -> T + Send + 'static,
        D: FnOnce(JobOutcome<T>) -> M + Send + 'static,
    {
        Ok(self.reserve_blocking_job(opts)?.pipe_blocking(work, done))
    }

    /// Delegate a deferred reply: run blocking `work` on an owned native
    /// thread, linked to `token`'s request, and deliver `done(outcome, token)`
    /// in a later turn. The obligation and credit stay in this actor: the
    /// token and its first linked job share one credit. On every error the
    /// token comes back. With [`JobOptions::cancel_on_caller_gone`] the job
    /// is cancelled when the caller leaves or the request's deadline passes
    /// (off by default, as in OTP).
    pub fn pipe_reply_blocking<R, T, M, W, D>(
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
        W: FnOnce(JobCtx<A>) -> T + Send + 'static,
        D: FnOnce(JobOutcome<T>, ReplyTo<R>) -> M + Send + 'static,
    {
        let (state, slot, token) = self.link_reply(&opts, token)?;
        let hold = Hold::Linked(LinkedJob(slot.clone()));
        let worker = match self.spawn_worker() {
            Ok(worker) => worker,
            // Dropping the hold detaches the linked job from the slot.
            Err(error) => {
                drop(hold);
                return Err(PipeReplyError::Failed { token, error });
            }
        };
        if !self.admit_job(&state, &opts) {
            drop((hold, worker));
            return Err(PipeReplyError::Failed {
                token,
                error: PipeError::OwnerStopping,
            });
        }
        if opts.cancel_on_caller_gone {
            crate::reply::jobs::cancel_when_closed(&slot, &state);
        }
        let done: Done<T, M> = Box::new(move |outcome| done(outcome, token));
        Ok(self.launch_blocking(opts, state, hold, worker, work, done))
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
        if self.replies.is_exiting() {
            return Err(PipeReplyError::Failed {
                token,
                error: PipeError::OwnerStopping,
            });
        }
        let state = JobState::new(self.id(), opts.emit_credits);
        let slot = token.slot_arc().clone();
        match slot.attach(&now_ticks) {
            Ok(()) => Ok((state, slot, token)),
            Err(Refused::Closed) => Err(PipeReplyError::Closed(token)),
            Err(Refused::Overloaded) => Err(PipeReplyError::Failed {
                token,
                error: PipeError::Overloaded,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::threads::ActorStart;

    struct Owner;
    impl Actor for Owner {}
    struct Note;
    impl Message for Note {
        type Result = ();
    }
    impl Handler<Note> for Owner {
        fn handle(&mut self, _: Note, _: PendingReply<'_, ()>, _: &Context<Self>) {}
    }

    /// Review round 2 (agy 2): dropping an unused permit returns its
    /// `MAX_NATIVE_JOBS` slot synchronously, so the next reservation
    /// succeeds at once. The slot used to live on the parked thread and
    /// return only when that thread woke up and exited.
    #[test]
    fn a_dropped_unused_permit_returns_its_native_slot_at_once() {
        struct One;
        impl Actor for One {
            const MAX_NATIVE_JOBS: Option<usize> = Some(1);
        }
        let owner = One.start();
        let ctx = owner.context();
        for i in 0..2000 {
            let permit = ctx
                .reserve_blocking_job(JobOptions::new())
                .unwrap_or_else(|e| panic!("reservation {i} refused: {e}"));
            drop(permit);
        }
        ctx.stop();
        owner.join();
    }

    /// Review P1 (Codex 13), threads mode: see the tasks-mode test.
    #[test]
    fn a_refused_emit_returns_its_credit_once() {
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
        owner.join();
    }

    /// `cancel_on_caller_gone` applies to reply jobs only: a plain job has no
    /// caller, so setting it there is a bug, caught in debug builds.
    #[cfg(debug_assertions)]
    #[test]
    fn cancel_on_caller_gone_on_a_plain_job_is_a_bug() {
        let owner = Owner.start();
        let ctx = owner.context();
        let refused = std::panic::catch_unwind(AssertUnwindSafe(|| {
            ctx.reserve_blocking_job(JobOptions::new().cancel_on_caller_gone())
                .map(drop)
        }));
        assert!(refused.is_err(), "the option was accepted on a plain job");
        let plain = ctx.reserve_blocking_job(JobOptions::new()).map(drop);
        assert!(plain.is_ok());
        ctx.stop();
        owner.join();
    }
}
