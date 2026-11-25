//! Targeted start races (unified start design 16.3): each pins one
//! interleaving of the startup decision with an outside event, by explicit
//! waits (gates the actors open and close) or a failpoint, never by timing.
//! `sim_test`s also run under the simulator (`--cfg spawned_sim`), where the
//! schedule explorer varies the remaining interleavings.
#![cfg(feature = "async")]
#![cfg_attr(spawned_sim, allow(unused_imports, dead_code))]

use spawned_concurrency::{
    message::Message,
    system::ActorSystem,
    tasks::{Actor, ActorStart, Context, Handler, StartOptions},
    ExitReason, PendingReply, StartError,
};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::oneshot;

const LIMIT: Duration = Duration::from_secs(10);

struct Ping;
impl Message for Ping {
    type Result = ();
}

/// A child whose `started()` reports that it began, then waits on a gate.
struct Gated {
    entered: Option<oneshot::Sender<()>>,
    gate: Option<oneshot::Receiver<()>>,
    trap: bool,
    started: Arc<AtomicUsize>,
}

impl Gated {
    fn new(trap: bool) -> (Self, oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (entered_tx, entered) = oneshot::channel();
        let (release, gate) = oneshot::channel();
        let child = Self {
            entered: Some(entered_tx),
            gate: Some(gate),
            trap,
            started: Arc::default(),
        };
        (child, entered, release)
    }
}

impl Actor for Gated {
    async fn started(&mut self, ctx: &Context<Self>) {
        self.started.fetch_add(1, Ordering::SeqCst);
        if self.trap {
            ctx.trap_exit(true);
        }
        if let Some(entered) = self.entered.take() {
            let _ = entered.send(());
        }
        if let Some(gate) = self.gate.take() {
            let _ = gate.await;
        }
    }
}

impl Handler<Ping> for Gated {
    async fn handle(&mut self, _: Ping, reply: PendingReply<'_, ()>, _: &Context<Self>) {
        reply.reply(());
    }
}

/// Planted bug 33: a kill outranks a held untrapped signal (design 2.3
/// rows 2 and 3): a non-trapping child that gets an exit signal and then a
/// kill while `started()` runs fails its start `Killed`.
#[spawned_macros::sim_test]
async fn kill_outranks_a_held_signal_during_started() {
    let (child, entered, release) = Gated::new(false);
    let starting = child.start();
    let handle = starting.actor_ref().unwrap().child_handle();
    entered.await.unwrap();
    handle.exit(ExitReason::Shutdown(None));
    handle.kill();
    release.send(()).unwrap();
    let result = starting.await;
    assert!(
        matches!(
            result,
            Err(StartError::InitExited {
                reason: ExitReason::Killed,
                pending: None
            })
        ),
        "{result:?}"
    );
    assert_eq!(handle.wait_exit_async().await, ExitReason::Killed);
}

/// A parent that begins its own exit, then tries a `start_link` in the same
/// turn.
struct StopsThenLinks {
    child_started: Arc<AtomicUsize>,
}
impl Actor for StopsThenLinks {}

struct StopThenLink;
impl Message for StopThenLink {
    type Result = Result<(), StartError>;
}
impl Handler<StopThenLink> for StopsThenLinks {
    async fn handle(
        &mut self,
        _: StopThenLink,
        reply: PendingReply<'_, Result<(), StartError>>,
        ctx: &Context<Self>,
    ) {
        ctx.stop();
        let (mut child, _entered, _release) = Gated::new(false);
        child.started = self.child_started.clone();
        reply.reply(child.start_link(ctx).await.map(|_| ()));
    }
}

/// Planted bug 32: a parent whose exit has begun (its own `ctx.stop()`,
/// still in its turn: requested, not dead) refuses `start_link` at stage 1
/// (`ParentExited`); no child runs.
#[spawned_macros::sim_test]
async fn start_link_through_an_exiting_parent_is_refused() {
    let child_started = Arc::new(AtomicUsize::new(0));
    let parent = StopsThenLinks {
        child_started: child_started.clone(),
    }
    .start()
    .await
    .unwrap();
    let result = parent.request(StopThenLink).await.unwrap();
    assert!(
        matches!(result, Err(StartError::ParentExited)),
        "{result:?}"
    );
    parent.wait_exit().await;
    assert_eq!(child_started.load(Ordering::SeqCst), 0);
}

/// A root whose `stopped()` reports and then waits on a gate.
struct SlowStop {
    entered: Mutex<Option<oneshot::Sender<()>>>,
    gate: Mutex<Option<oneshot::Receiver<()>>>,
}
impl Actor for SlowStop {
    async fn started(&mut self, ctx: &Context<Self>) {
        // Trapping: the system's Shutdown runs stopped() (E3).
        ctx.trap_exit(true);
    }
    async fn stopped(&mut self, _: &Context<Self>) {
        let entered = self.entered.lock().unwrap().take();
        if let Some(entered) = entered {
            let _ = entered.send(());
        }
        let gate = self.gate.lock().unwrap().take();
        if let Some(gate) = gate {
            let _ = gate.await;
        }
    }
}

