//! Threads-mode scenarios (no async runtime), run on their own OS-thread lane.
use crate::state::{e, ensure, leaks, Live, Rng, Stats, LIMIT, R, STARTED};
use spawned_concurrency::{
    message::Message,
    registry,
    threads::{
        send_after, send_interval, spawn_listener, Actor, ActorRef, ActorStart, Context, Handler,
        TimerHandle,
    },
    ActorError, ChildHandle, Down, Exit, ExitReason, MonitorRef,
};
use std::time::{Duration, Instant};

pub struct NWorker {
    _live: Live,
    value: u64,
    timers: Vec<TimerHandle>,
}
impl NWorker {
    fn new() -> Self {
        Self {
            _live: Live::new(),
            value: 0,
            timers: Vec::new(),
        }
    }
}
impl Actor for NWorker {
    fn started(&mut self, _: &Context<Self>) {
        STARTED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}
macro_rules! message {
    ($name:ident $(($($field:ty),*))? => $result:ty) => {
        pub struct $name $(($(pub $field),*))?;
        impl Message for $name {
            type Result = $result;
        }
    };
}
message!(Inc => u64);
message!(Get => u64);
message!(Crash => ());
message!(Slow(Duration) => ());
message!(ArmTimers(u64, u64, Duration) => ());
message!(CancelTimers => ());
#[derive(Clone)]
pub struct Bump;
impl Message for Bump {
    type Result = ();
}
impl Handler<Inc> for NWorker {
    fn handle(
        &mut self,
        _: Inc,
        reply: spawned_concurrency::PendingReply<'_, u64>,
        _: &Context<Self>,
    ) {
        let value = {
            self.value += 1;
            self.value
        };
        reply.reply(value);
    }
}
impl Handler<Get> for NWorker {
    fn handle(
        &mut self,
        _: Get,
        reply: spawned_concurrency::PendingReply<'_, u64>,
        _: &Context<Self>,
    ) {
        let value = { self.value };
        reply.reply(value);
    }
}
impl Handler<Bump> for NWorker {
    fn handle(
        &mut self,
        _: Bump,
        reply: spawned_concurrency::PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
        self.value += 1;
        reply.reply(());
    }
}
impl Handler<Crash> for NWorker {
    fn handle(
        &mut self,
        _: Crash,
        _reply: spawned_concurrency::PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
        panic!("soak: injected crash");
    }
}
impl Handler<Slow> for NWorker {
    fn handle(
        &mut self,
        msg: Slow,
        reply: spawned_concurrency::PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
        std::thread::sleep(msg.0);
        reply.reply(());
    }
}
impl Handler<ArmTimers> for NWorker {
    fn handle(
        &mut self,
        msg: ArmTimers,
        reply: spawned_concurrency::PendingReply<'_, ()>,
        ctx: &Context<Self>,
    ) {
        let ArmTimers(once, every, period) = msg;
        for i in 0..once {
            let delay = period * (i as u32 + 1);
            self.timers.push(send_after(delay, ctx.clone(), Bump));
        }
        for _ in 0..every {
            self.timers.push(send_interval(period, ctx.clone(), Bump));
        }
        reply.reply(());
    }
}
impl Handler<CancelTimers> for NWorker {
    fn handle(
        &mut self,
        _: CancelTimers,
        reply: spawned_concurrency::PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
        for timer in self.timers.drain(..) {
            timer.cancellation_token.cancel();
        }
        reply.reply(());
    }
}

pub struct NWatcher {
    _live: Live,
    downs: u64,
    exits: u64,
}
impl Actor for NWatcher {
    fn started(&mut self, _: &Context<Self>) {
        STARTED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    fn exit_received(&mut self, _: Exit, _: &Context<Self>) {
        self.exits += 1;
    }
}
message!(Watch(ChildHandle) => MonitorRef);
message!(Unwatch(MonitorRef) => ());
message!(LinkTo(ChildHandle) => ());
message!(Trap(bool) => ());
message!(Counts => (u64, u64));
impl Handler<Watch> for NWatcher {
    fn handle(
        &mut self,
        msg: Watch,
        reply: spawned_concurrency::PendingReply<'_, MonitorRef>,
        ctx: &Context<Self>,
    ) {
        let value = { ctx.monitor(&msg.0) };
        reply.reply(value);
    }
}
impl Handler<Unwatch> for NWatcher {
    fn handle(
        &mut self,
        msg: Unwatch,
        reply: spawned_concurrency::PendingReply<'_, ()>,
        ctx: &Context<Self>,
    ) {
        ctx.demonitor(msg.0);
        reply.reply(());
    }
}
impl Handler<LinkTo> for NWatcher {
    fn handle(
        &mut self,
        msg: LinkTo,
        reply: spawned_concurrency::PendingReply<'_, ()>,
        ctx: &Context<Self>,
    ) {
        ctx.link(&msg.0);
        reply.reply(());
    }
}
impl Handler<Trap> for NWatcher {
    fn handle(
        &mut self,
        msg: Trap,
        reply: spawned_concurrency::PendingReply<'_, ()>,
        ctx: &Context<Self>,
    ) {
        ctx.trap_exit(msg.0);
        reply.reply(());
    }
}
impl Handler<Counts> for NWatcher {
    fn handle(
        &mut self,
        _: Counts,
        reply: spawned_concurrency::PendingReply<'_, (u64, u64)>,
        _: &Context<Self>,
    ) {
        let value = { (self.downs, self.exits) };
        reply.reply(value);
    }
}
impl Handler<Down> for NWatcher {
    fn handle(
        &mut self,
        _: Down,
        reply: spawned_concurrency::PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
        self.downs += 1;
        reply.reply(());
    }
}

thread_local! {
    /// Actors started by the current episode; see `workload::Scope`.
    static SCOPE: std::cell::RefCell<Vec<ChildHandle>> = const { std::cell::RefCell::new(Vec::new()) };
}
fn track<A: Actor>(actor: ActorRef<A>) -> ActorRef<A> {
    SCOPE.with(|s| s.borrow_mut().push(actor.child_handle()));
    actor
}
/// Forget the last episode's actors, stopping them if it failed half-way.
pub fn clean_up(failed: bool) {
    for actor in SCOPE.with(|s| std::mem::take(&mut *s.borrow_mut())) {
        if failed {
            actor.stop();
        }
    }
}

fn watcher() -> ActorRef<NWatcher> {
    track(
        NWatcher {
            _live: Live::new(),
            downs: 0,
            exits: 0,
        }
        .start(),
    )
}

/// Exit within the limit: threads-mode `wait_exit` blocks without a timeout,
/// so poll the exit reason instead.
fn exit<A: Actor>(actor: &ActorRef<A>) -> Result<ExitReason, String> {
    let deadline = Instant::now() + LIMIT;
    loop {
        if let Some(reason) = actor.exit_reason() {
            return Ok(reason);
        }
        ensure!(Instant::now() < deadline, "threads actor did not exit");
        std::thread::sleep(Duration::from_micros(200));
    }
}

fn eventually(what: &str, mut check: impl FnMut() -> Result<bool, String>) -> R {
    let deadline = Instant::now() + LIMIT;
    while !check()? {
        ensure!(Instant::now() < deadline, "timed out: {what}");
        std::thread::sleep(Duration::from_micros(500));
    }
    Ok(())
}

fn finish(actor: &ActorRef<NWorker>, rng: &mut Rng) -> R {
    match rng.below(4) {
        0 => {
            ensure!(actor.request(Crash).is_err(), "crash request returned Ok");
            let reason = exit(actor)?;
            ensure!(
                matches!(reason, ExitReason::Panic(_)),
                "crash exit {reason:?}"
            );
        }
        1 => {
            actor.child_handle().stop_with_reason(ExitReason::Shutdown);
            let reason = exit(actor)?;
            ensure!(reason == ExitReason::Shutdown, "shutdown exit {reason:?}");
        }
        _ => {
            actor.child_handle().stop();
            let reason = exit(actor)?;
            ensure!(reason == ExitReason::Normal, "stop exit {reason:?}");
        }
    }
    Ok(())
}

fn stop_watcher(watcher: &ActorRef<NWatcher>) -> R {
    watcher.child_handle().stop();
    let reason = exit(watcher)?;
    ensure!(reason == ExitReason::Normal, "watcher exit {reason:?}");
    Ok(())
}

pub fn run(index: usize, rng: &mut Rng, stats: &Stats) -> R {
    match index {
        0 => churn(rng),
        1 => links_monitors(rng),
        2 => timers_streams(rng),
        3 => registry_requests(rng, stats),
        4 => burst(rng),
        _ => unreachable!(),
    }
}
pub const COUNT: usize = 5;

fn churn(rng: &mut Rng) -> R {
    let actors: Vec<_> = (0..1 + rng.below(4))
        .map(|_| track(NWorker::new().start()))
        .collect();
    for actor in &actors {
        for expected in 1..=1 + rng.below(3) {
            let got = actor.request(Inc).map_err(e("inc"))?;
            ensure!(got == expected, "inc returned {got}, expected {expected}");
        }
    }
    for actor in &actors {
        finish(actor, rng)?;
    }
    Ok(())
}

fn links_monitors(rng: &mut Rng) -> R {
    let watcher = watcher();
    let peer = track(NWorker::new().start());
    if rng.one_in(2) {
        let trap = rng.one_in(2);
        watcher.request(Trap(trap)).map_err(e("trap"))?;
        watcher
            .request(LinkTo(peer.child_handle()))
            .map_err(e("link"))?;
        let _ = peer.request(Crash);
        exit(&peer)?;
        if trap {
            eventually("trapped exit delivered", || {
                Ok(watcher.request(Counts).map_err(e("counts"))?.1 == 1)
            })?;
            stop_watcher(&watcher)
        } else {
            let reason = exit(&watcher)?;
            ensure!(reason.is_abnormal(), "linked peer exit {reason:?}");
            Ok(())
        }
    } else {
        let monitor = watcher
            .request(Watch(peer.child_handle()))
            .map_err(e("watch"))?;
        let expected = if rng.one_in(4) {
            watcher.request(Unwatch(monitor)).map_err(e("unwatch"))?;
            0
        } else {
            1
        };
        finish(&peer, rng)?;
        eventually("Down delivered", || {
            Ok(watcher.request(Counts).map_err(e("counts"))?.0 == expected)
        })?;
        stop_watcher(&watcher)
    }
}

fn timers_streams(rng: &mut Rng) -> R {
    let actor = track(NWorker::new().start());
    if rng.one_in(2) {
        let once = 1 + rng.below(3);
        let period = Duration::from_millis(1 + rng.below(3));
        actor
            .request(ArmTimers(once, rng.below(2), period))
            .map_err(e("arm"))?;
        std::thread::sleep(rng.ms(6));
        if rng.one_in(2) {
            actor.request(CancelTimers).map_err(e("cancel"))?;
        }
    } else {
        let n = 1 + rng.below(64);
        drop(spawn_listener(actor.context(), (0..n).map(|_| Bump)));
        eventually("finite listener drained", || {
            Ok(actor.request(Get).map_err(e("get"))? == n)
        })?;
    }
    finish(&actor, rng)
}

fn registry_requests(rng: &mut Rng, stats: &Stats) -> R {
    let actor = track(NWorker::new().start());
    let id = stats.next_id();
    let name = format!("soak-native-{id}");
    registry::register(&name, actor.clone()).map_err(e("register"))?;
    let found = registry::whereis::<ActorRef<NWorker>>(&name).map(|a| a.id());
    ensure!(found == Some(actor.id()), "whereis {name} -> {found:?}");
    if !(leaks().registry && id.is_multiple_of(4)) {
        registry::unregister(&name);
    }
    let slow = actor.request_with_timeout(
        Slow(Duration::from_millis(20 + rng.below(20))),
        Duration::from_millis(1),
    );
    ensure!(
        // A requester descheduled past the handler can see the reply first.
        matches!(slow, Err(ActorError::RequestTimeout) | Ok(())),
        "slow request: {slow:?}"
    );
    ensure!(actor.request(Inc).map_err(e("inc"))? == 1, "inc");
    finish(&actor, rng)?;
    ensure!(
        matches!(actor.request(Inc), Err(ActorError::NoProc)),
        "request to a stopped actor (NoProc past its death point)"
    );
    Ok(())
}

fn burst(rng: &mut Rng) -> R {
    let actor = track(NWorker::new().start());
    let n = 1_000 + rng.below(10_000);
    for _ in 0..n {
        actor.send(Bump).map_err(e("burst send"))?;
    }
    if !rng.one_in(3) {
        let got = actor.request_with_timeout(Get, LIMIT).map_err(e("drain"))?;
        ensure!(got == n, "drained {got} of {n}");
    }
    finish(&actor, rng)
}
