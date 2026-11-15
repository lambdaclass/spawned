//! Tasks-mode scenarios. Each runs one bounded lifecycle episode and checks
//! its outcome; the lanes in `main.rs` pick them from a seeded RNG forever.
use crate::state::{e, ensure, leak_on_restart, leaks, Live, Rng, Stats, LIMIT, R, STARTED};
use futures::StreamExt;
use spawned_concurrency::{
    message::Message,
    registry,
    supervision::{
        Backoff, ChildSpec, SupervisedChild, Supervisor, SupervisorConfig, SupervisorError,
        SupervisorSpec, SupervisorStrategy,
    },
    system::{ActorSystem, ExecutionGroup, SystemError},
    tasks::{
        send_after, send_interval, spawn_listener, Actor, ActorRef, ActorStart, Backend, Context,
        Handler, TimerHandle,
    },
    ActorError, ChildHandle, Down, Exit, ExitReason, MonitorRef,
};
use spawned_rt::tasks as rt;
use std::{sync::Arc, time::Duration};

/// Long-lived placement shared by every lane: one system, an async group and a
/// native thread group. Supervisors and managed actors come and go inside it.
pub struct Env {
    pub system: ActorSystem,
    pub async_group: ExecutionGroup,
    pub thread_group: ExecutionGroup,
    pub stats: Arc<Stats>,
}
impl Env {
    pub fn new(stats: Arc<Stats>) -> Result<Self, SystemError> {
        let system = ActorSystem::new()?;
        let async_group = system.async_group("soak-async", 2)?;
        let thread_group = system.thread_group("soak-native", 16)?;
        Ok(Self {
            system,
            async_group,
            thread_group,
            stats,
        })
    }
}

// ---------------------------------------------------------------------------
// Actors and messages
// ---------------------------------------------------------------------------

