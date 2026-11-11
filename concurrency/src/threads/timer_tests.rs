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
    fn handle(
        &mut self,
        _msg: Inc,
        reply: crate::reply::PendingReply<'_, ()>,
        _ctx: &Context<Self>,
    ) {
        self.count += 1;
        reply.reply(());
    }
}

impl Handler<StopTimer> for Repeater {
    fn handle(
        &mut self,
        _msg: StopTimer,
        reply: crate::reply::PendingReply<'_, ()>,
        _ctx: &Context<Self>,
    ) {
        if let Some(ct) = self.cancellation_token.clone() {
            ct.cancel();
        }
        reply.reply(());
    }
}

impl Handler<GetRepCount> for Repeater {
    fn handle(
        &mut self,
        _msg: GetRepCount,
        reply: crate::reply::PendingReply<'_, i32>,
        _ctx: &Context<Self>,
    ) {
        reply.reply(self.count);
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
    fn handle(
        &mut self,
        _msg: Inc,
        reply: crate::reply::PendingReply<'_, ()>,
        _ctx: &Context<Self>,
    ) {
        self.count += 1;
        reply.reply(());
    }
}

impl Handler<GetDelCount> for Delayed {
    fn handle(
        &mut self,
        _msg: GetDelCount,
        reply: crate::reply::PendingReply<'_, i32>,
        _ctx: &Context<Self>,
    ) {
        reply.reply(self.count);
    }
}

impl Handler<StopDelayed> for Delayed {
    fn handle(
        &mut self,
        _msg: StopDelayed,
        reply: crate::reply::PendingReply<'_, i32>,
        ctx: &Context<Self>,
    ) {
        let __reply_value: i32 = {
            ctx.stop();
            self.count
        };
        reply.reply(__reply_value);
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

// --- T4: threads timers use the monotonic clock (`recv_timeout`) ---

/// Logs the monotonic instant of every handled `Fired` and of every clone
/// of `Stamp` (send_interval clones the message once per send).
struct Clocked {
    handled: std::sync::Arc<std::sync::Mutex<Vec<std::time::Instant>>>,
    stall: Option<Duration>,
}
impl Actor for Clocked {}

#[derive(Debug)]
struct Fired;
impl Message for Fired {
    type Result = ();
}
impl Handler<Fired> for Clocked {
    fn handle(&mut self, _: Fired, __reply: crate::reply::PendingReply<'_, ()>, _: &Context<Self>) {
        let __value: () = {
            self.handled.lock().unwrap().push(std::time::Instant::now());
        };
        __reply.reply(__value);
    }
}

#[derive(Debug)]
struct Stamp(std::sync::Arc<std::sync::Mutex<Vec<std::time::Instant>>>);
impl Clone for Stamp {
    fn clone(&self) -> Self {
        self.0.lock().unwrap().push(std::time::Instant::now());
        Stamp(self.0.clone())
    }
}
impl Message for Stamp {
    type Result = ();
}
impl Handler<Stamp> for Clocked {
    fn handle(&mut self, _: Stamp, __reply: crate::reply::PendingReply<'_, ()>, _: &Context<Self>) {
        let __value: () = {
            self.handled.lock().unwrap().push(std::time::Instant::now());
            // The first tick stalls the actor for several periods.
            if let Some(stall) = self.stall.take() {
                rt::sleep(stall);
            }
        };
        __reply.reply(__value);
    }
}

/// T1/T4: `send_after` measures its delay on the monotonic clock: it never
/// fires early, fires once, and a long delay cancelled halfway never fires.
#[test]
pub fn threads_send_after_uses_the_monotonic_clock_over_long_delays() {
    let handled = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let actor = Clocked {
        handled: handled.clone(),
        stall: None,
    }
    .start();
    let delay = Duration::from_millis(300);
    let long = Duration::from_millis(1_500);
    let armed = std::time::Instant::now();
    let _short = send_after(delay, Context::from_ref(&actor), Fired);
    let cancelled = send_after(long, Context::from_ref(&actor), Fired);
    assert!(
        wait_for(|| handled.lock().unwrap().len(), |n| *n >= 1) >= 1,
        "timer never fired"
    );
    let fired = handled.lock().unwrap()[0];
    assert!(
        fired.duration_since(armed) >= delay,
        "T4: send_after fired after {:?}, before its {delay:?}",
        fired.duration_since(armed)
    );
    cancelled.cancellation_token.cancel();
    let _uncancelled = send_after(long, Context::from_ref(&actor), Fired);
    let armed_long = std::time::Instant::now();
    assert!(
        wait_for(|| handled.lock().unwrap().len(), |n| *n >= 2) >= 2,
        "timer never fired"
    );
    let fired_long = handled.lock().unwrap()[1];
    assert!(
        fired_long.duration_since(armed_long) >= long,
        "T4: long send_after fired after {:?}",
        fired_long.duration_since(armed_long)
    );
    // The cancelled 1.5 s timer was due before the uncancelled one fired.
    assert!(armed + long <= fired_long);
    assert_eq!(handled.lock().unwrap().len(), 2, "T1: fired more than once");
    actor.child_handle().stop();
    actor.join();
}

/// T2/T4: threads `send_interval` is fixed delay on the monotonic clock:
/// consecutive sends are at least one period apart, also while the actor is
/// stalled for 3.5 periods, so there is no burst to catch up.
#[test]
pub fn threads_send_interval_is_fixed_delay_while_the_actor_stalls() {
    let period = Duration::from_millis(50);
    let sends = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let handled = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let actor = Clocked {
        handled: handled.clone(),
        stall: Some(period * 7 / 2),
    }
    .start();
    let timer = send_interval(period, Context::from_ref(&actor), Stamp(sends.clone()));
    assert!(
        wait_for(|| handled.lock().unwrap().len(), |n| *n >= 10) >= 10,
        "timer never fired"
    );
    timer.cancellation_token.cancel();
    let sends = sends.lock().unwrap().clone();
    assert!(sends.len() >= 10, "{}", sends.len());
    for pair in sends.windows(2) {
        assert!(
            pair[1].duration_since(pair[0]) >= period,
            "T2/T4: two sends {:?} apart, period {period:?}",
            pair[1].duration_since(pair[0])
        );
    }
    actor.child_handle().stop();
    actor.join();
}
