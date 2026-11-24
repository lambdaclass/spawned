//! The status turn (docs/design/introspection.md, "State and status"; I4,
//! I5): `get_state` / `get_status` (OTP `sys:get_state` / `sys:get_status`)
//! run in a turn of their own, between handler turns, in mailbox order, with
//! one outcome (`Ok`, `RequestTimeout`, `ActorStopped(final)`, `NoProc`,
//! `CallbackFailed`); a caught callback panic does not end the actor; a
//! request whose caller gave up before dispatch is not run; state is
//! redacted unless the actor opts in. Both modes.
#![cfg_attr(spawned_sim, allow(unused_imports, dead_code))]

use spawned_concurrency::introspection::{Inspect, Lifecycle, StateView};
use spawned_concurrency::message::Message;
use spawned_concurrency::{ActorError, ExitReason, PendingReply};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::Arc;
use std::time::Duration;

const LIMIT: Duration = Duration::from_secs(5);

struct Wedge {
    entered: std_mpsc::Sender<()>,
    release: std_mpsc::Receiver<()>,
}
impl Message for Wedge {
    type Result = ();
}
struct Bump;
impl Message for Bump {
    type Result = u64;
}
/// Defers its reply and keeps the token in a `RefCell` (interior
/// mutability a status callback can reach).
struct Defer;
impl Message for Defer {
    type Result = ();
}

fn wedge() -> (Wedge, std_mpsc::Receiver<()>, std_mpsc::Sender<()>) {
    let (entered_tx, entered) = std_mpsc::channel();
    let (release_tx, release) = std_mpsc::channel();
    (
        Wedge {
            entered: entered_tx,
            release,
        },
        entered,
        release_tx,
    )
}

#[cfg(all(feature = "async", not(spawned_sim)))]
mod tasks_mode {
    use super::*;
    use spawned_concurrency::introspection::MAX_STATUS_BYTES;
    use spawned_concurrency::tasks::{Actor, ActorStart, Context, Handler};
    use spawned_concurrency::{actor, ReplyTo};
    use std::cell::RefCell;

    #[derive(Default)]
    struct Counter {
        count: u64,
        token: RefCell<Option<ReplyTo<()>>>,
    }
    impl Actor for Counter {}
    impl Handler<Wedge> for Counter {
        async fn handle(&mut self, msg: Wedge, reply: PendingReply<'_, ()>, _: &Context<Self>) {
            self.count += 1;
            msg.entered.send(()).unwrap();
            let _ = msg.release.recv_timeout(LIMIT);
            reply.reply(());
        }
    }
    impl Handler<Bump> for Counter {
        async fn handle(&mut self, _: Bump, reply: PendingReply<'_, u64>, _: &Context<Self>) {
            self.count += 1;
            reply.reply(self.count);
        }
    }
    impl Handler<Defer> for Counter {
        async fn handle(&mut self, _: Defer, reply: PendingReply<'_, ()>, _: &Context<Self>) {
            *self.token.borrow_mut() = Some(reply.defer().unwrap());
        }
    }

