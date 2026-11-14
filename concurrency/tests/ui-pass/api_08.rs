// The 0.8 request, deadline, notice and configuration API compiles as
// documented in MIGRATION's 0.7 to 0.8 call-site table.
use spawned_concurrency::message::Message;
use spawned_concurrency::supervision::{Backoff, SupervisorConfig, SupervisorStrategy};
use spawned_concurrency::tasks::{Actor, ActorRef, Context, Handler, PendingReply, Recipient};
use spawned_concurrency::{
    ActorError, ActorId, Deadline, Down, Exit, ExitReason, ReplyTo, Response,
    DEFAULT_REQUEST_TIMEOUT,
};
use std::time::Duration;

struct Get;
impl Message for Get {
    type Result = u32;
}
struct Counter;
impl Actor for Counter {}
impl Handler<Get> for Counter {
    async fn handle(&mut self, _: Get, reply: PendingReply<'_, u32>, _: &Context<Self>) {
        // A deadline forwards as is: `other.send_request(msg, reply.deadline())`.
        let _: Deadline = reply.deadline();
        reply.reply(1);
    }
}

fn requests(actor: &ActorRef<Counter>, recipient: &Recipient<Get>) -> Result<(), ActorError> {
    let _rx = actor.send_request(Get, Duration::from_secs(1))?;
    let _rx = actor.send_request(Get, Deadline::Never)?;
    let _rx = actor.send_request(Get, spawned_concurrency::Instant::now())?;
    let rx = recipient.send_request(Get, Deadline::after(DEFAULT_REQUEST_TIMEOUT))?;
    let _: Deadline = rx.deadline();
    let _response: Response<u32> = Response::from(actor.send_request(Get, DEFAULT_REQUEST_TIMEOUT));
    let _ctx: Context<Counter> = actor.context();
    actor.child_handle().stop_with_reason(ExitReason::Shutdown);
    Ok(())
}

fn token(token: &ReplyTo<u32>) {
    token.shorten_deadline(Duration::from_millis(5));
    let _: Deadline = token.deadline();
}

fn notices(down: Down, exit: Exit) {
    let Down { actor, reason, .. } = down;
    let _: (ActorId, ExitReason) = (actor, reason);
    let Exit { from, .. } = exit;
    let _: ActorId = from;
    let _ = Exit::new(ActorId::EXTERNAL, ExitReason::Normal);
}

fn config() -> SupervisorConfig {
    SupervisorConfig::new()
        .strategy(SupervisorStrategy::RestForOne)
        .max_restarts(3)
        .backoff(Backoff::new().jitter(false).seed(7))
}

fn main() {
    let _ = (requests, token, notices, config);
}
