//! Records reply-slot traces from real actors for TLA+ trace validation
//! against `verification/tla/DeferredReplies.tla` (verification/tla/README.md).
//! Each scenario writes `replies_<name>.json` to `$LIFECYCLE_TRACE_DIR`
//! (default `target/lifecycle-traces`); `verification/tla/trace_check.py`
//! checks them (schema `replies-v1`).
#![cfg(feature = "lifecycle-trace")]

use spawned_concurrency::{
    lifecycle_trace,
    message::Message,
    tasks::{Actor, ActorRef, ActorStart, Context, Deadline, Handler, PendingReply, ReplyTo},
    ActorError, ExitReason,
};
use spawned_rt::tasks::sleep;
use std::{path::PathBuf, time::Duration};

const BOUND: usize = 1;

fn trace_dir() -> PathBuf {
    std::env::var_os("LIFECYCLE_TRACE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/lifecycle-traces")
        })
}

#[derive(Default)]
struct Owner {
    stored: Vec<ReplyTo<u32>>,
}

impl Actor for Owner {
    const MAX_OUTSTANDING: usize = BOUND;
}

/// Replied in its turn.
struct Echo(u32);
impl Message for Echo {
    type Result = u32;
}
impl Handler<Echo> for Owner {
    async fn handle(&mut self, msg: Echo, reply: PendingReply<'_, u32>, _: &Context<Self>) {
        reply.reply(msg.0);
    }
}

/// Deferred and stored for a later turn (`Overloaded` at the bound).
struct Hold;
impl Message for Hold {
    type Result = u32;
}
impl Handler<Hold> for Owner {
    async fn handle(&mut self, _: Hold, reply: PendingReply<'_, u32>, _: &Context<Self>) {
        if let Ok(token) = reply.defer() {
            self.stored.push(token);
        }
    }
}

/// Never answered: the unused obligation answers `ReplyDropped` at turn end.
struct Ignore;
impl Message for Ignore {
    type Result = u32;
}
impl Handler<Ignore> for Owner {
    async fn handle(&mut self, _: Ignore, _reply: PendingReply<'_, u32>, _: &Context<Self>) {}
}

/// Deferred, then the turn panics: the token is held until the death point,
/// which answers it `ActorStopped(Panic(..))`.
struct Boom;
impl Message for Boom {
    type Result = u32;
}
impl Handler<Boom> for Owner {
    async fn handle(&mut self, _: Boom, reply: PendingReply<'_, u32>, _: &Context<Self>) {
        let _token = reply.defer();
        panic!("boom");
    }
}

/// A sent (not requested) message: answer, or drop, every stored token.
enum Flush {
    Reply,
    Drop,
}
impl Message for Flush {
    type Result = ();
}
impl Handler<Flush> for Owner {
    async fn handle(&mut self, msg: Flush, _: PendingReply<'_, ()>, _: &Context<Self>) {
        for token in self.stored.drain(..) {
            match msg {
                Flush::Reply => {
                    token.reply(7);
                }
                Flush::Drop => drop(token),
            }
        }
    }
}

/// Wait until every message sent before has been handled.
struct Sync;
impl Message for Sync {
    type Result = ();
}
impl Handler<Sync> for Owner {
    async fn handle(&mut self, _: Sync, reply: PendingReply<'_, ()>, _: &Context<Self>) {
        reply.reply(());
    }
}

/// Start an owner whose reply slots are traced.
fn start() -> ActorRef<Owner> {
    let actor = Owner::default().start();
    lifecycle_trace::watch_deferred(id(&actor));
    actor
}

fn id(actor: &ActorRef<Owner>) -> u64 {
    actor.id().to_string().parse().unwrap()
}

/// Write the owner's reply trace once it is dead.
fn write(name: &str, owner: u64) -> Vec<String> {
    let events = lifecycle_trace::take_replies(owner);
    assert_eq!(
        events.last().map(|e| e.action),
        Some("Death"),
        "{name}: the owner's death was not recorded"
    );
    let json = format!(
        "{{\"name\":\"replies_{name}\",\"schema\":\"replies-v1\",\"bound\":{BOUND},\"events\":[\n{}\n]}}\n",
        events
            .iter()
            .map(|e| e.to_json())
            .collect::<Vec<_>>()
            .join(",\n")
    );
    let dir = trace_dir();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("replies_{name}.json")), json).unwrap();
    events
        .iter()
        .map(|e| {
            format!(
                "{}{}",
                e.action,
                if e.x.is_empty() {
                    String::new()
                } else {
                    format!(":{}", e.x)
                }
            )
        })
        .collect()
}

