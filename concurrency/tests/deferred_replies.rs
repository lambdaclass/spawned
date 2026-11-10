//! Deferred replies (docs/design/deferred-replies.md): every discharge path of
//! a request's reply slot, in `tasks` mode (also under `spawned_sim`) and in
//! `threads` mode.
use spawned_concurrency::{message::Message, ActorError, Deadline, DeferError, Delivery, ReplyTo};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Park the request: the handler defers it and stores the token.
pub struct Park(pub u64);
impl Message for Park {
    type Result = u64;
}
/// Answer every parked request with its value plus `0`.
pub struct Release(pub u64);
impl Message for Release {
    type Result = usize;
}
/// Drop every parked token between turns.
pub struct DropAll;
impl Message for DropAll {
    type Result = ();
}
/// Return without answering (the `PendingReply` is dropped in the turn).
pub struct Forget;
impl Message for Forget {
    type Result = u64;
}
/// Panic in the turn, after optionally deferring into a local.
pub struct Explode {
    pub defer_first: bool,
}
impl Message for Explode {
    type Result = u64;
}
/// Report what `defer()` returned (for sends, expired requests).
pub struct TryDefer;
impl Message for TryDefer {
    type Result = ();
}
/// Stop, answering parked tokens in `stopped()` when `answer` is set.
pub struct Stop {
    pub answer: bool,
}
impl Message for Stop {
    type Result = ();
}
pub struct Outstanding;
impl Message for Outstanding {
    type Result = usize;
}

/// Shared observations of the deferred-reply actor.
#[derive(Default)]
pub struct Seen {
    pub defer_errors: Mutex<Vec<DeferError>>,
    pub late: Mutex<Vec<Delivery>>,
}

#[cfg(feature = "async")]
mod tasks_mode {
    use super::*;
    use spawned_concurrency::tasks::{Actor, ActorStart, Context, Handler};
    use spawned_concurrency::PendingReply;

    pub struct Parker {
        pub parked: Vec<(u64, ReplyTo<u64>)>,
        pub answer_in_stopped: bool,
        pub seen: Arc<Seen>,
    }
    impl Parker {
        pub fn new(seen: Arc<Seen>) -> Self {
            Self {
                parked: Vec::new(),
                answer_in_stopped: false,
                seen,
            }
        }
    }
    impl Actor for Parker {
        const MAX_OUTSTANDING: usize = 2;
        async fn stopped(&mut self, _ctx: &Context<Self>) {
            if self.answer_in_stopped {
                for (v, token) in self.parked.drain(..) {
                    token.reply(v + 1000);
                }
            }
        }
    }
    impl Handler<Park> for Parker {
        async fn handle(&mut self, msg: Park, reply: PendingReply<'_, u64>, _: &Context<Self>) {
            match reply.defer() {
                Ok(token) => self.parked.push((msg.0, token)),
                Err(e) => self.seen.defer_errors.lock().unwrap().push(e),
            }
        }
    }
    impl Handler<Release> for Parker {
        async fn handle(
            &mut self,
            msg: Release,
            reply: PendingReply<'_, usize>,
            _: &Context<Self>,
        ) {
            let n = self.parked.len();
            for (v, token) in self.parked.drain(..) {
                let delivery = token.reply(v + msg.0);
                if delivery != Delivery::Delivered {
                    self.seen.late.lock().unwrap().push(delivery);
                }
            }
            reply.reply(n);
        }
    }
    impl Handler<DropAll> for Parker {
        async fn handle(&mut self, _: DropAll, reply: PendingReply<'_, ()>, _: &Context<Self>) {
            self.parked.clear();
            reply.reply(());
        }
    }
    impl Handler<Forget> for Parker {
        async fn handle(&mut self, _: Forget, _reply: PendingReply<'_, u64>, _: &Context<Self>) {}
    }
    impl Handler<Explode> for Parker {
        async fn handle(&mut self, msg: Explode, reply: PendingReply<'_, u64>, _: &Context<Self>) {
            let _held = if msg.defer_first {
                Some(reply.defer().expect("defer"))
            } else {
                None
            };
            panic!("explode");
        }
    }
    impl Handler<TryDefer> for Parker {
        async fn handle(&mut self, _: TryDefer, reply: PendingReply<'_, ()>, _: &Context<Self>) {
            match reply.defer() {
                Ok(token) => drop(token),
                Err(e) => self.seen.defer_errors.lock().unwrap().push(e),
            }
        }
    }
    impl Handler<Stop> for Parker {
        async fn handle(&mut self, msg: Stop, reply: PendingReply<'_, ()>, ctx: &Context<Self>) {
            self.answer_in_stopped = msg.answer;
            ctx.stop();
            reply.reply(());
        }
    }
    impl Handler<Outstanding> for Parker {
        async fn handle(
            &mut self,
            _: Outstanding,
            reply: PendingReply<'_, usize>,
            ctx: &Context<Self>,
        ) {
            reply.reply(ctx.deferred_outstanding());
        }
    }

