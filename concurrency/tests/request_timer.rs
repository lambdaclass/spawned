//! Caller timers and service deadlines stay in their own clock domains.
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

#[cfg(not(spawned_sim))]
#[tokio::test(start_paused = true)]
async fn wall_time_while_paused_cannot_expire_a_new_request() {
    // Force the real-clock epoch to precede the wait, just as another actor
    // would. Tokio's paused clock does not advance during synchronous work.
    let _ = Deadline::after(Duration::from_secs(1));
    std::thread::sleep(Duration::from_millis(100));
    let actor = Idle(None).start();
    let duration = Duration::from_millis(20);
    let began = tokio::time::Instant::now();
    assert!(matches!(
        actor.request_with_timeout(Park, duration).await,
        Err(ActorError::RequestTimeout)
    ));
    assert!(
        began.elapsed() >= duration,
        "slot expired before runtime timeout"
    );
    assert!(began.elapsed() <= duration + Duration::from_millis(2));
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