async fn stop(name: &str, actor: ActorRef<Owner>) -> Vec<String> {
    let owner = id(&actor);
    actor.context().stop();
    actor.join().await;
    write(name, owner)
}

#[tokio::test]
async fn reply_in_turn() {
    let actor = start();
    let got = actor.request(Echo(3)).await;
    assert!(matches!(got, Ok(3)), "{got:?}");
    let events = stop("in_turn", actor).await;
    assert_eq!(
        events,
        ["Send", "Dispatch", "Discharge:ok", "Exit", "Death"]
    );
}

#[tokio::test]
async fn stored_token_answered_later() {
    let actor = start();
    let held = actor
        .send_request(Hold, spawned_concurrency::DEFAULT_REQUEST_TIMEOUT)
        .unwrap();
    actor.request(Sync).await.unwrap();
    actor.send(Flush::Reply).unwrap();
    let got = held.await;
    assert!(matches!(got, Ok(7)), "{got:?}");
    stop("later_reply", actor).await;
}

#[tokio::test]
async fn stored_token_dropped_later() {
    let actor = start();
    let held = actor
        .send_request(Hold, spawned_concurrency::DEFAULT_REQUEST_TIMEOUT)
        .unwrap();
    actor.request(Sync).await.unwrap();
    actor.send(Flush::Drop).unwrap();
    let got = held.await;
    assert!(matches!(got, Err(ActorError::ReplyDropped)), "{got:?}");
    stop("later_drop", actor).await;
}

#[tokio::test]
async fn unused_obligation_answers_dropped() {
    let actor = start();
    let got = actor.request(Ignore).await;
    assert!(matches!(got, Err(ActorError::ReplyDropped)), "{got:?}");
    stop("unused", actor).await;
}

#[tokio::test]
async fn defer_at_the_bound_is_overloaded() {
    let actor = start();
    let first = actor
        .send_request(Hold, spawned_concurrency::DEFAULT_REQUEST_TIMEOUT)
        .unwrap();
    let got = actor.request(Hold).await;
    assert!(matches!(got, Err(ActorError::Overloaded)), "{got:?}");
    actor.send(Flush::Reply).unwrap();
    let got = first.await;
    assert!(matches!(got, Ok(7)), "{got:?}");
    stop("overloaded", actor).await;
}

#[tokio::test]
async fn stored_token_expires_at_its_deadline() {
    let actor = start();
    let deadline = Deadline::At(spawned_rt::time::Instant::now() + Duration::from_millis(30));
    let held = actor.send_request(Hold, deadline).unwrap();
    let got = held.await;
    assert!(matches!(got, Err(ActorError::RequestTimeout)), "{got:?}");
    actor.send(Flush::Reply).unwrap();
    actor.request(Sync).await.unwrap();
    stop("expired", actor).await;
}

#[tokio::test]
async fn caller_gives_up() {
    let actor = start();
    let held = actor
        .send_request(Hold, spawned_concurrency::DEFAULT_REQUEST_TIMEOUT)
        .unwrap();
    actor.request(Sync).await.unwrap();
    drop(held);
    actor.send(Flush::Reply).unwrap();
    actor.request(Sync).await.unwrap();
    stop("abandoned", actor).await;
}

#[tokio::test]
async fn death_closes_stored_tokens() {
    let actor = start();
    let held = actor
        .send_request(Hold, spawned_concurrency::DEFAULT_REQUEST_TIMEOUT)
        .unwrap();
    actor.request(Sync).await.unwrap();
    let owner = id(&actor);
    actor.context().stop();
    let got = held.await;
    assert!(
        matches!(got, Err(ActorError::ActorStopped(ExitReason::Normal))),
        "{got:?}"
    );
    actor.join().await;
    let events = write("death", owner);
    assert!(
        !events.iter().any(|e| e == "Discharge:stopped"),
        "{events:?}"
    );
}

#[tokio::test]
async fn panic_in_turn_answers_stopped() {
    let actor = start();
    // The token unwound by the panic is exit pending: answered at the death
    // point with the final reason, not by a discharge of its own.
    let got = actor.request(Boom).await;
    assert!(
        matches!(got, Err(ActorError::ActorStopped(ExitReason::Panic(_)))),
        "{got:?}"
    );
    actor.join().await;
    sleep(Duration::from_millis(10)).await;
    write("panic", id(&actor));
}