    fn start() -> (spawned_concurrency::tasks::ActorRef<Parker>, Arc<Seen>) {
        let seen = Arc::new(Seen::default());
        (Parker::new(seen.clone()).start(), seen)
    }

    async fn settle(actor: &spawned_concurrency::tasks::ActorRef<Parker>) -> usize {
        actor.request(Outstanding).await.unwrap()
    }

    #[spawned_macros::sim_test]
    async fn deferred_reply_answers_from_a_later_turn() {
        let (actor, _) = start();
        let a = actor.request_raw_until(Park(1), Deadline::Never).unwrap();
        let b = actor.request_raw_until(Park(2), Deadline::Never).unwrap();
        assert_eq!(settle(&actor).await, 2);
        // The mailbox kept serving while both requests were parked.
        assert_eq!(actor.request(Release(10)).await.unwrap(), 2);
        assert_eq!(a.await.unwrap(), 11);
        assert_eq!(b.await.unwrap(), 12);
        assert_eq!(settle(&actor).await, 0);
        actor.context().stop();
        actor.join().await;
    }

    #[spawned_macros::sim_test]
    async fn dropped_token_between_turns_answers_reply_dropped() {
        let (actor, _) = start();
        let rx = actor.request_raw_until(Park(1), Deadline::Never).unwrap();
        actor.request(DropAll).await.unwrap();
        assert!(matches!(rx.await, Err(ActorError::ReplyDropped)));
        assert_eq!(settle(&actor).await, 0);
        actor.context().stop();
        actor.join().await;
    }

    #[spawned_macros::sim_test]
    async fn unanswered_turn_answers_reply_dropped() {
        let (actor, _) = start();
        assert!(matches!(
            actor.request(Forget).await,
            Err(ActorError::ReplyDropped)
        ));
        actor.context().stop();
        actor.join().await;
    }

    #[spawned_macros::sim_test]
    async fn panicking_turn_answers_actor_stopped() {
        for defer_first in [false, true] {
            let (actor, _) = start();
            let result = actor.request(Explode { defer_first }).await;
            assert!(
                matches!(result, Err(ActorError::ActorStopped)),
                "{result:?}"
            );
            actor.join().await;
        }
    }

    #[spawned_macros::sim_test]
    async fn defer_at_the_bound_answers_overloaded() {
        let (actor, seen) = start();
        let a = actor.request_raw_until(Park(1), Deadline::Never).unwrap();
        let b = actor.request_raw_until(Park(2), Deadline::Never).unwrap();
        // MAX_OUTSTANDING is 2: the third deferral is refused in the same step.
        assert!(matches!(
            actor.request(Park(3)).await,
            Err(ActorError::Overloaded)
        ));
        assert_eq!(*seen.defer_errors.lock().unwrap(), [DeferError::Overloaded]);
        assert_eq!(actor.request(Release(0)).await.unwrap(), 2);
        assert_eq!((a.await.unwrap(), b.await.unwrap()), (1, 2));
        actor.context().stop();
        actor.join().await;
    }

