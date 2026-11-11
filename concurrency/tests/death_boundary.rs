//! The death point D (docs/design/semantics.md, `Lifecycle!NoOrphanRequest`):
//! every queued request and every deferred reply is answered with the final
//! reason at D, before any payload or state destructor runs and whatever
//! drives the death (the run loop, or the runtime dropping the actor). Each
//! test blocks a destructor and checks that the callers are answered while
//! it is still blocked.
#![cfg(not(spawned_sim))] // blocking destructors and OS threads
use spawned_concurrency::{message::Message, ActorError, ExitReason};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const LIMIT: Duration = Duration::from_secs(5);

/// A payload whose destructor blocks until released, and reports that it
/// started. It never releases itself within an assertion window (`LIMIT`):
/// a test that answers only after destruction fails, never passes late.
struct Gated {
    started: Option<mpsc::Sender<()>>,
    release: Option<mpsc::Receiver<()>>,
}
impl Drop for Gated {
    fn drop(&mut self) {
        if let (Some(started), Some(release)) = (self.started.take(), self.release.take()) {
            let _ = started.send(());
            let _ = release.recv_timeout(LIMIT * 6);
        }
    }
}
/// A gated payload, its "destructor started" signal and its release.
fn gated() -> (Gated, mpsc::Receiver<()>, mpsc::Sender<()>) {
    let (started_tx, started) = mpsc::channel();
    let (release, release_rx) = mpsc::channel();
    (
        Gated {
            started: Some(started_tx),
            release: Some(release_rx),
        },
        started,
        release,
    )
}
fn inert() -> Gated {
    Gated {
        started: None,
        release: None,
    }
}

