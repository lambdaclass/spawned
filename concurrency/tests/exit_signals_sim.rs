//! Simulator orderings for OTP exit signals (docs/design/otp-exit-signals.md),
//! explored with PCT and bounded DFS over real actors: a kill racing
//! `stopped()`, a late signal racing the death point D, a link or monitor
//! racing D (real notice or `NoProc`), the unlink/demonitor fences against
//! delivery, a supervisor's shutdown (the parent's exit) against a trapping
//! child's queued messages, and S1's deferral of a ready child's exit racing
//! the tree's commit. Run with:
//!
//! ```sh
//! RUSTFLAGS="--cfg spawned_sim --cfg tokio_unstable" cargo test -p spawned-concurrency --test exit_signals_sim
//! ```
#![cfg(all(spawned_sim, feature = "async"))]

use spawned_concurrency::{
    message::Message,
    reply::PendingReply,
    supervision::{ChildSpec, SupervisorConfig, SupervisorStrategy},
    system::ActorSystem,
    tasks::{Actor, ActorRef, ActorStart, Context, Handler},
    Down, Exit, ExitReason, MonitorRef,
};
use spawned_rt::tasks::{
    sim::explore::{explore, Strategy},
    sleep, spawn,
};
use std::{
    collections::BTreeMap,
    future::Future,
    sync::{Arc, Mutex},
    time::Duration,
};

type Log = Arc<Mutex<Vec<String>>>;

fn push(log: &Log, entry: impl Into<String>) {
    log.lock().unwrap().push(entry.into());
}

fn entries(log: &Log) -> Vec<String> {
    log.lock().unwrap().clone()
}

fn position(log: &[String], entry: &str) -> Option<usize> {
    log.iter().position(|e| e == entry)
}

// Outcome counts per body, printed by the driver: every branch the
// assertions distinguish must be reached. Keyed by test: the simulator runs
// each body on a fresh thread named like the test thread that explores it.
static OUTCOMES: std::sync::Mutex<BTreeMap<String, BTreeMap<String, usize>>> =
    std::sync::Mutex::new(BTreeMap::new());

fn test_name() -> String {
    std::thread::current().name().unwrap_or("").to_owned()
}

fn outcomes() -> std::sync::MutexGuard<'static, BTreeMap<String, BTreeMap<String, usize>>> {
    OUTCOMES.lock().unwrap_or_else(|p| p.into_inner())
}

fn outcome(name: impl Into<String>) {
    *outcomes()
        .entry(test_name())
        .or_default()
        .entry(name.into())
        .or_default() += 1;
}

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// A logging actor: `stopped()` takes `slow_stop` virtual milliseconds.
struct Node {
    log: Log,
    trap: bool,
    slow_stop: u64,
}

impl Node {
    fn new(log: &Log, trap: bool) -> Self {
        Self {
            log: log.clone(),
            trap,
            slow_stop: 0,
        }
    }
}

impl Actor for Node {
    async fn started(&mut self, ctx: &Context<Self>) {
        if self.trap {
            ctx.trap_exit(true);
        }
    }
    async fn stopped(&mut self, ctx: &Context<Self>) {
        push(&self.log, format!("stopped:begin:{}", ctx.exit_pending()));
        for _ in 0..self.slow_stop {
            sleep(ms(1)).await;
        }
        push(&self.log, "stopped:end");
    }
    async fn exit_received(&mut self, exit: Exit, _: &Context<Self>) {
        push(&self.log, format!("exit:{:?}", exit.reason));
    }
}

impl Handler<Down> for Node {
    async fn handle(&mut self, down: Down, reply: PendingReply<'_, ()>, _: &Context<Self>) {
        push(&self.log, format!("down:{:?}", down.reason));
        reply.reply(());
    }
}

type Action = Box<dyn FnOnce(&Context<Node>) + Send>;
struct Do(Action);
impl Message for Do {
    type Result = ();
}
impl Handler<Do> for Node {
    async fn handle(&mut self, f: Do, reply: PendingReply<'_, ()>, ctx: &Context<Self>) {
        (f.0)(ctx);
        reply.reply(());
    }
}

/// Logged when handled; records whether an exit is pending.
struct Probe(u32);
impl Message for Probe {
    type Result = ();
}
impl Handler<Probe> for Node {
    async fn handle(&mut self, p: Probe, reply: PendingReply<'_, ()>, ctx: &Context<Self>) {
        push(&self.log, format!("probe:{}:{}", p.0, ctx.exit_pending()));
        reply.reply(());
    }
}