/// Planted bug 34: row 9. The group's admission closes (system shutdown
/// began) while a managed child is in `started()` and before its own
/// Shutdown reaches it (the system stops roots newest first; a newer root's
/// slow `stopped()` holds the coordinator): the child's ready return
/// commits nothing and its start fails `Shutdown`.
#[spawned_macros::sim_test]
async fn ready_after_the_group_closed_fails_the_start() {
    let system = Arc::new(ActorSystem::new().unwrap());
    let group = system.async_group("start-races-closing", 4).unwrap();
    let (child, child_entered, release_child) = Gated::new(true);
    let starting = child.with_options(&group).start();
    child_entered.await.unwrap();
    let (stop_entered_tx, stop_entered) = oneshot::channel();
    let (release_stop, stop_gate) = oneshot::channel();
    let newer = SlowStop {
        entered: Mutex::new(Some(stop_entered_tx)),
        gate: Mutex::new(Some(stop_gate)),
    }
    .with_options(&group)
    .start()
    .await
    .unwrap();
    let shutdown = {
        let system = system.clone();
        spawned_rt::tasks::spawn(async move { system.shutdown(LIMIT).await.complete })
    };
    // The newer root is in stopped(): admission is closed, the child is not
    // signalled yet.
    stop_entered.await.unwrap();
    release_child.send(()).unwrap();
    let result = starting.await;
    assert!(
        matches!(
            result,
            Err(StartError::InitExited {
                reason: ExitReason::Shutdown(None),
                pending: None
            })
        ),
        "{result:?}"
    );
    release_stop.send(()).unwrap();
    newer.wait_exit().await;
    assert!(shutdown.await.unwrap());
}

/// A child whose state drop is slow and observable.
struct SlowDrop {
    dropped: Arc<AtomicBool>,
}
impl Actor for SlowDrop {
    async fn started(&mut self, _: &Context<Self>) -> Result<(), String> {
        Err("no config".into())
    }
}
impl Drop for SlowDrop {
    fn drop(&mut self) {
        std::thread::sleep(Duration::from_millis(50));
        self.dropped.store(true, Ordering::SeqCst);
    }
}

/// Planted bug 28: a failed start is answered only after the child's
/// publication: its state is already dropped (design 5.5,
/// `FailureAfterPublication`). Real threads: the waiter runs on another
/// worker while the child's slow destructor runs (on one thread the drop
/// always finishes before the waiter is polled).
#[cfg(not(spawned_sim))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_start_answers_after_the_state_drop() {
    let dropped = Arc::new(AtomicBool::new(false));
    let result = SlowDrop {
        dropped: dropped.clone(),
    }
    .start()
    .await;
    assert!(
        matches!(result, Err(StartError::InitFailed { pending: None, .. })),
        "{result:?}"
    );
    assert!(
        dropped.load(Ordering::SeqCst),
        "answered before the state drop"
    );
}

/// A child whose `started()` takes exactly `after`.
struct TakesExactly(Duration);
impl Actor for TakesExactly {
    async fn started(&mut self, _: &Context<Self>) {
        spawned_rt::tasks::sleep(self.0).await;
    }
}
impl Handler<Ping> for TakesExactly {
    async fn handle(&mut self, _: Ping, reply: PendingReply<'_, ()>, _: &Context<Self>) {
        reply.reply(());
    }
}

/// Planted bug 25 (simulator race): `started()` returns at the init
/// timer's own instant, so the schedule explorer orders the ready commit
/// and the timer's expiry both ways. Whichever wins, a child answered
/// `Started` is never killed by the timer (`TimeoutNeverKillsReady`).
#[spawned_macros::sim_test]
async fn the_init_timer_never_kills_a_ready_child() {
    let after = Duration::from_millis(50);
    let result = TakesExactly(after)
        .with_options(StartOptions::new().init_timeout(after))
        .start()
        .await;
    match result {
        Ok(actor) => {
            spawned_rt::tasks::sleep(Duration::from_millis(20)).await;
            assert!(
                actor.request(Ping).await.is_ok(),
                "a child answered Started was killed by the init timer"
            );
            actor.child_handle().stop();
            actor.wait_exit().await;
        }
        Err(StartError::InitTimeout { .. }) => {}
        other => panic!("{other:?}"),
    }
}

/// Planted bug 25 (failpoint): the timer fires before the ready commit but
/// reaches the cell after it (`start.init_timer_fire` delays it). The ready
/// commitment stands; the timer loses and kills nothing.
#[cfg(all(feature = "failpoints", not(spawned_sim)))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_timer_reaching_the_cell_after_ready_loses() {
    use spawned_concurrency::failpoints::{self, Action, FailPoint, FailScenario};
    let _scenario = FailScenario::setup();
    failpoints::configure(
        "start.init_timer_fire",
        FailPoint::new(Action::Delay(Duration::from_millis(400))).times(1),
    )
    .unwrap();
    let actor = TakesExactly(Duration::from_millis(100))
        .with_options(StartOptions::new().init_timeout(Duration::from_millis(20)))
        .start()
        .await
        .unwrap();
    // Until the delayed timer has reached the cell.
    let deadline = std::time::Instant::now() + LIMIT;
    while failpoints::stats("start.init_timer_fire").fired < 1 {
        assert!(std::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(
        actor.request(Ping).await.is_ok(),
        "the late timer killed a ready child"
    );
}