/// A request carrying a payload.
struct Queued(#[allow(dead_code)] Gated);
impl Message for Queued {
    type Result = ();
}
/// Blocks the handler until released.
struct Block(mpsc::Receiver<()>, mpsc::Sender<()>);
impl Message for Block {
    type Result = ();
}

/// A payload whose destructor waits (at most 3 s) for a queued caller's
/// answer, and records whether it arrived: destroyed before the death point
/// answers that caller, it waits in vain.
struct WaitsForAnswer {
    caller: std::sync::Arc<std::sync::Mutex<Option<spawned_concurrency::ReplyReceiver<()>>>>,
    answered: std::sync::Arc<std::sync::atomic::AtomicBool>,
}
impl Drop for WaitsForAnswer {
    fn drop(&mut self) {
        let began = Instant::now();
        while began.elapsed() < Duration::from_secs(3) {
            let mut caller = self.caller.lock().unwrap();
            if let Some(receiver) = caller.as_mut() {
                if receiver.try_recv().is_some() {
                    self.answered
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                    return;
                }
            }
            drop(caller);
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}
/// A job's message carrying such a payload.
struct Waiting(#[allow(dead_code)] WaitsForAnswer);
impl Message for Waiting {
    type Result = ();
}

/// Wait until `poll` yields, at most `LIMIT`.
fn eventually<T>(what: &str, mut poll: impl FnMut() -> Option<T>) -> T {
    let began = Instant::now();
    loop {
        if let Some(value) = poll() {
            return value;
        }
        assert!(began.elapsed() < LIMIT, "{what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[cfg(feature = "async")]
mod tasks_mode {
    use super::*;
    use spawned_concurrency::tasks::{Actor, ActorStart, Context, Handler};
    use spawned_concurrency::{PendingReply, ReplyTo};
    use std::future::Future;
    use std::sync::Arc;

    /// Defers and keeps the token in the actor's state.
    struct Park;
    impl Message for Park {
        type Result = ();
    }

    struct Owner {
        _state: Gated,
        parked: Vec<ReplyTo<()>>,
    }
    impl Actor for Owner {}
    impl Handler<Queued> for Owner {
        async fn handle(&mut self, _: Queued, reply: PendingReply<'_, ()>, _: &Context<Self>) {
            reply.reply(());
        }
    }
    impl Handler<Block> for Owner {
        async fn handle(&mut self, msg: Block, reply: PendingReply<'_, ()>, _: &Context<Self>) {
            let _ = msg.1.send(());
            let _ = msg.0.recv_timeout(LIMIT);
            reply.reply(());
        }
    }
    impl Handler<Park> for Owner {
        async fn handle(&mut self, _: Park, reply: PendingReply<'_, ()>, _: &Context<Self>) {
            self.parked.push(reply.defer().unwrap());
        }
    }

    /// Defers and hands the token out.
    struct HandOut(mpsc::Sender<ReplyTo<()>>);
    impl Message for HandOut {
        type Result = ();
    }
    impl Handler<HandOut> for Owner {
        async fn handle(&mut self, msg: HandOut, reply: PendingReply<'_, ()>, _: &Context<Self>) {
            msg.0.send(reply.defer().unwrap()).unwrap();
        }
    }

    impl Handler<Waiting> for Owner {
        async fn handle(&mut self, _: Waiting, _: PendingReply<'_, ()>, _: &Context<Self>) {}
    }

    /// Review round 2 (Codex 1): the owner's exit cancels its jobs without
    /// destroying their payloads; a scheduled emit's message and a held
    /// result are destroyed after the death point answered every caller.
    /// Cancellation used to destroy them first: a destructor waiting for a
    /// queued caller's answer waited for an answer that could not come.
    #[test]
    fn job_payloads_are_destroyed_after_every_caller_is_answered() {
        use spawned_concurrency::tasks::JobOptions;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Mutex;
        for held in [false, true] {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap();
            let _guard = runtime.enter();
            let actor = Owner {
                _state: inert(),
                parked: Vec::new(),
            }
            .start();
            let caller = Arc::new(Mutex::new(None));
            let answered = Arc::new(AtomicBool::new(false));
            let payload = WaitsForAnswer {
                caller: caller.clone(),
                answered: answered.clone(),
            };
            let (scheduled_tx, scheduled) = mpsc::channel();
            runtime.block_on(async {
                actor
                    .__in_turn(move |_, ctx| {
                        ctx.pipe_with_ctx(
                            JobOptions::default(),
                            move |job| async move {
                                let far = spawned_concurrency::Instant::now() + LIMIT * 10;
                                if held {
                                    // The result waits for this emit: held.
                                    job.emit_at(far, Queued(inert())).await.unwrap();
                                    scheduled_tx.send(()).unwrap();
                                    Some(payload)
                                } else {
                                    job.emit_at(far, Waiting(payload)).await.unwrap();
                                    scheduled_tx.send(()).unwrap();
                                    job.cancelled().await;
                                    None
                                }
                            },
                            |_| Queued(inert()),
                        )
                        .unwrap();
                    })
                    .unwrap()
                    .await
                    .unwrap();
            });
            scheduled.recv_timeout(LIMIT).unwrap();
            let (unblock, blocked) = mpsc::channel();
            let (entered_tx, entered) = mpsc::channel();
            actor.send(Block(blocked, entered_tx)).unwrap();
            entered.recv_timeout(LIMIT).unwrap();
            let queued = actor
                .request_raw_until(Queued(inert()), spawned_concurrency::Deadline::Never)
                .unwrap();
            *caller.lock().unwrap() = Some(queued);
            actor.context().stop();
            unblock.send(()).unwrap();
            runtime.block_on(actor.join());
            assert!(
                answered.load(Ordering::SeqCst),
                "a job payload (held: {held}) was destroyed before the queued caller was answered"
            );
        }
    }

    /// Holds its message (and its destructor) across a pending await.
    struct Hang(#[allow(dead_code)] Gated, mpsc::Sender<()>);
    impl Message for Hang {
        type Result = ();
    }
    impl Handler<Hang> for Owner {
        async fn handle(&mut self, msg: Hang, _: PendingReply<'_, ()>, _: &Context<Self>) {
            let msg = msg;
            let _ = msg.1.send(());
            std::future::pending::<()>().await;
            drop(msg);
        }
    }

    /// Review round 2 (Codex 2, agy 1): a runtime that drops an actor while
    /// a turn is pending destroys the turn (its message, the handler's
    /// locals) only after the death point answered the turn's own caller and
    /// every queued one. The pending lifecycle future used to be destroyed
    /// first, so a blocking destructor in it left every caller waiting.
    #[test]
    fn a_dropped_pending_turn_is_destroyed_after_every_caller_is_answered() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (payload, destroying, release) = gated();
        let (entered_tx, entered) = mpsc::channel();
        let never = spawned_concurrency::Deadline::Never;
        let (actor, mut turn_caller, mut queued) = runtime.block_on(async {
            let actor = Owner {
                _state: inert(),
                parked: Vec::new(),
            }
            .start();
            let turn_caller = actor
                .request_raw_until(Hang(payload, entered_tx), never)
                .unwrap();
            let queued = actor.request_raw_until(Queued(inert()), never).unwrap();
            while entered.try_recv().is_err() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            (actor, turn_caller, queued)
        });
        let dropper = std::thread::spawn(move || drop(runtime));
        // The pending turn's message is being destroyed (and blocks).
        destroying.recv_timeout(LIMIT).unwrap();
        eventually(
            "the dropped turn's caller waited for its destruction",
            || stopped(turn_caller.try_recv(), &ExitReason::Killed),
        );
        eventually("a queued caller waited for the turn's destruction", || {
            stopped(queued.try_recv(), &ExitReason::Killed)
        });
        release.send(()).unwrap();
        dropper.join().unwrap();
        assert_eq!(actor.exit_reason(), Some(ExitReason::Killed));
    }

    /// Review round 2 (Codex 5): the death point answers every caller before
    /// any wake-up runs. The first queued caller's waker waits for the second
    /// caller's answer; closing the first slot used to run that waker before
    /// the second slot was closed, so the second answer had to wait.
    #[test]
    fn every_queued_caller_is_answered_before_any_wake_runs() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Mutex;
        use std::task::{Wake, Waker};
        type Receiver = spawned_concurrency::ReplyReceiver<()>;
        struct WaitForSecond {
            second: Mutex<Receiver>,
            answered: AtomicBool,
        }
        impl Wake for WaitForSecond {
            fn wake(self: Arc<Self>) {
                let began = Instant::now();
                while began.elapsed() < Duration::from_secs(3) {
                    if self.second.lock().unwrap().try_recv().is_some() {
                        self.answered.store(true, Ordering::SeqCst);
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        }
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let _guard = runtime.enter();
        let actor = Owner {
            _state: inert(),
            parked: Vec::new(),
        }
        .start();
        let (unblock, blocked) = mpsc::channel();
        let (entered_tx, entered) = mpsc::channel();
        actor.send(Block(blocked, entered_tx)).unwrap();
        entered.recv_timeout(LIMIT).unwrap();
        let never = spawned_concurrency::Deadline::Never;
        let first = actor.request_raw_until(Queued(inert()), never).unwrap();
        let second = actor.request_raw_until(Queued(inert()), never).unwrap();
        let waiter = Arc::new(WaitForSecond {
            second: Mutex::new(second),
            answered: AtomicBool::new(false),
        });
        let waker = Waker::from(waiter.clone());
        let mut awaiting = Box::pin(std::future::IntoFuture::into_future(first));
        assert!(awaiting
            .as_mut()
            .poll(&mut std::task::Context::from_waker(&waker))
            .is_pending());
        actor.context().stop();
        unblock.send(()).unwrap();
        runtime.block_on(actor.join());
        assert!(
            waiter.answered.load(Ordering::SeqCst),
            "a wake-up ran before every queued caller was answered"
        );
    }

    /// Review round 2 (Codex 3): `ReplyTo::overloaded()` holds its
    /// death-fence permit only across the transition. Its caller's waker
    /// runs after: here it stops the owner and waits for the published
    /// exit. The permit used to cover the wake, so the owner's death waited
    /// for the waker and the waker for the death.
    #[test]
    fn overloaded_wakes_the_caller_after_releasing_the_death_fence() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::task::{Wake, Waker};
        struct StopAndWait {
            actor: spawned_concurrency::tasks::ActorRef<Owner>,
            exited: Arc<AtomicBool>,
        }
        impl Wake for StopAndWait {
            fn wake(self: Arc<Self>) {
                self.actor.context().stop();
                let began = Instant::now();
                while began.elapsed() < Duration::from_secs(3) {
                    if self.actor.exit_reason().is_some() {
                        self.exited.store(true, Ordering::SeqCst);
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        }
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let _guard = runtime.enter();
        let actor = Owner {
            _state: inert(),
            parked: Vec::new(),
        }
        .start();
        let (tokens, token) = mpsc::channel();
        let receiver = actor
            .request_raw_until(HandOut(tokens), spawned_concurrency::Deadline::Never)
            .unwrap();
        let token = token.recv_timeout(LIMIT).unwrap();
        let exited = Arc::new(AtomicBool::new(false));
        let waker = Waker::from(Arc::new(StopAndWait {
            actor: actor.clone(),
            exited: exited.clone(),
        }));
        let mut awaiting = Box::pin(std::future::IntoFuture::into_future(receiver));
        let mut cx = std::task::Context::from_waker(&waker);
        assert!(awaiting.as_mut().poll(&mut cx).is_pending());
        // The wake runs on this helper, inside `overloaded()`.
        std::thread::spawn(move || token.overloaded())
            .join()
            .unwrap();
        assert!(
            exited.load(Ordering::SeqCst),
            "the owner could not die while overloaded() held its fence permit"
        );
    }

    fn stopped<R>(result: Option<Result<R, ActorError>>, reason: &ExitReason) -> Option<()> {
        match result? {
            Err(ActorError::ActorStopped(r)) if r == *reason => Some(()),
            Err(other) => panic!("unexpected error {other}"),
            Ok(_) => panic!("a request that was never handled was answered Ok"),
        }
    }

    /// Review P0 (Codex 1): requests queued behind the exit are answered
    /// at D, before the first queued payload's destructor runs. The mailbox
    /// used to be dropped envelope by envelope, each payload before its
    /// request: a blocking destructor left its own caller and every later
    /// one unanswered while it ran.
    #[test]
    fn queued_requests_are_answered_before_payload_destructors() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let _guard = runtime.enter();
        let actor = Owner {
            _state: inert(),
            parked: Vec::new(),
        }
        .start();
        let (unblock, blocked) = mpsc::channel();
        let (entered_tx, entered) = mpsc::channel();
        actor.send(Block(blocked, entered_tx)).unwrap();
        entered.recv_timeout(LIMIT).unwrap();
        let (payload, destroying, release) = gated();
        let mut first = actor.request_raw(Queued(payload)).unwrap();
        let mut second = actor.request_raw(Queued(inert())).unwrap();
        actor.context().stop();
        unblock.send(()).unwrap();
        destroying.recv_timeout(LIMIT).unwrap();
        // The first queued payload's destructor is running (blocked).
        eventually("the queued callers were not answered at death", || {
            stopped(first.try_recv(), &ExitReason::Normal)
        });
        eventually(
            "a later queued caller waited for an earlier destructor",
            || stopped(second.try_recv(), &ExitReason::Normal),
        );
        release.send(()).unwrap();
        runtime.block_on(actor.join());
    }

    /// Review P1 (Codex 5): a never-polled actor dropped with its runtime
    /// reaches D (queued callers answered `Killed`) before its state and
    /// mailbox are destroyed. Its startup payload was destroyed first.
    #[test]
    fn a_never_polled_actor_answers_before_its_state_is_destroyed() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (state, destroying, release) = gated();
        // Spawned but never polled: the runtime is never driven.
        let actor = {
            let _guard = runtime.enter();
            Owner {
                _state: state,
                parked: Vec::new(),
            }
            .start()
        };
        let mut queued = actor.request_raw(Queued(inert())).unwrap();
        let dropper = std::thread::spawn(move || drop(runtime));
        destroying.recv_timeout(LIMIT).unwrap();
        eventually("a queued caller waited for the state destructor", || {
            stopped(queued.try_recv(), &ExitReason::Killed)
        });
        release.send(()).unwrap();
        dropper.join().unwrap();
        assert_eq!(actor.exit_reason(), Some(ExitReason::Killed));
    }

    /// Review P1 (Codex 5): a running actor dropped with its runtime reaches
    /// D (deferred and queued callers answered `Killed`) before its state is
    /// destroyed. The state was destroyed first, and the registry closed
    /// after publication.
    #[test]
    fn a_dropped_running_actor_answers_before_its_state_is_destroyed() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (state, destroying, release) = gated();
        let (actor, mut parked) = runtime.block_on(async {
            let actor = Owner {
                _state: state,
                parked: Vec::new(),
            }
            .start();
            let parked = actor.request_raw(Park).unwrap();
            // Let the actor defer it.
            actor.request(Queued(inert())).await.unwrap();
            (actor, parked)
        });
        let dropper = std::thread::spawn(move || drop(runtime));
        destroying.recv_timeout(LIMIT).unwrap();
        eventually("a deferred caller waited for the state destructor", || {
            stopped(parked.try_recv(), &ExitReason::Killed)
        });
        release.send(()).unwrap();
        dropper.join().unwrap();
        assert_eq!(actor.exit_reason(), Some(ExitReason::Killed));
    }
}

mod threads_mode {
    use super::*;
    use spawned_concurrency::threads::{Actor, ActorStart, Context, Handler};
    use spawned_concurrency::PendingReply;

    struct Owner;
    impl Actor for Owner {}
    impl Handler<Queued> for Owner {
        fn handle(&mut self, _: Queued, reply: PendingReply<'_, ()>, _: &Context<Self>) {
            reply.reply(());
        }
    }
    impl Handler<Block> for Owner {
        fn handle(&mut self, msg: Block, reply: PendingReply<'_, ()>, _: &Context<Self>) {
            let _ = msg.1.send(());
            let _ = msg.0.recv_timeout(LIMIT);
            reply.reply(());
        }
    }

    impl Handler<Waiting> for Owner {
        fn handle(&mut self, _: Waiting, _: PendingReply<'_, ()>, _: &Context<Self>) {}
    }

    /// Review round 2 (Codex 1), threads mode: a scheduled emit's message
    /// is destroyed after the death point answered every caller.
    #[test]
    fn job_payloads_are_destroyed_after_every_caller_is_answered() {
        use spawned_concurrency::threads::JobOptions;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Arc, Mutex};
        let actor = Owner.start();
        let caller = Arc::new(Mutex::new(None));
        let answered = Arc::new(AtomicBool::new(false));
        let payload = WaitsForAnswer {
            caller: caller.clone(),
            answered: answered.clone(),
        };
        let (scheduled_tx, scheduled) = mpsc::channel();
        spawned_test::threads::in_actor(&actor, move |_, ctx| {
            ctx.pipe_blocking_with(
                JobOptions::default(),
                move |job| {
                    let far = spawned_concurrency::Instant::now() + LIMIT * 10;
                    job.emit_at_blocking(far, Waiting(payload)).unwrap();
                    scheduled_tx.send(()).unwrap();
                    while !job.is_cancelled() {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                },
                |_| Queued(inert()),
            )
            .unwrap();
        });
        scheduled.recv_timeout(LIMIT).unwrap();
        let (unblock, blocked) = mpsc::channel();
        let (entered_tx, entered) = mpsc::channel();
        actor.send(Block(blocked, entered_tx)).unwrap();
        entered.recv_timeout(LIMIT).unwrap();
        let queued = actor
            .request_raw_until(Queued(inert()), spawned_concurrency::Deadline::Never)
            .unwrap();
        *caller.lock().unwrap() = Some(queued);
        actor.context().stop();
        unblock.send(()).unwrap();
        actor.join();
        assert!(
            answered.load(Ordering::SeqCst),
            "a job payload was destroyed before the queued caller was answered"
        );
    }

    /// Review P0 (Codex 1), threads mode.
    #[test]
    fn queued_requests_are_answered_before_payload_destructors() {
        let actor = Owner.start();
        let (unblock, blocked) = mpsc::channel();
        let (entered_tx, entered) = mpsc::channel();
        actor.send(Block(blocked, entered_tx)).unwrap();
        entered.recv_timeout(LIMIT).unwrap();
        let (payload, destroying, release) = gated();
        let mut first = actor.request_raw(Queued(payload)).unwrap();
        let mut second = actor.request_raw(Queued(inert())).unwrap();
        actor.context().stop();
        unblock.send(()).unwrap();
        destroying.recv_timeout(LIMIT).unwrap();
        let answered = |rx: &mut spawned_concurrency::ReplyReceiver<()>| match rx.try_recv()? {
            Err(ActorError::ActorStopped(ExitReason::Normal)) => Some(()),
            other => panic!("unexpected {other:?}"),
        };
        eventually("the queued callers were not answered at death", || {
            answered(&mut first)
        });
        eventually(
            "a later queued caller waited for an earlier destructor",
            || answered(&mut second),
        );
        release.send(()).unwrap();
        actor.join();
    }
}

#[cfg(feature = "async")]
#[path = "support/drop_subprocess.rs"]
mod subprocess;

/// Review round 2 (Codex 5): a runtime drop's "stopped() skipped" log (a
/// tracing subscriber is user code) runs after every caller is answered. It
/// used to run between the seal and the answers, so a subscriber waiting for
/// a queued caller blocked the death point. Global subscriber: own process.
#[cfg(feature = "async")]
mod subscriber_after_discharge {
    use super::subprocess;
    use super::*;
    use spawned_concurrency::tasks::{Actor, ActorStart, Context, Handler};
    use spawned_concurrency::{PendingReply, ReplyReceiver};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;

    static QUEUED: Mutex<Option<ReplyReceiver<()>>> = Mutex::new(None);
    static ANSWERED_WHEN_LOGGED: AtomicBool = AtomicBool::new(false);
    static LOGGED: AtomicBool = AtomicBool::new(false);

    /// Waits (bounded) in every event for the queued caller's answer.
    struct WaitingSubscriber;
    impl tracing::Subscriber for WaitingSubscriber {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, _: &tracing::Event<'_>) {
            let Some(mut queued) = QUEUED.lock().unwrap().take() else {
                return;
            };
            LOGGED.store(true, Ordering::SeqCst);
            let began = Instant::now();
            while began.elapsed() < Duration::from_secs(3) {
                if queued.try_recv().is_some() {
                    ANSWERED_WHEN_LOGGED.store(true, Ordering::SeqCst);
                    return;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    struct Idle;
    impl Actor for Idle {}
    impl Handler<Queued> for Idle {
        async fn handle(&mut self, _: Queued, reply: PendingReply<'_, ()>, _: &Context<Self>) {
            reply.reply(());
        }
    }

    #[test]
    fn the_skipped_stopped_log_runs_after_every_caller_is_answered() {
        subprocess::run(
            "subscriber_after_discharge::child",
            "skipped-stopped log ran after the answers",
        );
    }

    #[test]
    #[ignore = "subprocess helper exercised by the enabled parent"]
    fn child() {
        if std::env::var_os("SPAWNED_MAILBOX_DROP_CHILD").is_none() {
            return;
        }
        tracing::subscriber::set_global_default(WaitingSubscriber).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        // Never polled: dropping the runtime drops it before `stopped()`.
        let actor = {
            let _guard = runtime.enter();
            Idle.start()
        };
        let queued = actor
            .request_raw_until(Queued(inert()), spawned_concurrency::Deadline::Never)
            .unwrap();
        *QUEUED.lock().unwrap() = Some(queued);
        drop(runtime);
        assert!(LOGGED.load(Ordering::SeqCst), "nothing was logged");
        assert!(
            ANSWERED_WHEN_LOGGED.load(Ordering::SeqCst),
            "the log ran before the queued caller was answered"
        );
        println!("skipped-stopped log ran after the answers");
    }
}