async fn on(actor: &ActorRef<Node>, f: impl FnOnce(&Context<Node>) + Send + 'static) {
    actor.request(Do(Box::new(f))).await.unwrap();
}

/// A round trip, so everything queued before it was handled.
async fn settle(actor: &ActorRef<Node>) {
    on(actor, |_| {}).await;
}

async fn stop(actor: &ActorRef<Node>) {
    let handle = actor.child_handle();
    handle.stop();
    handle.wait_exit_async().await;
}

/// A `Down` is enqueued by the monitor's waiter after the target's exit is
/// published (unlike a link notice, not before `wait_exit` returns): wait
/// for it, bounded, then settle once more so a second one would show.
async fn downs(observer: &ActorRef<Node>, log: &Log) -> Vec<String> {
    for _ in 0..100 {
        settle(observer).await;
        if !notices(&entries(log), "down:").is_empty() {
            break;
        }
        sleep(ms(1)).await;
    }
    settle(observer).await;
    notices(&entries(log), "down:")
}

fn notices(log: &[String], kind: &str) -> Vec<String> {
    log.iter()
        .filter(|e| e.starts_with(kind))
        .cloned()
        .collect()
}

// ---------------------------------------------------------------------------
// Bodies
// ---------------------------------------------------------------------------

/// E1: a kill racing an administrative stop's slow `stopped()`. Before
/// `stopped()` begins it is skipped and the actor ends `Killed`; during it,
/// it finishes and the actor ends `Killed`; after D it is ignored. A monitor
/// sees the same final reason.
async fn kill_vs_stopped(seed: u64) {
    let (log, watched) = (Log::default(), Log::default());
    let mut node = Node::new(&log, false);
    node.slow_stop = 2;
    let target = node.start();
    let watcher = Node::new(&watched, false).start();
    let handle = target.child_handle();
    let h = handle.clone();
    on(&watcher, move |ctx| {
        ctx.monitor(&h);
    })
    .await;
    let (stopper, killer) = (handle.clone(), handle.clone());
    let stop_at = seed % 3;
    let kill_at = (seed / 3) % 5;
    let stopping = spawn(async move {
        sleep(ms(stop_at)).await;
        stopper.stop_with_reason(ExitReason::Shutdown);
    });
    let l = log.clone();
    let killing = spawn(async move {
        sleep(ms(kill_at)).await;
        push(&l, "kill");
        killer.kill();
    });
    let reason = handle.wait_exit_async().await;
    stopping.await.unwrap();
    killing.await.unwrap();
    let log = entries(&log);
    let kill = position(&log, "kill").unwrap();
    let begin = log.iter().position(|e| e.starts_with("stopped:begin"));
    let end = position(&log, "stopped:end");
    assert_eq!(
        begin.is_some(),
        end.is_some(),
        "a begun stopped() finishes: {log:?}"
    );
    assert!(
        begin.is_none_or(|b| b < kill),
        "stopped() began after the kill: {log:?}"
    );
    outcome(match (&reason, begin) {
        (ExitReason::Killed, None) => "kill: before stopped()",
        (ExitReason::Killed, Some(_)) => "kill: during stopped()",
        _ => "kill: after D",
    });
    match reason {
        ExitReason::Killed => {}
        ExitReason::Shutdown => {
            assert!(
                end.is_some_and(|e| e < kill),
                "the kill came before D but did not win: {log:?}"
            );
        }
        other => panic!("unexpected reason {other:?}: {log:?}"),
    }
    assert_eq!(
        downs(&watcher, &watched).await,
        vec![format!("down:{reason:?}")]
    );
    stop(&watcher).await;
}

