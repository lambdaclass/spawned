use crate::threads::{
    send_after, send_interval, Actor, ActorStart, Context, Handler,
};
use crate::message::Message;
use spawned_rt::threads::{self as rt, CancellationToken};
use std::time::Duration;

// --- Repeater (interval timer test) ---

#[derive(Clone, Debug)]
struct Inc;
impl Message for Inc { type Result = (); }

#[derive(Clone, Debug)]
struct StopTimer;
impl Message for StopTimer { type Result = (); }

#[derive(Debug)]
struct GetRepCount;
impl Message for GetRepCount { type Result = i32; }

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
        let timer = send_interval(
            Duration::from_millis(100),
            ctx.clone(),
            Inc,
        );
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

    rt::sleep(Duration::from_secs(1));

    let count = repeater.send_request(GetRepCount).unwrap();
    assert_eq!(9, count);

    repeater.send(StopTimer).unwrap();

    rt::sleep(Duration::from_secs(1));

    let count2 = repeater.send_request(GetRepCount).unwrap();
    assert_eq!(9, count2);
}

// --- Delayed (send_after test) ---

#[derive(Debug)]
struct GetDelCount;
impl Message for GetDelCount { type Result = i32; }

#[derive(Debug)]
struct StopDelayed;
impl Message for StopDelayed { type Result = i32; }

struct Delayed {
    count: i32,
}

impl Delayed {
    pub fn new(initial_count: i32) -> Self {
        Delayed { count: initial_count }
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

    rt::sleep(Duration::from_millis(200));

    let count = actor.send_request(GetDelCount).unwrap();
    assert_eq!(1, count);

    let ctx = Context::from_ref(&actor);
    let timer = send_after(Duration::from_millis(100), ctx, Inc);

    timer.cancellation_token.cancel();

    rt::sleep(Duration::from_millis(200));

    let count2 = actor.send_request(GetDelCount).unwrap();
    assert_eq!(1, count2);
}

#[test]
pub fn test_send_after_actor_shutdown() {
    let actor = Delayed::new(0).start();

    let ctx = Context::from_ref(&actor);
    let _ = send_after(Duration::from_millis(100), ctx, Inc);

    rt::sleep(Duration::from_millis(200));

    let count = actor.send_request(GetDelCount).unwrap();
    assert_eq!(1, count);

    let ctx = Context::from_ref(&actor);
    let _ = send_after(Duration::from_millis(100), ctx, Inc);

    let count2 = actor.send_request(StopDelayed).unwrap();

    rt::sleep(Duration::from_millis(200));

    assert_eq!(1, count2);
}