    #[spawned_macros::sim_test]
    async fn abandoned_caller_returns_the_credit_and_drops_the_late_reply() {
        let (actor, seen) = start();
        let rx = actor.request_raw_until(Park(1), Deadline::Never).unwrap();
        assert_eq!(settle(&actor).await, 1);
        drop(rx);
        assert_eq!(
            settle(&actor).await,
            0,
            "abandonment returns the credit at once"
        );
        actor.request(Release(0)).await.unwrap();
        assert_eq!(*seen.late.lock().unwrap(), [Delivery::CallerGone]);
        actor.context().stop();
        actor.join().await;
    }

    #[spawned_macros::sim_test]
    async fn sends_and_expired_requests_cannot_defer() {
        let (actor, seen) = start();
        actor.send(TryDefer).unwrap();
        let expired = actor
            .request_raw_until(TryDefer, Deadline::after(Duration::ZERO))
            .unwrap();
        assert!(matches!(expired.await, Err(ActorError::RequestTimeout)));
        settle(&actor).await;
        assert_eq!(
            *seen.defer_errors.lock().unwrap(),
            [DeferError::NoCaller, DeferError::Closed]
        );
        actor.context().stop();
        actor.join().await;
    }

    #[spawned_macros::sim_test]
    async fn late_reply_after_the_deadline_is_expired() {
        let (actor, seen) = start();
        let rx = actor
            .request_raw_until(Park(1), Deadline::after(Duration::from_millis(50)))
            .unwrap();
        assert!(matches!(rx.await, Err(ActorError::RequestTimeout)));
        actor.request(Release(0)).await.unwrap();
        assert_eq!(*seen.late.lock().unwrap(), [Delivery::Expired]);
        assert_eq!(settle(&actor).await, 0);
        actor.context().stop();
        actor.join().await;
    }

    #[spawned_macros::sim_test]
    async fn stopped_may_answer_stored_tokens() {
        let (actor, _) = start();
        let rx = actor.request_raw_until(Park(7), Deadline::Never).unwrap();
        actor.request(Stop { answer: true }).await.unwrap();
        assert_eq!(rx.await.unwrap(), 1007);
        actor.join().await;
    }

    #[spawned_macros::sim_test]
    async fn death_answers_stored_tokens_actor_stopped() {
        let (actor, _) = start();
        let rx = actor.request_raw_until(Park(7), Deadline::Never).unwrap();
        let queued_behind_stop = {
            actor.request(Stop { answer: false }).await.unwrap();
            actor.request_raw_until(Park(8), Deadline::Never)
        };
        assert!(matches!(rx.await, Err(ActorError::ActorStopped)));
        if let Ok(queued) = queued_behind_stop {
            assert!(matches!(queued.await, Err(ActorError::ActorStopped)));
        }
        actor.join().await;
    }

    #[spawned_macros::sim_test]
    async fn token_closed_resolves_when_the_caller_leaves() {
        use spawned_concurrency::tasks::ActorRef;
        struct Watcher {
            closed: Option<spawned_rt::tasks::JoinHandle<bool>>,
        }
        impl Actor for Watcher {}
        struct Watch;
        impl Message for Watch {
            type Result = ();
        }
        struct Ping;
        impl Message for Ping {
            type Result = ();
        }
        struct Done;
        impl Message for Done {
            type Result = bool;
        }
        impl Handler<Watch> for Watcher {
            async fn handle(&mut self, _: Watch, reply: PendingReply<'_, ()>, _: &Context<Self>) {
                let token = reply.defer().unwrap();
                self.closed = Some(spawned_rt::tasks::spawn(async move {
                    token.closed().await;
                    token.is_closed()
                }));
            }
        }
        impl Handler<Ping> for Watcher {
            async fn handle(&mut self, _: Ping, reply: PendingReply<'_, ()>, _: &Context<Self>) {
                reply.reply(());
            }
        }
        impl Handler<Done> for Watcher {
            async fn handle(&mut self, _: Done, reply: PendingReply<'_, bool>, _: &Context<Self>) {
                let closed = self.closed.take().unwrap().await.unwrap();
                reply.reply(closed);
            }
        }
        let actor: ActorRef<Watcher> = Watcher { closed: None }.start();
        let rx = actor.request_raw_until(Watch, Deadline::Never).unwrap();
        actor.request(Ping).await.unwrap(); // `Watch` was handled (FIFO)
        drop(rx);
        assert!(actor.request(Done).await.unwrap());
        actor.context().stop();
        actor.join().await;
    }