    /// I4: queued behind the backlog, answered between turns: never during
    /// the wedged turn, before the message sent after it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn status_runs_between_turns_in_mailbox_order() {
        let actor = Counter::default().start().await.unwrap();
        let (msg, entered, release) = wedge();
        actor.send(msg).unwrap();
        entered.recv_timeout(LIMIT).unwrap();
        let state = actor.get_state(|c| c.count);
        actor.send(Bump).unwrap();
        let mut state = Box::pin(state);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut state)
                .await
                .is_err(),
            "answered during the wedged turn"
        );
        release.send(()).unwrap();
        assert_eq!(state.await.unwrap(), 1);
        assert_eq!(actor.get_state(|c| c.count).await.unwrap(), 2);
        let report = actor.get_status().await.unwrap();
        assert_eq!(report.info.lifecycle, Lifecycle::Idle);
        assert_eq!(report.info.id, actor.id());
        assert_eq!(report.state, StateView::Redacted);
        actor.child_handle().stop();
        actor.join().await;
    }

    /// I4: every outcome. `RequestTimeout` on a stuck actor; the abandoned
    /// request's callback never runs; `ActorStopped(final)` for one queued at
    /// the death point; `NoProc` after it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn status_outcomes_and_abandoned_requests_are_not_run() {
        let actor = Counter::default().start().await.unwrap();
        let (msg, entered, release) = wedge();
        actor.send(msg).unwrap();
        entered.recv_timeout(LIMIT).unwrap();
        let ran = Arc::new(AtomicBool::new(false));
        let seen = ran.clone();
        let late = actor
            .with_request_timeout(Duration::from_millis(30))
            .get_state(move |_| seen.store(true, Ordering::SeqCst))
            .await;
        assert!(matches!(late, Err(ActorError::RequestTimeout)), "{late:?}");
        release.send(()).unwrap();
        // The abandoned status turn is dispatched before this request.
        actor.request(Bump).await.unwrap();
        assert!(!ran.load(Ordering::SeqCst), "an abandoned callback ran");
        // Queued at the death point: answered with the final reason.
        let (msg, entered, release) = wedge();
        actor.send(msg).unwrap();
        entered.recv_timeout(LIMIT).unwrap();
        let queued = actor.with_request_timeout(LIMIT).get_status();
        actor.child_handle().kill();
        release.send(()).unwrap();
        let queued = queued.await;
        assert!(
            matches!(queued, Err(ActorError::ActorStopped(ExitReason::Killed))),
            "{queued:?}"
        );
        assert_eq!(actor.wait_exit().await, ExitReason::Killed);
        assert!(matches!(
            actor.get_state(|c| c.count).await,
            Err(ActorError::NoProc)
        ));
        assert!(matches!(
            actor.child_handle().get_status().await,
            Err(ActorError::NoProc)
        ));
    }

    /// I4: a callback panic answers `CallbackFailed`, the actor continues,
    /// and effects through interior mutability are not rolled back (the
    /// deferred token it took and dropped answers `ReplyDropped`).
    #[tokio::test]
    async fn callback_panic_is_callback_failed_and_actor_continues() {
        let actor = Counter::default().start().await.unwrap();
        let deferred = actor
            .send_request(Defer, spawned_concurrency::Deadline::Never)
            .unwrap();
        actor.request(Bump).await.unwrap();
        assert_eq!(actor.info().deferred_outstanding, 1);
        let failed = actor
            .get_state(|c| {
                drop(c.token.borrow_mut().take());
                panic!("callback boom");
            })
            .await;
        assert!(
            matches!(failed, Err(ActorError::CallbackFailed)),
            "{failed:?}"
        );
        assert!(matches!(deferred.await, Err(ActorError::ReplyDropped)));
        assert_eq!(actor.request(Bump).await.unwrap(), 2);
        assert_eq!(actor.info().lifecycle, Lifecycle::Idle);
        assert_eq!(actor.info().deferred_outstanding, 0);
        actor.child_handle().stop();
        assert_eq!(actor.wait_exit().await, ExitReason::Normal);
    }

    /// I4: a status turn takes no deferred-reply credit and leaves an
    /// outstanding token alone.
    #[tokio::test]
    async fn get_state_runs_while_deferred_reply_is_outstanding() {
        let actor = Counter::default().start().await.unwrap();
        let _deferred = actor
            .send_request(Defer, spawned_concurrency::Deadline::Never)
            .unwrap();
        actor.request(Bump).await.unwrap();
        let held = actor.get_state(|c| c.token.borrow().is_some()).await;
        assert!(held.unwrap());
        assert_eq!(actor.info().deferred_outstanding, 1);
        actor.child_handle().stop();
        actor.join().await;
    }

    /// I4: admitted during `stopped()`: answered `ActorStopped` at the death
    /// point; sent during destruction (after D): `NoProc`.
    struct SlowExit {
        stop: Option<tokio::sync::oneshot::Receiver<()>>,
        drop_gate: Option<std_mpsc::Receiver<()>>,
    }
    impl Actor for SlowExit {
        async fn stopped(&mut self, _: &Context<Self>) {
            let _ = self.stop.take().unwrap().await;
        }
    }
    impl Drop for SlowExit {
        fn drop(&mut self) {
            if let Some(gate) = self.drop_gate.take() {
                let _ = gate.recv_timeout(LIMIT);
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn status_during_stopped_and_destruction() {
        let (stop_tx, stop) = tokio::sync::oneshot::channel();
        let (drop_tx, drop_gate) = std_mpsc::channel();
        let actor = SlowExit {
            stop: Some(stop),
            drop_gate: Some(drop_gate),
        }
        .start()
        .await
        .unwrap();
        actor.child_handle().stop();
        let until = std::time::Instant::now() + LIMIT;
        while actor.info().lifecycle != Lifecycle::Exiting {
            assert!(std::time::Instant::now() < until);
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let during_stopped = actor.get_status();
        stop_tx.send(()).unwrap();
        assert!(matches!(
            during_stopped.await,
            Err(ActorError::ActorStopped(ExitReason::Normal))
        ));
        assert!(matches!(
            actor.info().lifecycle,
            Lifecycle::Dead {
                published: false,
                ..
            }
        ));
        assert!(matches!(
            actor.get_state(|_| ()).await,
            Err(ActorError::NoProc)
        ));
        drop_tx.send(()).unwrap();
        actor.join().await;
    }

    /// I5: state is redacted unless the actor opts in; `inspect` formats it
    /// through `Inspect`, `inspect = debug` through `Debug`; output over
    /// 64 KiB is truncated.
    struct Secret;
    #[actor]
    impl Secret {}

    struct Shown {
        peers: usize,
    }
    impl Inspect for Shown {
        fn format_status(&self) -> String {
            format!("{} peers", self.peers)
        }
    }
    #[actor(inspect)]
    impl Shown {}

    #[derive(Debug)]
    #[allow(dead_code)] // read through Debug only
    struct Debugged {
        height: u64,
    }
    #[actor(inspect = debug)]
    impl Debugged {}

    struct Huge;
    impl Inspect for Huge {
        fn format_status(&self) -> String {
            "x".repeat(MAX_STATUS_BYTES + 10)
        }
    }
    #[actor(inspect)]
    impl Huge {}

    struct Faulty;
    impl Inspect for Faulty {
        fn format_status(&self) -> String {
            panic!("formatter boom")
        }
    }
    #[actor(inspect)]
    impl Faulty {}

    #[tokio::test]
    async fn state_is_redacted_by_default_and_formatted_on_opt_in() {
        let secret = Secret.start().await.unwrap();
        assert_eq!(
            secret.get_status().await.unwrap().state,
            StateView::Redacted
        );
        let shown = Shown { peers: 3 }.start().await.unwrap();
        assert_eq!(
            shown.child_handle().get_status().await.unwrap().state,
            StateView::Formatted("3 peers".into())
        );
        let debugged = Debugged { height: 7 }.start().await.unwrap();
        match debugged.get_status().await.unwrap().state {
            StateView::Formatted(text) => assert!(text.contains("height: 7"), "{text}"),
            other => panic!("{other:?}"),
        }
        let huge = Huge.start().await.unwrap();
        match huge.get_status().await.unwrap().state {
            StateView::Truncated(text) => assert_eq!(text.len(), MAX_STATUS_BYTES),
            other => panic!("{other:?}"),
        }
        let faulty = Faulty.start().await.unwrap();
        assert!(matches!(
            faulty.get_status().await,
            Err(ActorError::CallbackFailed)
        ));
        assert_eq!(faulty.info().lifecycle, Lifecycle::Idle);
        for handle in [
            secret.child_handle(),
            shown.child_handle(),
            debugged.child_handle(),
            huge.child_handle(),
            faulty.child_handle(),
        ] {
            handle.stop();
            handle.wait_exit_async().await;
        }
    }

    /// I4 with `metrics`: a status request is not a message: it is not in
    /// the mailbox depth.
    #[cfg(feature = "metrics")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn status_bypasses_metrics_mailbox_accounting() {
        let actor = Counter::default().start().await.unwrap();
        let (msg, entered, release) = wedge();
        actor.send(msg).unwrap();
        entered.recv_timeout(LIMIT).unwrap();
        let state = actor.get_state(|c| c.count);
        assert_eq!(actor.mailbox_depth(), 0);
        actor.send(Bump).unwrap();
        assert_eq!(actor.mailbox_depth(), 1);
        release.send(()).unwrap();
        assert_eq!(state.await.unwrap(), 1);
        actor.request(Bump).await.unwrap();
        assert_eq!(actor.mailbox_depth(), 0);
        actor.child_handle().stop();
        actor.join().await;
    }
}

#[cfg(not(spawned_sim))]
mod threads_mode {
    use super::*;
    use spawned_concurrency::threads::{Actor, ActorStart, Context, Handler};
    use spawned_concurrency::{actor, ReplyTo};
    use std::cell::RefCell;

    #[derive(Default)]
    struct Counter {
        count: u64,
        token: RefCell<Option<ReplyTo<()>>>,
    }
    impl Actor for Counter {}
    impl Handler<Wedge> for Counter {
        fn handle(&mut self, msg: Wedge, reply: PendingReply<'_, ()>, _: &Context<Self>) {
            self.count += 1;
            msg.entered.send(()).unwrap();
            let _ = msg.release.recv_timeout(LIMIT);
            reply.reply(());
        }
    }
    impl Handler<Bump> for Counter {
        fn handle(&mut self, _: Bump, reply: PendingReply<'_, u64>, _: &Context<Self>) {
            self.count += 1;
            reply.reply(self.count);
        }
    }
    impl Handler<Defer> for Counter {
        fn handle(&mut self, _: Defer, reply: PendingReply<'_, ()>, _: &Context<Self>) {
            *self.token.borrow_mut() = Some(reply.defer().unwrap());
        }
    }

    #[test]
    fn status_runs_between_turns_and_outcomes() {
        let actor = Counter::default().start().unwrap();
        let (msg, entered, release) = wedge();
        actor.send(msg).unwrap();
        entered.recv_timeout(LIMIT).unwrap();
        let ran = Arc::new(AtomicBool::new(false));
        let seen = ran.clone();
        let late = actor
            .with_request_timeout(Duration::from_millis(30))
            .get_state(move |_| seen.store(true, Ordering::SeqCst))
            .into_result();
        assert!(matches!(late, Err(ActorError::RequestTimeout)), "{late:?}");
        let a = actor.clone();
        let behind = std::thread::spawn(move || a.get_state(|c| c.count).into_result());
        std::thread::sleep(Duration::from_millis(20));
        actor.send(Bump).unwrap();
        release.send(()).unwrap();
        assert_eq!(behind.join().unwrap().unwrap(), 1);
        assert!(!ran.load(Ordering::SeqCst), "an abandoned callback ran");
        let report = actor.get_status().unwrap();
        assert_eq!(report.info.lifecycle, Lifecycle::Idle);
        assert_eq!(report.state, StateView::Redacted);
        actor.child_handle().stop();
        actor.join();
        assert!(matches!(
            actor.get_state(|c| c.count).into_result(),
            Err(ActorError::NoProc)
        ));
    }

    #[test]
    fn callback_panic_is_callback_failed_and_actor_continues() {
        let actor = Counter::default().start().unwrap();
        let deferred = actor
            .send_request(Defer, spawned_concurrency::Deadline::Never)
            .unwrap();
        actor.request(Bump).unwrap();
        let failed = actor
            .get_state(|c| {
                drop(c.token.borrow_mut().take());
                panic!("callback boom");
            })
            .into_result();
        assert!(matches!(failed, Err(ActorError::CallbackFailed)));
        assert!(matches!(deferred.recv(), Err(ActorError::ReplyDropped)));
        assert_eq!(actor.request(Bump).unwrap(), 2);
        actor.child_handle().stop();
        assert_eq!(actor.wait_exit(), ExitReason::Normal);
    }

    #[derive(Debug)]
    #[allow(dead_code)] // read through Debug only
    struct Debugged {
        height: u64,
    }
    #[actor(inspect = debug)]
    impl Debugged {}

    struct Shown;
    impl Inspect for Shown {
        fn format_status(&self) -> String {
            "shown".into()
        }
    }
    #[actor(inspect)]
    impl Shown {}

    #[test]
    fn inspect_formats_state() {
        inspect_formats_state_inner();
    }

    /// A type-erased `ChildHandle::get_status` never blocks an async caller,
    /// even on a threads-mode actor: on a current-thread runtime, another
    /// task keeps running while the status request waits for a wedged actor,
    /// and the request ends `RequestTimeout`.
    #[cfg(feature = "async")]
    #[test]
    fn child_handle_get_status_does_not_block_an_async_caller() {
        let actor = Counter::default().start().unwrap();
        let (msg, entered, release) = wedge();
        actor.send(msg).unwrap();
        entered.recv_timeout(LIMIT).unwrap();
        let handle = actor.child_handle();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (ticks, outcome) = runtime.block_on(async move {
            let ticks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let counter = ticks.clone();
            let ticker = tokio::spawn(async move {
                loop {
                    counter.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            });
            let outcome = handle
                .get_status_with_timeout(Duration::from_millis(200))
                .await;
            ticker.abort();
            (ticks.load(Ordering::SeqCst), outcome)
        });
        assert!(
            matches!(outcome, Err(ActorError::RequestTimeout)),
            "{outcome:?}"
        );
        assert!(ticks > 5, "the runtime was blocked: {ticks} ticks");
        release.send(()).unwrap();
        actor.child_handle().stop();
        actor.join();
    }

    fn inspect_formats_state_inner() {
        let debugged = Debugged { height: 9 }.start().unwrap();
        match debugged.child_handle().get_status().unwrap().state {
            StateView::Formatted(text) => assert!(text.contains("height: 9")),
            other => panic!("{other:?}"),
        }
        let shown = Shown.start().unwrap();
        assert_eq!(
            shown.get_status().unwrap().state,
            StateView::Formatted("shown".into())
        );
        debugged.child_handle().stop();
        shown.child_handle().stop();
        debugged.join();
        shown.join();
    }
}

/// I4 under seeded schedules: a status request racing the actor's stop has
/// exactly one outcome from the allowed set, and never runs inside a turn.
#[cfg(feature = "async")]
mod seeded {
    use super::*;
    use spawned_concurrency::tasks::{Actor, ActorStart, Context, Handler};

    struct Busy {
        in_turn: bool,
    }
    impl Actor for Busy {}
    impl Handler<Bump> for Busy {
        async fn handle(&mut self, _: Bump, reply: PendingReply<'_, u64>, _: &Context<Self>) {
            self.in_turn = true;
            spawned_rt::tasks::yield_now().await;
            self.in_turn = false;
            reply.reply(0);
        }
    }

    #[spawned_concurrency::sim_test]
    async fn get_status_races_stop() {
        let actor = Busy { in_turn: false }.spawn();
        let _ = actor.send(Bump);
        let first = actor.get_state(|b| b.in_turn);
        let _ = actor.send(Bump);
        actor.child_handle().stop();
        let second = actor.get_state(|b| b.in_turn);
        for outcome in [first.await, second.await] {
            match outcome {
                Ok(in_turn) => assert!(!in_turn, "status ran inside a turn"),
                Err(ActorError::ActorStopped(ExitReason::Normal)) | Err(ActorError::NoProc) => {}
                other => panic!("unexpected outcome {other:?}"),
            }
        }
        assert_eq!(actor.wait_exit().await, ExitReason::Normal);
    }
}