pub struct Worker {
    _live: Live,
    value: u64,
    timers: Vec<TimerHandle>,
}
impl Worker {
    pub fn new() -> Self {
        Self {
            _live: Live::new(),
            value: 0,
            timers: Vec::new(),
        }
    }
}
impl Actor for Worker {
    async fn started(&mut self, _: &Context<Self>) {
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
message!(RegisterManaged(String) => Result<(), String>);
#[derive(Clone)]
pub struct Bump;
impl Message for Bump {
    type Result = ();
}

impl Handler<Inc> for Worker {
    async fn handle(
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
impl Handler<Get> for Worker {
    async fn handle(
        &mut self,
        _: Get,
        reply: spawned_concurrency::PendingReply<'_, u64>,
        _: &Context<Self>,
    ) {
        let value = { self.value };
        reply.reply(value);
    }
}
impl Handler<Bump> for Worker {
    async fn handle(
        &mut self,
        _: Bump,
        reply: spawned_concurrency::PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
        self.value += 1;
        reply.reply(());
    }
}
impl Handler<Crash> for Worker {
    async fn handle(
        &mut self,
        _: Crash,
        _reply: spawned_concurrency::PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
        panic!("soak: injected crash");
    }
}
impl Handler<Slow> for Worker {
    async fn handle(
        &mut self,
        msg: Slow,
        reply: spawned_concurrency::PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
        rt::sleep(msg.0).await;
        reply.reply(());
    }
}
impl Handler<ArmTimers> for Worker {
    async fn handle(
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
impl Handler<CancelTimers> for Worker {
    async fn handle(
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
impl Handler<RegisterManaged> for Worker {
    async fn handle(
        &mut self,
        msg: RegisterManaged,
        reply: spawned_concurrency::PendingReply<'_, Result<(), String>>,
        ctx: &Context<Self>,
    ) {
        let value = { ctx.register_name(&msg.0).map_err(|e| e.to_string()) };
        reply.reply(value);
    }
}

/// Observes other actors through monitors and links.
pub struct Watcher {
    _live: Live,
    downs: u64,
    exits: u64,
}
impl Watcher {
    pub fn new() -> Self {
        Self {
            _live: Live::new(),
            downs: 0,
            exits: 0,
        }
    }
}
impl Actor for Watcher {
    async fn started(&mut self, _: &Context<Self>) {
        STARTED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    async fn exit_received(&mut self, _: Exit, _: &Context<Self>) {
        self.exits += 1;
    }
}
message!(Watch(ChildHandle) => MonitorRef);
message!(Unwatch(MonitorRef) => ());
message!(LinkTo(ChildHandle) => ());
message!(Unlink(ChildHandle) => ());
message!(Trap(bool) => ());
message!(Counts => (u64, u64));
message!(SpawnLinked => ActorRef<Worker>);
impl Handler<Watch> for Watcher {
    async fn handle(
        &mut self,
        msg: Watch,
        reply: spawned_concurrency::PendingReply<'_, MonitorRef>,
        ctx: &Context<Self>,
    ) {
        let value = { ctx.monitor(&msg.0) };
        reply.reply(value);
    }
}
impl Handler<Unwatch> for Watcher {
    async fn handle(
        &mut self,
        msg: Unwatch,
        reply: spawned_concurrency::PendingReply<'_, ()>,
        ctx: &Context<Self>,
    ) {
        ctx.demonitor(msg.0);
        reply.reply(());
    }
}
impl Handler<LinkTo> for Watcher {
    async fn handle(
        &mut self,
        msg: LinkTo,
        reply: spawned_concurrency::PendingReply<'_, ()>,
        ctx: &Context<Self>,
    ) {
        ctx.link(&msg.0);
        reply.reply(());
    }
}
impl Handler<Unlink> for Watcher {
    async fn handle(
        &mut self,
        msg: Unlink,
        reply: spawned_concurrency::PendingReply<'_, ()>,
        ctx: &Context<Self>,
    ) {
        ctx.unlink(&msg.0);
        reply.reply(());
    }
}
impl Handler<Trap> for Watcher {
    async fn handle(
        &mut self,
        msg: Trap,
        reply: spawned_concurrency::PendingReply<'_, ()>,
        ctx: &Context<Self>,
    ) {
        ctx.trap_exit(msg.0);
        reply.reply(());
    }
}
impl Handler<Counts> for Watcher {
    async fn handle(
        &mut self,
        _: Counts,
        reply: spawned_concurrency::PendingReply<'_, (u64, u64)>,
        _: &Context<Self>,
    ) {
        let value = { (self.downs, self.exits) };
        reply.reply(value);
    }
}
impl Handler<SpawnLinked> for Watcher {
    async fn handle(
        &mut self,
        _: SpawnLinked,
        reply: spawned_concurrency::PendingReply<'_, ActorRef<Worker>>,
        ctx: &Context<Self>,
    ) {
        let value = { Worker::new().start_linked(ctx) };
        reply.reply(value);
    }
}
impl Handler<Down> for Watcher {
    async fn handle(
        &mut self,
        _: Down,
        reply: spawned_concurrency::PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
        self.downs += 1;
        reply.reply(());
    }
}

// ---------------------------------------------------------------------------
// Cleanup of a failed episode
// ---------------------------------------------------------------------------

/// What one episode started. An actor lives on when its last `ActorRef` drops
/// (as in Erlang), so when a scenario fails or is timed out half-way the lane
/// stops whatever it left behind; otherwise the harness itself would leak.
#[derive(Default)]
pub struct Scope {
    actors: Vec<ChildHandle>,
    supervisors: Vec<Supervisor>,
}
impl Scope {
    pub fn clean_up(&mut self) {
        for actor in self.actors.drain(..) {
            actor.stop();
        }
        for supervisor in self.supervisors.drain(..) {
            supervisor.stop();
        }
    }
}
tokio::task_local! {
    pub static SCOPE: Arc<std::sync::Mutex<Scope>>;
}
fn track<A: Actor>(actor: ActorRef<A>) -> ActorRef<A> {
    let _ = SCOPE.try_with(|s| {
        s.lock()
            .unwrap_or_else(|p| p.into_inner())
            .actors
            .push(actor.child_handle())
    });
    actor
}
fn track_supervisor(supervisor: &Supervisor) {
    let _ = SCOPE.try_with(|s| {
        s.lock()
            .unwrap_or_else(|p| p.into_inner())
            .supervisors
            .push(supervisor.clone())
    });
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn backend(rng: &mut Rng) -> Backend {
    [Backend::Async, Backend::Blocking, Backend::Thread][rng.below(3) as usize]
}

async fn exit<A: Actor>(actor: &ActorRef<A>) -> Result<ExitReason, String> {
    rt::timeout(LIMIT, actor.wait_exit())
        .await
        .map_err(|_| "actor did not exit within the limit".to_string())
}

/// Crash, stop or shut down the actor, then check the exit reason it reports.
async fn finish(actor: &ActorRef<Worker>, rng: &mut Rng) -> R {
    match rng.below(4) {
        0 => {
            ensure!(
                actor.request(Crash).await.is_err(),
                "crash request returned Ok"
            );
            let reason = exit(actor).await?;
            ensure!(
                matches!(reason, ExitReason::Panic(_)),
                "crash exit {reason:?}"
            );
        }
        1 => {
            actor.child_handle().stop_with_reason(ExitReason::Shutdown);
            let reason = exit(actor).await?;
            ensure!(reason == ExitReason::Shutdown, "shutdown exit {reason:?}");
        }
        _ => {
            actor.child_handle().stop();
            let reason = exit(actor).await?;
            ensure!(reason == ExitReason::Normal, "stop exit {reason:?}");
        }
    }
    Ok(())
}

async fn stop_watcher(watcher: &ActorRef<Watcher>) -> R {
    watcher.child_handle().stop();
    let reason = exit(watcher).await?;
    ensure!(reason == ExitReason::Normal, "watcher exit {reason:?}");
    Ok(())
}

/// Poll `check` until it returns true or the limit passes.
async fn eventually<F, Fut>(what: &str, mut check: F) -> R
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<bool, String>>,
{
    let deadline = std::time::Instant::now() + LIMIT;
    loop {
        if check().await? {
            return Ok(());
        }
        ensure!(std::time::Instant::now() < deadline, "timed out: {what}");
        rt::sleep(Duration::from_millis(1)).await;
    }
}

// ---------------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------------

pub async fn run(index: usize, rng: &mut Rng, env: &Env) -> R {
    match index {
        0 => churn(rng, Backend::Async).await,
        1 => churn(rng, Backend::Blocking).await,
        2 => churn(rng, Backend::Thread).await,
        3 => group(rng, env).await,
        4 => supervisor(rng, env, SupervisorStrategy::OneForOne, false).await,
        5 => supervisor(rng, env, SupervisorStrategy::OneForAll, false).await,
        6 => supervisor(rng, env, SupervisorStrategy::RestForOne, false).await,
        7 => {
            let strategy = [
                SupervisorStrategy::OneForOne,
                SupervisorStrategy::OneForAll,
                SupervisorStrategy::RestForOne,
            ][rng.below(3) as usize];
            supervisor(rng, env, strategy, true).await
        }
        8 => links(rng).await,
        9 => monitors(rng).await,
        10 => timers(rng).await,
        11 => registry_names(rng, env).await,
        12 => requests(rng).await,
        13 => streams(rng).await,
        14 => system_cycle(rng).await,
        15 => burst(rng).await,
        _ => unreachable!(),
    }
}

/// Start a few actors on one backend, exercise requests, end each one.
async fn churn(rng: &mut Rng, backend: Backend) -> R {
    let actors: Vec<_> = (0..1 + rng.below(6))
        .map(|_| track(Worker::new().start_with_backend(backend)))
        .collect();
    for actor in &actors {
        for expected in 1..=1 + rng.below(3) {
            let got = actor.request(Inc).await.map_err(e("inc"))?;
            ensure!(got == expected, "inc returned {got}, expected {expected}");
        }
    }
    for actor in &actors {
        finish(actor, rng).await?;
    }
    Ok(())
}

/// Managed actors on the long-lived groups, some with owned registrations
/// that must disappear with the actor.
async fn group(rng: &mut Rng, env: &Env) -> R {
    let group = if rng.one_in(3) {
        &env.thread_group
    } else {
        &env.async_group
    };
    let mut actors = Vec::new();
    for _ in 0..1 + rng.below(3) {
        match group.start(Worker::new()) {
            Ok(actor) => actors.push(track(actor)),
            Err(SystemError::Capacity) => {
                env.stats
                    .capacity
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            Err(err) => return Err(format!("group start: {err:?}")),
        }
    }
    let mut names = Vec::new();
    for actor in &actors {
        ensure!(actor.request(Inc).await.map_err(e("inc"))? == 1, "inc");
        if rng.one_in(2) {
            let name = format!("soak-managed-{}", env.stats.next_id());
            actor
                .request(RegisterManaged(name.clone()))
                .await
                .map_err(e("register"))??;
            let found = registry::whereis::<ActorRef<Worker>>(&name).map(|a| a.id());
            ensure!(
                found == Some(actor.id()),
                "managed name {name} -> {found:?}"
            );
            names.push(name);
        }
    }
    for actor in &actors {
        finish(actor, rng).await?;
    }
    for name in names {
        eventually("managed registration removed", || {
            let gone = registry::whereis::<ActorRef<Worker>>(&name).is_none();
            async move { Ok(gone) }
        })
        .await?;
    }
    Ok(())
}

fn config(strategy: SupervisorStrategy) -> SupervisorConfig {
    SupervisorConfig::new()
        .strategy(strategy)
        .max_restarts(60_000)
        .backoff(
            Backoff::new()
                .initial(Duration::from_millis(1))
                .maximum(Duration::from_millis(2))
                .jitter(false),
        )
}

fn child(name: String, group: &ExecutionGroup) -> ChildSpec<Worker> {
    ChildSpec::new(name, group, |_| async {
        leak_on_restart();
        Ok::<_, String>(Worker::new())
    })
}

/// Three children (plus an optional nested subtree); crash some and wait for
/// their replacement generation; shut the tree down.
async fn supervisor(rng: &mut Rng, env: &Env, strategy: SupervisorStrategy, nested: bool) -> R {
    if leaks().task {
        rt::spawn(std::future::pending::<()>());
    }
    let mut tree = env.system.supervisor(config(strategy)).map_err(e("tree"))?;
    let mut children: Vec<SupervisedChild<Worker>> = Vec::new();
    for i in 0..3 {
        if nested && i == 1 {
            let placement = env.async_group.clone();
            let published = Arc::new(std::sync::Mutex::new(None));
            let slot = published.clone();
            tree.add_supervisor(SupervisorSpec::new(
                "subtree",
                &env.async_group,
                config(strategy),
                move |subtree| {
                    let leaf = subtree.add(child("leaf".into(), &placement))?;
                    *slot.lock().unwrap_or_else(|p| p.into_inner()) = Some(leaf);
                    Ok::<_, SupervisorError>(())
                },
            ))
            .map_err(e("add subtree"))?;
        }
        children.push(
            tree.add(child(format!("child-{i}"), &env.async_group))
                .map_err(e("add child"))?,
        );
    }
    let supervisor = rt::timeout(LIMIT, tree.start())
        .await
        .map_err(|_| "supervisor start timed out".to_string())?
        .map_err(e("supervisor start"))?;
    track_supervisor(&supervisor);
    for _ in 0..1 + rng.below(3) {
        let target = &children[rng.below(children.len() as u64) as usize];
        let before = target.snapshot().generation;
        let actor = rt::timeout(LIMIT, target.wait_for_generation(before.saturating_sub(1)))
            .await
            .map_err(|_| "child never ready".to_string())?
            .map_err(e("child ready"))?;
        let generation = target.snapshot().generation;
        let _ = actor.request(Crash).await;
        let next = rt::timeout(LIMIT, target.wait_for_generation(generation))
            .await
            .map_err(|_| format!("{strategy:?}: no restart after generation {generation}"))?
            .map_err(e("restart"))?;
        ensure!(next.id() != actor.id(), "restart reused the actor id");
        // The replacement may itself be replaced by a collateral restart.
        let _ = next.request(Inc).await;
    }
    let report = supervisor.shutdown(LIMIT).await;
    ensure!(
        report.complete,
        "supervisor shutdown incomplete: {report:?}"
    );
    for child in &children {
        ensure!(
            child.current().is_err(),
            "child still current after shutdown"
        );
    }
    Ok(())
}

/// Bidirectional links: trapped exits, propagated exits, `start_linked`, unlink.
async fn links(rng: &mut Rng) -> R {
    let watcher = track(Watcher::new().start());
    let peer = track(Worker::new().start_with_backend(backend(rng)));
    match rng.below(4) {
        0 => {
            watcher.request(Trap(true)).await.map_err(e("trap"))?;
            watcher
                .request(LinkTo(peer.child_handle()))
                .await
                .map_err(e("link"))?;
            let _ = peer.request(Crash).await;
            exit(&peer).await?;
            eventually("trapped exit delivered", || async {
                Ok(watcher.request(Counts).await.map_err(e("counts"))?.1 == 1)
            })
            .await?;
            stop_watcher(&watcher).await
        }
        1 => {
            watcher
                .request(LinkTo(peer.child_handle()))
                .await
                .map_err(e("link"))?;
            let _ = peer.request(Crash).await;
            exit(&peer).await?;
            let reason = exit(&watcher).await?;
            ensure!(reason.is_abnormal(), "linked peer exit {reason:?}");
            Ok(())
        }
        2 => {
            peer.child_handle().stop();
            exit(&peer).await?;
            watcher.request(Trap(true)).await.map_err(e("trap"))?;
            let child = track(watcher.request(SpawnLinked).await.map_err(e("spawn"))?);
            ensure!(child.request(Inc).await.map_err(e("inc"))? == 1, "inc");
            let _ = child.request(Crash).await;
            exit(&child).await?;
            eventually("start_linked exit delivered", || async {
                Ok(watcher.request(Counts).await.map_err(e("counts"))?.1 == 1)
            })
            .await?;
            stop_watcher(&watcher).await
        }
        _ => {
            let handle = peer.child_handle();
            watcher
                .request(LinkTo(handle.clone()))
                .await
                .map_err(e("link"))?;
            watcher.request(Unlink(handle)).await.map_err(e("unlink"))?;
            let _ = peer.request(Crash).await;
            exit(&peer).await?;
            let (_, exits) = watcher.request(Counts).await.map_err(e("counts"))?;
            ensure!(exits == 0, "exit delivered after unlink");
            stop_watcher(&watcher).await
        }
    }
}

/// Monitors, demonitors, monitoring an already-dead actor, and a watcher
/// that stops before its targets.
async fn monitors(rng: &mut Rng) -> R {
    let watcher = track(Watcher::new().start());
    let targets: Vec<_> = (0..1 + rng.below(4))
        .map(|_| track(Worker::new().start_with_backend(backend(rng))))
        .collect();
    let mut expected = 0;
    for target in &targets {
        let monitor = watcher
            .request(Watch(target.child_handle()))
            .await
            .map_err(e("watch"))?;
        if rng.one_in(4) {
            watcher
                .request(Unwatch(monitor))
                .await
                .map_err(e("unwatch"))?;
        } else {
            expected += 1;
        }
    }
    if rng.one_in(5) {
        // The watcher goes first; its monitors must not outlive it.
        stop_watcher(&watcher).await?;
        for target in &targets {
            finish(target, rng).await?;
        }
        return Ok(());
    }
    for target in &targets {
        finish(target, rng).await?;
    }
    if rng.one_in(2) {
        watcher
            .request(Watch(targets[0].child_handle()))
            .await
            .map_err(e("watch dead"))?;
        expected += 1;
    }
    eventually("every Down delivered", || async {
        Ok(watcher.request(Counts).await.map_err(e("counts"))?.0 == expected)
    })
    .await?;
    stop_watcher(&watcher).await
}

/// `send_after` and `send_interval`, cancelled, fired, or live at stop.
async fn timers(rng: &mut Rng) -> R {
    let actor = track(Worker::new().start_with_backend(backend(rng)));
    let once = 1 + rng.below(4);
    let period = Duration::from_millis(1 + rng.below(4));
    actor
        .request(ArmTimers(once, rng.below(3), period))
        .await
        .map_err(e("arm"))?;
    rt::sleep(rng.ms(8)).await;
    match rng.below(3) {
        0 => actor.request(CancelTimers).await.map_err(e("cancel"))?,
        1 => {}
        _ => {
            eventually("one-shot timers fired", || async {
                Ok(actor.request(Get).await.map_err(e("get"))? >= once)
            })
            .await?;
            actor.request(CancelTimers).await.map_err(e("cancel"))?;
        }
    }
    finish(&actor, rng).await
}

/// Unmanaged registry: register, look up, reject duplicates, unregister.
async fn registry_names(rng: &mut Rng, env: &Env) -> R {
    let actor = track(Worker::new().start());
    let leak = leaks().registry;
    for _ in 0..1 + rng.below(4) {
        let id = env.stats.next_id();
        let name = format!("soak-name-{id}");
        registry::register(&name, actor.clone()).map_err(e("register"))?;
        let found = registry::whereis::<ActorRef<Worker>>(&name).map(|a| a.id());
        ensure!(found == Some(actor.id()), "whereis {name} -> {found:?}");
        ensure!(
            registry::register(&name, actor.clone()).is_err(),
            "duplicate registration accepted"
        );
        if leak && id.is_multiple_of(4) {
            continue;
        }
        registry::unregister(&name);
        ensure!(
            registry::whereis::<ActorRef<Worker>>(&name).is_none(),
            "{name} still registered"
        );
    }
    finish(&actor, rng).await
}

/// Requests that time out, requests dropped mid-flight, requests to a stopped actor.
async fn requests(rng: &mut Rng) -> R {
    let actor = track(Worker::new().start_with_backend(backend(rng)));
    let timed_out = actor
        .request_with_timeout(
            Slow(Duration::from_millis(20 + rng.below(20))),
            Duration::from_millis(1),
        )
        .await;
    ensure!(
        // A requester descheduled past the handler can see the reply first.
        matches!(timed_out, Err(ActorError::RequestTimeout) | Ok(())),
        "slow request: {timed_out:?}"
    );
    if rng.one_in(2) {
        // Dropping the request future cancels it.
        let _ = rt::timeout(
            Duration::from_millis(1),
            actor.request(Slow(Duration::from_millis(10))),
        )
        .await;
    }
    let got = actor
        .request_with_timeout(Inc, LIMIT)
        .await
        .map_err(e("inc"))?;
    ensure!(got == 1, "inc after timeouts returned {got}");
    finish(&actor, rng).await?;
    ensure!(
        matches!(actor.request(Inc).await, Err(ActorError::NoProc)),
        "request to a stopped actor (NoProc past its death point)"
    );
    ensure!(actor.send(Bump).is_err(), "send to a stopped actor");
    Ok(())
}

/// Stream listeners: a finite stream drained into the actor, and an endless
/// one that must end when the actor stops.
async fn streams(rng: &mut Rng) -> R {
    let actor = track(Worker::new().start_with_backend(backend(rng)));
    let n = 1 + rng.below(64);
    drop(spawn_listener(
        actor.context(),
        futures::stream::iter((0..n).map(|_| Bump)),
    ));
    eventually("finite stream drained", || async {
        Ok(actor.request(Get).await.map_err(e("get"))? == n)
    })
    .await?;
    if rng.one_in(2) {
        let endless = futures::stream::repeat(()).then(|_| async {
            rt::sleep(Duration::from_millis(1)).await;
            Bump
        });
        drop(spawn_listener(actor.context(), endless));
        rt::sleep(rng.ms(5)).await;
    }
    finish(&actor, rng).await
}

/// A whole `ActorSystem` lifetime with actors still running at shutdown.
async fn system_cycle(rng: &mut Rng) -> R {
    let system = ActorSystem::new().map_err(e("system"))?;
    let group = system.async_group("cycle", 1).map_err(e("group"))?;
    let native = if rng.one_in(2) {
        Some(
            system
                .thread_group("cycle-native", 2)
                .map_err(e("native"))?,
        )
    } else {
        None
    };
    let mut actors = Vec::new();
    for _ in 0..1 + rng.below(4) {
        actors.push(group.start(Worker::new()).map_err(e("start"))?);
    }
    if let Some(native) = &native {
        actors.push(native.start(Worker::new()).map_err(e("native start"))?);
    }
    for actor in &actors {
        ensure!(actor.request(Inc).await.map_err(e("inc"))? == 1, "inc");
    }
    if rng.one_in(2) {
        let mut tree = system
            .supervisor(config(SupervisorStrategy::OneForOne))
            .map_err(e("tree"))?;
        tree.add(child("cycle-child".into(), &group))
            .map_err(e("child"))?;
        rt::timeout(LIMIT, tree.start())
            .await
            .map_err(|_| "cycle supervisor start timed out".to_string())?
            .map_err(e("cycle supervisor"))?;
    }
    let report = system.shutdown(LIMIT).await;
    ensure!(
        report.complete,
        "system shutdown incomplete: groups {:?}, actors {:?}",
        report.unfinished_groups,
        report.unfinished_actors
    );
    for actor in &actors {
        ensure!(actor.exit_reason().is_some(), "actor alive after shutdown");
    }
    Ok(())
}

/// Fill a mailbox with thousands of messages, then drain it or stop with it full.
async fn burst(rng: &mut Rng) -> R {
    let actor = track(Worker::new().start_with_backend(backend(rng)));
    let n = 1_000 + rng.below(20_000);
    for _ in 0..n {
        actor.send(Bump).map_err(e("burst send"))?;
    }
    if !rng.one_in(3) {
        let got = actor
            .request_with_timeout(Get, LIMIT)
            .await
            .map_err(e("drain"))?;
        ensure!(got == n, "drained {got} of {n}");
        ensure!(actor.mailbox_depth() == 0, "mailbox not empty after drain");
    }
    finish(&actor, rng).await
}