    #[spawned_macros::sim_test]
    async fn request_timeout_starts_at_send() {
        let (actor, _) = start();
        // Sent now with a short deadline, polled only after it passed.
        let response = actor
            .request_raw_until(Park(1), Deadline::after(Duration::from_millis(10)))
            .unwrap();
        spawned_rt::tasks::sleep(Duration::from_millis(50)).await;
        assert!(matches!(response.await, Err(ActorError::RequestTimeout)));
        actor.context().stop();
        actor.join().await;
    }

    #[spawned_macros::sim_test]
    async fn reply_pair_drives_a_manual_handler_directly() {
        let seen = Arc::new(Seen::default());
        let mut parker = Parker::new(seen);
        let actor = Parker::new(Arc::new(Seen::default())).start();
        let ctx = actor.context();
        let (reply, rx) = spawned_concurrency::reply::reply_pair::<u64>(Deadline::Never);
        parker.handle(Park(5), reply, &ctx).await;
        let (release, released) = spawned_concurrency::reply::reply_pair::<usize>(Deadline::Never);
        parker.handle(Release(1), release, &ctx).await;
        assert_eq!(released.await.unwrap(), 1);
        assert_eq!(rx.await.unwrap(), 6);
        actor.context().stop();
        actor.join().await;
    }
}

#[cfg(not(spawned_sim))]
mod threads_mode {
    use super::*;
    use spawned_concurrency::threads::{Actor, ActorStart, Context, Handler};
    use spawned_concurrency::PendingReply;

    pub struct Parker {
        parked: Vec<(u64, ReplyTo<u64>)>,
        answer_in_stopped: bool,
        seen: Arc<Seen>,
    }
    impl Actor for Parker {
        const MAX_OUTSTANDING: usize = 2;
        fn stopped(&mut self, _ctx: &Context<Self>) {
            if self.answer_in_stopped {
                for (v, token) in self.parked.drain(..) {
                    token.reply(v + 1000);
                }
            }
        }
    }
    impl Handler<Park> for Parker {
        fn handle(&mut self, msg: Park, reply: PendingReply<'_, u64>, _: &Context<Self>) {
            match reply.defer() {
                Ok(token) => self.parked.push((msg.0, token)),
                Err(e) => self.seen.defer_errors.lock().unwrap().push(e),
            }
        }
    }
    impl Handler<Release> for Parker {
        fn handle(&mut self, msg: Release, reply: PendingReply<'_, usize>, _: &Context<Self>) {
            let n = self.parked.len();
            for (v, token) in self.parked.drain(..) {
                let delivery = token.reply(v + msg.0);
                if delivery != Delivery::Delivered {
                    self.seen.late.lock().unwrap().push(delivery);
                }
            }
            reply.reply(n);
        }
    }
    impl Handler<DropAll> for Parker {
        fn handle(&mut self, _: DropAll, reply: PendingReply<'_, ()>, _: &Context<Self>) {
            self.parked.clear();
            reply.reply(());
        }
    }
    impl Handler<Forget> for Parker {
        fn handle(&mut self, _: Forget, _reply: PendingReply<'_, u64>, _: &Context<Self>) {}
    }
    impl Handler<Explode> for Parker {
        fn handle(&mut self, msg: Explode, reply: PendingReply<'_, u64>, _: &Context<Self>) {
            let _held = if msg.defer_first {
                Some(reply.defer().expect("defer"))
            } else {
                None
            };
            panic!("explode");
        }
    }
    impl Handler<Stop> for Parker {
        fn handle(&mut self, msg: Stop, reply: PendingReply<'_, ()>, ctx: &Context<Self>) {
            self.answer_in_stopped = msg.answer;
            ctx.stop();
            reply.reply(());
        }
    }
    impl Handler<Outstanding> for Parker {
        fn handle(&mut self, _: Outstanding, reply: PendingReply<'_, usize>, ctx: &Context<Self>) {
            reply.reply(ctx.deferred_outstanding());
        }
    }