/// Decision 8: an untrapped signal racing D after a `Normal` stop replaces
/// the reason until D (the running `stopped()` still finishes); after D it
/// is ignored. A trapping link and a monitor see the same final reason.
async fn late_signal_vs_death(seed: u64) {
    let (log, observed) = (Log::default(), Log::default());
    let mut node = Node::new(&log, false);
    node.slow_stop = 2;
    let target = node.start();
    let observer = Node::new(&observed, true).start();
    let handle = target.child_handle();
    let h = handle.clone();
    on(&observer, move |ctx| {
        ctx.link(&h);
        ctx.monitor(&h);
    })
    .await;
    handle.stop();
    let (signaller, l) = (handle.clone(), log.clone());
    let at = seed % 5;
    let signalling = spawn(async move {
        sleep(ms(at)).await;
        push(&l, "signal");
        signaller.exit(ExitReason::Shutdown);
    });
    let reason = handle.wait_exit_async().await;
    signalling.await.unwrap();
    let log = entries(&log);
    let signal = position(&log, "signal").unwrap();
    let end = position(&log, "stopped:end").expect("a self-exit runs stopped()");
    outcome(format!("late signal: {reason:?}"));
    match reason {
        ExitReason::Shutdown => {}
        ExitReason::Normal => assert!(end < signal, "a signal before D was lost: {log:?}"),
        other => panic!("unexpected reason {other:?}: {log:?}"),
    }
    let downs = downs(&observer, &observed).await;
    assert_eq!(
        notices(&entries(&observed), "exit:"),
        vec![format!("exit:{reason:?}")]
    );
    assert_eq!(downs, vec![format!("down:{reason:?}")]);
    stop(&observer).await;
}

/// E4/E5: `link` + `monitor` (even seeds) or `try_link` (odd seeds) racing
/// the target's D. Each registration gets exactly one notice, the real
/// reason before D or `NoProc` after it; registrations made in one turn
/// agree; `try_link` after D gets `Err(NoProc)` and no notice.
async fn link_vs_death(seed: u64) {
    let observed = Log::default();
    let target = Node::new(&Log::default(), false).start();
    let observer = Node::new(&observed, true).start();
    let handle = target.child_handle();
    let stopper = handle.clone();
    let stop_at = seed % 3;
    let stopping = spawn(async move {
        sleep(ms(stop_at)).await;
        stopper.stop_with_reason(ExitReason::Shutdown);
    });
    sleep(ms((seed / 3) % 3)).await;
    let use_try = seed % 2 == 1;
    let result = Arc::new(Mutex::new(None));
    let (h, r) = (handle.clone(), result.clone());
    on(&observer, move |ctx| {
        if use_try {
            *r.lock().unwrap() = Some(ctx.try_link(&h).is_ok());
        } else {
            ctx.link(&h);
            ctx.monitor(&h);
        }
    })
    .await;
    stopping.await.unwrap();
    assert_eq!(handle.wait_exit_async().await, ExitReason::Shutdown);
    let downs = if use_try {
        settle(&observer).await;
        Vec::new()
    } else {
        downs(&observer, &observed).await
    };
    let observed = entries(&observed);
    let exits = notices(&observed, "exit:");
    outcome(format!(
        "link: try={use_try} {:?}",
        exits.first().map(String::as_str)
    ));
    if use_try {
        let linked = result.lock().unwrap().unwrap();
        let expected = if linked {
            vec!["exit:Shutdown".to_string()]
        } else {
            vec![]
        };
        assert_eq!(exits, expected, "try_link linked={linked}");
    } else {
        assert_eq!(exits.len(), 1, "{observed:?}");
        assert!(["exit:Shutdown", "exit:NoProc"].contains(&exits[0].as_str()));
        let down = exits[0].replace("exit:", "down:");
        assert_eq!(downs, vec![down], "link and monitor in one turn disagree");
    }
    stop(&observer).await;
}

