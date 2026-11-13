//! An implicit call timeout and its slot use one captured runtime deadline.
#![cfg(feature = "async")]
use spawned_concurrency::{
    message::Message,
    tasks::{Actor, ActorStart, Context, Handler},
    ActorError, Deadline, PendingReply, ReplyTo,
};
use std::time::Duration;

struct Park;
impl Message for Park {
    type Result = bool;
}
struct Idle(Option<ReplyTo<bool>>);
impl Actor for Idle {}
impl Handler<Park> for Idle {
    async fn handle(&mut self, _: Park, reply: PendingReply<'_, bool>, _: &Context<Self>) {
        if reply.deadline() == Deadline::Never {
            reply.reply(true);
        } else {
            self.0 = Some(reply.defer().unwrap());
        }
    }
}

#[cfg(not(spawned_sim))]
#[tokio::test(start_paused = true)]
async fn requests_after_virtual_time_advance_keep_their_full_timeout() {
    // An absolute wall-clock sleep_until would incorrectly be in the past.
    tokio::time::advance(Duration::from_secs(120)).await;
    let actor = Idle(None).start();
    for duration in [Duration::from_millis(20), Duration::from_millis(40)] {
        let began = tokio::time::Instant::now();
        assert!(matches!(
            actor.request_with_timeout(Park, duration).await,
            Err(ActorError::RequestTimeout)
        ));
        assert!(began.elapsed() >= duration, "timeout fired early");
        assert!(began.elapsed() <= duration + Duration::from_millis(2));
    }
    actor.context().stop();
    actor.join().await;
}

#[spawned_macros::sim_test]
async fn overflowing_relative_timeout_remains_unbounded() {
    let actor = Idle(None).start();
    let answer = spawned_rt::tasks::timeout(
        Duration::from_secs(5),
        actor.request_with_timeout(Park, Duration::MAX),
    )
    .await
    .expect("the unbounded request should still receive its immediate reply")
    .unwrap();
    assert!(answer);
    actor.context().stop();
    actor.join().await;
}
