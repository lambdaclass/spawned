use crate::message::Message;
use crate::test_wait::wait_for;
use crate::threads::{send_after, send_interval, Actor, ActorStart, Context, Handler};
use spawned_rt::threads::{self as rt, CancellationToken};
use std::time::Duration;

// --- Repeater (interval timer test) ---

#[derive(Clone, Debug)]
struct Inc;
impl Message for Inc {
    type Result = ();
}

#[derive(Clone, Debug)]
struct StopTimer;
impl Message for StopTimer {
    type Result = ();
}

#[derive(Debug)]
struct GetRepCount;
impl Message for GetRepCount {
    type Result = i32;
}

struct Repeater {
    count: i32,
    cancellation_token: Option<CancellationToken>,
}

impl Repeater {
    pub fn new(initial_count: i32) -> Self {
        Repeater {
            count: initial_count,
            cancellation_token: None,
        }
    }
}

impl Actor for Repeater {
    fn started(&mut self, ctx: &Context<Self>) {
        let timer = send_interval(Duration::from_millis(100), ctx.clone(), Inc);
        self.cancellation_token = Some(timer.cancellation_token);
    }
}

impl Handler<Inc> for Repeater {
    fn handle(&mut self, _msg: Inc, _ctx: &Context<Self>) {
        self.count += 1;
    }
}

impl Handler<StopTimer> for Repeater {
    fn handle(&mut self, _msg: StopTimer, _ctx: &Context<Self>) {
        if let Some(ct) = self.cancellation_token.clone() {
            ct.cancel();
        }
    }
}

impl Handler<GetRepCount> for Repeater {
    fn handle(&mut self, _msg: GetRepCount, _ctx: &Context<Self>) -> i32 {
        self.count
    }
}

#[test]
pub fn test_send_interval_and_cancellation() {
    let repeater = Repeater::new(0).start();

    // The interval keeps firing.
    let count = wait_for(
        || repeater.request(GetRepCount),
        |count| matches!(count, Ok(n) if *n >= 3),
    )
    .unwrap();
    assert!(count >= 3, "interval stopped firing at {count}");

    // StopTimer is handled before the next request. The timer thread checks
    // the token and then sends, so one tick that raced the cancellation may
    // still land, but no more than one.
    repeater.send(StopTimer).unwrap();
    let stopped_at = repeater.request(GetRepCount).unwrap();
    rt::sleep(Duration::from_millis(500));
    let after = repeater.request(GetRepCount).unwrap();
    assert!(
        after <= stopped_at + 1,
        "interval kept firing after cancellation: {stopped_at} -> {after}"
    );
}

// --- Delayed (send_after test) ---

#[derive(Debug)]
struct GetDelCount;
impl Message for GetDelCount {
    type Result = i32;
}

#[derive(Debug)]
struct StopDelayed;
impl Message for StopDelayed {
    type Result = i32;
}

struct Delayed {
    count: i32,
}

impl Delayed {
    pub fn new(initial_count: i32) -> Self {
        Delayed {
            count: initial_count,
        }
    }
}

impl Actor for Delayed {}

impl Handler<Inc> for Delayed {
    fn handle(&mut self, _msg: Inc, _ctx: &Context<Self>) {
        self.count += 1;
    }
}

impl Handler<GetDelCount> for Delayed {
    fn handle(&mut self, _msg: GetDelCount, _ctx: &Context<Self>) -> i32 {
        self.count
    }
}

impl Handler<StopDelayed> for Delayed {
    fn handle(&mut self, _msg: StopDelayed, ctx: &Context<Self>) -> i32 {
        ctx.stop();
        self.count
    }
}

#[test]
pub fn test_send_after_and_cancellation() {
    let actor = Delayed::new(0).start();

    let ctx = Context::from_ref(&actor);
    let _ = send_after(Duration::from_millis(100), ctx, Inc);

    let count = wait_for(
        || actor.request(GetDelCount),
        |count| matches!(count, Ok(n) if *n >= 1),
    )
    .unwrap();
    assert_eq!(1, count);

    let ctx = Context::from_ref(&actor);
    let timer = send_after(Duration::from_millis(100), ctx, Inc);

    timer.cancellation_token.cancel();

    rt::sleep(Duration::from_millis(200));

    let count2 = actor.request(GetDelCount).unwrap();
    assert_eq!(1, count2);
}

#[test]
pub fn test_send_after_actor_shutdown() {
    let actor = Delayed::new(0).start();

    let ctx = Context::from_ref(&actor);
    let _ = send_after(Duration::from_millis(100), ctx, Inc);

    let count = wait_for(
        || actor.request(GetDelCount),
        |count| matches!(count, Ok(n) if *n >= 1),
    )
    .unwrap();
    assert_eq!(1, count);

    let ctx = Context::from_ref(&actor);
    let _ = send_after(Duration::from_millis(100), ctx, Inc);

    let count2 = actor.request(StopDelayed).unwrap();

    rt::sleep(Duration::from_millis(200));

    assert_eq!(1, count2);
}