/// E6: `unlink` + `demonitor` racing the target's death and delivery. A
/// marker sent to itself right after the fence turn is behind any notice
/// enqueued before it; no `Exit` or `Down` of that link or monitor is handled
/// after the marker, and at most one of each before it.
async fn unlink_demonitor_fence(seed: u64) {
    struct Marker;
    impl Message for Marker {
        type Result = ();
    }
    impl Handler<Marker> for Node {
        async fn handle(&mut self, _: Marker, reply: PendingReply<'_, ()>, _: &Context<Self>) {
            push(&self.log, "marker");
            reply.reply(());
        }
    }
    let observed = Log::default();
    let target = Node::new(&Log::default(), false).start();
    let observer = Node::new(&observed, true).start();
    let handle = target.child_handle();
    let monitor: Arc<Mutex<Option<MonitorRef>>> = Arc::default();
    let (h, m) = (handle.clone(), monitor.clone());
    on(&observer, move |ctx| {
        ctx.link(&h);
        *m.lock().unwrap() = Some(ctx.monitor(&h));
    })
    .await;
    let stopper = handle.clone();
    let stop_at = seed % 3;
    let stopping = spawn(async move {
        sleep(ms(stop_at)).await;
        stopper.stop_with_reason(ExitReason::Panic("gone".into()));
    });
    sleep(ms((seed / 3) % 3)).await;
    let (h, m) = (handle.clone(), monitor.clone());
    on(&observer, move |ctx| {
        ctx.unlink(&h);
        ctx.demonitor(m.lock().unwrap().unwrap());
        ctx.send(Marker).unwrap();
    })
    .await;
    stopping.await.unwrap();
    handle.wait_exit_async().await;
    settle(&observer).await;
    let observed = entries(&observed);
    let marker = position(&observed, "marker").unwrap();
    assert!(
        observed[marker + 1..]
            .iter()
            .all(|e| !e.starts_with("exit:") && !e.starts_with("down:")),
        "a notice after unlink/demonitor returned: {observed:?}"
    );
    let (exits, downs) = (notices(&observed, "exit:"), notices(&observed, "down:"));
    assert!(exits.len() <= 1 && downs.len() <= 1, "{observed:?}");
    outcome(format!("fence: exit={} down={}", exits.len(), downs.len()));
    stop(&observer).await;
}

/// E3: a supervisor's shutdown (the parent's exit signal) racing a trapping
/// child's queued messages. Messages queued before the parent exit are
/// handled first and in order, `exit_pending` turns true once and stays
/// true, `exit_received` never sees the parent exit, `stopped()` runs and
/// sees it pending, and the child ends `Shutdown`.
async fn supervisor_shutdown_vs_queue(seed: u64) {
    let log = Log::default();
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("sim-e3", 1).unwrap();
    let mut tree = system.supervisor(SupervisorConfig::default()).unwrap();
    let l = log.clone();
    let child = tree
        .add(
            ChildSpec::new("trapping", &group, move |_| {
                let log = l.clone();
                async move { Ok::<_, String>(Node::new(&log, true)) }
            })
            .shutdown_timeout(Duration::from_secs(5)),
        )
        .unwrap();
    let supervisor = tree.start().await.unwrap();
    let actor = child.current().unwrap();
    let handle = actor.child_handle();
    let gap = seed % 2;
    let sending = spawn(async move {
        for i in 0..4 {
            let _ = actor.send(Probe(i));
            sleep(ms(gap)).await;
        }
    });
    sleep(ms((seed / 2) % 4)).await;
    assert!(supervisor.shutdown(Duration::from_secs(10)).await.complete);
    sending.await.unwrap();
    assert_eq!(handle.wait_exit_async().await, ExitReason::Shutdown);
    let log = entries(&log);
    assert!(
        notices(&log, "exit:").is_empty(),
        "exit_received saw the parent exit: {log:?}"
    );
    assert_eq!(
        log.last().map(String::as_str),
        Some("stopped:end"),
        "{log:?}"
    );
    assert!(log.contains(&"stopped:begin:true".to_string()), "{log:?}");
    let probes: Vec<(u32, bool)> = log
        .iter()
        .filter_map(|e| e.strip_prefix("probe:"))
        .map(|p| {
            let (i, pending) = p.split_once(':').unwrap();
            (i.parse().unwrap(), pending == "true")
        })
        .collect();
    assert!(
        probes.windows(2).all(|w| w[0].0 < w[1].0),
        "out of order: {log:?}"
    );
    assert!(
        probes.windows(2).all(|w| w[0].1 <= w[1].1),
        "exit_pending went back to false: {log:?}"
    );
    outcome(format!(
        "e3: handled={} pending={}",
        probes.len(),
        probes.iter().filter(|p| p.1).count()
    ));
    assert!(system.shutdown(Duration::from_secs(10)).await.complete);
}