    fn start() -> (spawned_concurrency::threads::ActorRef<Parker>, Arc<Seen>) {
        let seen = Arc::new(Seen::default());
        let actor = Parker {
            parked: Vec::new(),
            answer_in_stopped: false,
            seen: seen.clone(),
        }
        .start();
        (actor, seen)
    }

    #[test]
    fn every_discharge_path() {
        // Later turn.
        let (actor, seen) = start();
        let a = actor.request_raw_until(Park(1), Deadline::Never).unwrap();
        assert_eq!(actor.request(Outstanding).unwrap(), 1);
        assert_eq!(actor.request(Release(10)).unwrap(), 1);
        assert_eq!(a.recv().unwrap(), 11);
        // Dropped between turns.
        let b = actor.request_raw_until(Park(1), Deadline::Never).unwrap();
        actor.request(DropAll).unwrap();
        assert!(matches!(b.recv(), Err(ActorError::ReplyDropped)));
        // Unanswered turn.
        assert!(matches!(
            actor.request(Forget),
            Err(ActorError::ReplyDropped)
        ));
        // Bound.
        let c = actor.request_raw_until(Park(1), Deadline::Never).unwrap();
        let d = actor.request_raw_until(Park(2), Deadline::Never).unwrap();
        assert!(matches!(
            actor.request(Park(3)),
            Err(ActorError::Overloaded)
        ));
        assert_eq!(*seen.defer_errors.lock().unwrap(), [DeferError::Overloaded]);
        // Abandonment returns the credit.
        drop(c);
        assert_eq!(actor.request(Outstanding).unwrap(), 1);
        // Expired deadline: the receiver expires the slot; the late reply is dropped.
        let e = actor
            .request_raw_until(Park(4), Deadline::after(Duration::from_millis(20)))
            .unwrap();
        assert!(matches!(e.recv(), Err(ActorError::RequestTimeout)));
        actor.request(Release(0)).unwrap();
        assert_eq!(d.recv().unwrap(), 2);
        assert_eq!(
            *seen.late.lock().unwrap(),
            [Delivery::CallerGone, Delivery::Expired]
        );
        assert_eq!(actor.request(Outstanding).unwrap(), 0);
        // stopped() may answer; then death closes the rest.
        let f = actor.request_raw_until(Park(7), Deadline::Never).unwrap();
        actor.request(Stop { answer: true }).unwrap();
        assert_eq!(f.recv().unwrap(), 1007);
        actor.join();
        assert!(matches!(
            actor.request(Outstanding),
            Err(ActorError::ActorStopped)
        ));
    }

    #[test]
    fn panic_and_death_answer_actor_stopped() {
        for defer_first in [false, true] {
            let (actor, _) = start();
            assert!(matches!(
                actor.request(Explode { defer_first }),
                Err(ActorError::ActorStopped)
            ));
            actor.join();
        }
        let (actor, _) = start();
        let parked = actor.request_raw_until(Park(1), Deadline::Never).unwrap();
        actor.request(Stop { answer: false }).unwrap();
        assert!(matches!(parked.recv(), Err(ActorError::ActorStopped)));
        actor.join();
    }
}
