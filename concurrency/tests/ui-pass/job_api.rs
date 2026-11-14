// The 0.8 job API: one call per shape, options first, the work receives
// its `JobCtx`, `JobOutcome` is a `Result`, `PipeError` is the enum.
use spawned_concurrency::message::Message;
use spawned_concurrency::tasks::{
    Actor, Context, Deadline, Handler, JobCtx, JobError, JobOptions, JobOutcome, PendingReply,
    PipeError, PipeReplyError, ReplyTo,
};
use spawned_concurrency::Instant;
use std::num::NonZeroU16;
use std::time::Duration;

struct Owner;
impl Actor for Owner {}

struct Done(JobOutcome<u32>);
impl Message for Done {
    type Result = ();
}
struct Answered(JobOutcome<u32>, ReplyTo<u32>);
impl Message for Answered {
    type Result = ();
}

impl Handler<Done> for Owner {
    async fn handle(&mut self, msg: Done, _: PendingReply<'_, ()>, _: &Context<Self>) {
        // A plain `Result`: `?`-style handling and wildcard error arms.
        let _value: Result<u32, JobError> = msg.0;
    }
}
impl Handler<Answered> for Owner {
    async fn handle(&mut self, msg: Answered, _: PendingReply<'_, ()>, _: &Context<Self>) {
        let Answered(outcome, token) = msg;
        let _ = token.reply(outcome.unwrap_or(0));
    }
}

fn spawn_all(ctx: &Context<Owner>, token: ReplyTo<u32>) -> Result<(), PipeError> {
    let credits = NonZeroU16::new(4).unwrap();
    ctx.pipe(JobOptions::new(), |_| async { 1 }, Done)?;
    ctx.pipe(
        JobOptions::new().key("k").emit_credits(credits),
        |job: JobCtx<Owner>| async move { u32::from(job.is_cancelled()) },
        Done,
    )?;
    ctx.pipe_blocking(JobOptions::new().deadline(Duration::from_secs(1)), |_| 2, Done)?;
    ctx.pipe_blocking(JobOptions::new().deadline(Instant::now()), |_| 3, Done)?;
    ctx.pipe_blocking(JobOptions::new().deadline(Deadline::Never), |_| 4, Done)?;
    if let Ok(permit) = ctx.reserve_job(JobOptions::new()) {
        permit.pipe(|_| async { 5 }, Done);
    }
    let permit = ctx.reserve_blocking_job(JobOptions::default())?;
    permit.pipe_blocking(|job| u32::from(job.is_cancelled()), Done);
    match ctx.pipe_reply(
        JobOptions::new().cancel_on_caller_gone(),
        token,
        |_| async { 6 },
        Answered,
    ) {
        Ok(_) => {}
        Err(PipeReplyError::Failed { token, error }) => {
            let _ = matches!(error, PipeError::Overloaded | PipeError::NativeBudget);
            let _ = ctx.pipe_reply_blocking(JobOptions::new(), token, |_| 7, Answered);
        }
        Err(other) => drop(other.into_token()),
    }
    Ok(())
}

fn main() {
    let _ = spawn_all;
}
