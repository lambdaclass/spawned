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

/// A payload whose destructor blocks until released (or `LIMIT`), and
/// reports that it started.
struct Gated {
    started: Option<mpsc::Sender<()>>,
    release: Option<mpsc::Receiver<()>>,
}
impl Drop for Gated {
    fn drop(&mut self) {
        if let (Some(started), Some(release)) = (self.started.take(), self.release.take()) {
            let _ = started.send(());
            let _ = release.recv_timeout(LIMIT);
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