/// S1: a ready child killed while the tree's initial start is still running
/// (a slow sibling). Its exit is deferred until the tree commits ready, then
/// handled under OneForOne: `start()` succeeds, the child restarts once
/// and only after the sibling's `started()`, and the sibling is untouched.
async fn deferred_exit_vs_commit(seed: u64) {
    let log = Log::default();
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("sim-s1", 2).unwrap();
    let config = SupervisorConfig::default().strategy(SupervisorStrategy::OneForOne);
    let mut tree = system.supervisor(config).unwrap();
    let (l, starts) = (log.clone(), Arc::new(Mutex::new(0u32)));
    let first = tree
        .add(ChildSpec::new("first", &group, move |_| {
            let mut n = starts.lock().unwrap();
            *n += 1;
            push(&l, format!("first:start:{n}"));
            let log = Log::default();
            async move { Ok::<_, String>(Node::new(&log, false)) }
        }))
        .unwrap();
    let (l, delay) = (log.clone(), 1 + (seed / 4) % 4);
    let second = tree
        .add(ChildSpec::new("second", &group, move |_| {
            let l = l.clone();
            async move {
                sleep(ms(delay)).await;
                push(&l, "second:ready");
                Ok::<_, String>(Node::new(&Log::default(), false))
            }
        }))
        .unwrap();
    let kill_at = seed % 4;
    let (f, l) = (first.clone(), log.clone());
    let killing = spawn(async move {
        let actor = f.wait_ready().await.unwrap();
        sleep(ms(kill_at)).await;
        push(&l, "kill");
        actor.child_handle().kill();
    });
    let supervisor = tree
        .start()
        .await
        .expect("a ready child's exit failed start()");
    killing.await.unwrap();
    first.wait_for_generation(1).await.unwrap();
    let log = entries(&log);
    let restart = position(&log, "first:start:2").unwrap();
    let ready = position(&log, "second:ready").unwrap();
    let kill = position(&log, "kill").unwrap();
    outcome(if kill < ready {
        "s1: exit during startup (deferred)"
    } else {
        "s1: exit after the commit"
    });
    assert!(
        ready < restart,
        "restarted before the tree committed: {log:?}"
    );
    assert!(position(&log, "first:start:3").is_none(), "{log:?}");
    assert_eq!(second.snapshot().generation, 1);
    assert!(supervisor.shutdown(Duration::from_secs(10)).await.complete);
    assert!(system.shutdown(Duration::from_secs(10)).await.complete);
}

// ---------------------------------------------------------------------------
// Driver
// ---------------------------------------------------------------------------

/// PCT draws a new seed per run; DFS keeps one input seed per search, so it
/// is repeated over the first `DFS_SEEDS` seeds.
const PCT: (&str, usize) = ("pct:d=3", 1000);
const DFS: (&str, usize) = ("dfs:bound=3,depth=300", 300);
const DFS_SEEDS: u64 = 12;

fn check<F, Fut>(name: &str, body: F)
where
    F: Fn(u64) -> Fut + Sync,
    Fut: Future<Output = ()>,
{
    outcomes().remove(&test_name());
    let report = explore(&Strategy::parse(PCT.0).unwrap(), 0, PCT.1, &body);
    assert!(
        report.found.is_none(),
        "{name} under {}: {:?}",
        PCT.0,
        report.found
    );
    println!(
        "{name} {}: {} runs, {} schedules",
        PCT.0, report.runs, report.schedules
    );
    let (mut runs, mut exhausted) = (0, 0);
    for seed in 0..DFS_SEEDS {
        let report = explore(&Strategy::parse(DFS.0).unwrap(), seed, DFS.1, &body);
        assert!(
            report.found.is_none(),
            "{name} under {} seed {seed}: {:?}",
            DFS.0,
            report.found
        );
        runs += report.runs;
        exhausted += usize::from(report.complete());
    }
    println!(
        "{name} {}: {runs} runs over {DFS_SEEDS} seeds, {exhausted} searches exhausted",
        DFS.0
    );
    let outcomes = outcomes().remove(&test_name()).unwrap_or_default();
    assert!(!outcomes.is_empty(), "{name}: no outcome recorded");
    println!("{name} outcomes: {outcomes:?}");
}

#[test]
fn kill_races_stopped() {
    check("kill vs stopped", kill_vs_stopped);
}

#[test]
fn late_signal_races_the_death_point() {
    check("late signal vs D", late_signal_vs_death);
}

#[test]
fn link_and_monitor_race_the_death_point() {
    check("link vs D", link_vs_death);
}

#[test]
fn unlink_and_demonitor_fence_delivery() {
    check("unlink/demonitor fence", unlink_demonitor_fence);
}

#[test]
fn supervisor_shutdown_races_a_trapping_childs_queue() {
    check("supervisor shutdown vs queue", supervisor_shutdown_vs_queue);
}

#[test]
fn ready_child_exit_races_the_tree_commit() {
    check("deferred exit vs commit", deferred_exit_vs_commit);
}
