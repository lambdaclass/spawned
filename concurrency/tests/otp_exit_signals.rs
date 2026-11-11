//! Exit signals, kills, NoProc and parent exits as in OTP
//! (docs/design/otp-exit-signals.md, ranks 5e and 5g). Each test names the
//! design rule it pins; every one fails on v0.7 (the API or the outcome
//! differs there).
#![cfg(feature = "async")]
use spawned_concurrency::{message::Message, ActorId, Down, Exit, ExitReason, NoProc};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

const LIMIT: Duration = Duration::from_secs(10);

/// What an actor observed: `exit_received` calls, `Down`s and whether
/// `stopped()` ran.
#[derive(Default, Clone)]
struct Seen {
    exits: Arc<Mutex<Vec<Exit>>>,
    downs: Arc<Mutex<Vec<Down>>>,
    stopped: Arc<AtomicBool>,
}
impl Seen {
    fn exits(&self) -> Vec<Exit> {
        self.exits.lock().unwrap().clone()
    }
    fn downs(&self) -> Vec<Down> {
        self.downs.lock().unwrap().clone()
    }
    fn stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }
}

struct Ping;
impl Message for Ping {
    type Result = u8;
}

mod tasks_mode {
    use super::*;
    use spawned_concurrency::tasks::{Actor, ActorRef, ActorStart, Context, Handler};
    use spawned_concurrency::ChildHandle;

    pub struct Node {
        pub seen: Seen,
        pub trap: bool,
        /// stopped() waits for this signal (to act while it runs).
        pub in_stopped: Option<(
            tokio::sync::oneshot::Sender<()>,
            tokio::sync::oneshot::Receiver<()>,
        )>,
        pub panic_in_stopped: bool,
    }
    impl Node {
        pub fn new(seen: &Seen, trap: bool) -> Self {
            Self {
                seen: seen.clone(),
                trap,
                in_stopped: None,
                panic_in_stopped: false,
            }
        }
    }
    impl Actor for Node {
        async fn started(&mut self, ctx: &Context<Self>) {
            if self.trap {
                ctx.trap_exit(true);
            }
        }
        async fn stopped(&mut self, _: &Context<Self>) {
            self.seen.stopped.store(true, Ordering::SeqCst);
            if let Some((entered, release)) = self.in_stopped.take() {
                let _ = entered.send(());
                let _ = release.await;
            }
            if self.panic_in_stopped {
                panic!("stopped() panic after the kill");
            }
        }
        async fn exit_received(&mut self, exit: Exit, _: &Context<Self>) {
            self.seen.exits.lock().unwrap().push(exit);
        }
    }
    impl Handler<Ping> for Node {
        async fn handle(
            &mut self,
            _: Ping,
            reply: spawned_concurrency::reply::PendingReply<'_, u8>,
            _: &Context<Self>,
        ) {
            reply.reply(7);
        }
    }
    impl Handler<Down> for Node {
        async fn handle(
            &mut self,
            down: Down,
            reply: spawned_concurrency::reply::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            self.seen.downs.lock().unwrap().push(down);
            reply.reply(());
        }
    }
    struct Do(Box<dyn FnOnce(&Context<Node>) + Send>);
    impl Message for Do {
        type Result = ();
    }
    impl Handler<Do> for Node {
        async fn handle(
            &mut self,
            f: Do,
            reply: spawned_concurrency::reply::PendingReply<'_, ()>,
            ctx: &Context<Self>,
        ) {
            (f.0)(ctx);
            reply.reply(());
        }
    }
    /// Run `f` inside one of `actor`'s handlers.
    pub async fn on(actor: &ActorRef<Node>, f: impl FnOnce(&Context<Node>) + Send + 'static) {
        actor.request(Do(Box::new(f))).await.unwrap();
    }
    pub async fn ended(child: &ChildHandle) -> ExitReason {
        tokio::time::timeout(LIMIT, child.wait_exit_async())
            .await
            .expect("actor did not exit")
    }
    /// A request round trip, so everything queued before it was handled.
    pub async fn settle(actor: &ActorRef<Node>) {
        assert_eq!(actor.request(Ping).await.unwrap(), 7);
    }

    // E1: kill() is untrappable and ends the target Killed without stopped();
    // links receive Killed (trappable), monitors Down { Killed }.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn kill_is_untrappable_and_propagates_killed() {
        let (target_seen, peer_seen, watcher_seen, trapper_seen) = (
            Seen::default(),
            Seen::default(),
            Seen::default(),
            Seen::default(),
        );
        let target = Node::new(&target_seen, true).start();
        let peer = Node::new(&peer_seen, false).start();
        let trapper = Node::new(&trapper_seen, true).start();
        let watcher = Node::new(&watcher_seen, false).start();
        settle(&target).await;
        settle(&trapper).await;
        let t = target.child_handle();
        let t2 = t.clone();
        on(&peer, move |ctx| ctx.link(&t2)).await;
        let t3 = t.clone();
        on(&trapper, move |ctx| ctx.link(&t3)).await;
        let t4 = t.clone();
        on(&watcher, move |ctx| {
            ctx.monitor(&t4);
        })
        .await;
        t.kill();
        assert_eq!(ended(&t).await, ExitReason::Killed);
        assert!(!target_seen.stopped(), "stopped() runs after a kill");
        assert!(target_seen.exits().is_empty(), "a kill was trapped");
        // A non-trapping peer ends Killed (and skips stopped()).
        assert_eq!(ended(&peer.child_handle()).await, ExitReason::Killed);
        assert!(!peer_seen.stopped());
        // A trapping peer gets the notice as a message.
        settle(&trapper).await;
        assert_eq!(
            trapper_seen.exits(),
            vec![Exit {
                from: t.id(),
                reason: ExitReason::Killed
            }]
        );
        settle(&watcher).await;
        assert_eq!(watcher_seen.downs().len(), 1);
        assert_eq!(watcher_seen.downs()[0].reason, ExitReason::Killed);
        trapper.child_handle().stop();
        watcher.child_handle().stop();
        ended(&trapper.child_handle()).await;
        ended(&watcher.child_handle()).await;
    }

    // E1 + precedence: a kill during stopped() replaces the requested reason;
    // the running stopped() finishes and a panic in it does not replace Killed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn kill_during_stopped_supersedes_and_panic_does_not_replace_it() {
        let seen = Seen::default();
        let (entered_tx, entered) = tokio::sync::oneshot::channel();
        let (release, release_rx) = tokio::sync::oneshot::channel();
        let mut node = Node::new(&seen, false);
        node.in_stopped = Some((entered_tx, release_rx));
        node.panic_in_stopped = true;
        let actor = node.start();
        settle(&actor).await;
        let child = actor.child_handle();
        child.stop_with_reason(ExitReason::Shutdown);
        entered.await.unwrap();
        child.kill();
        release.send(()).unwrap();
        assert_eq!(ended(&child).await, ExitReason::Killed);
        assert!(seen.stopped(), "a stopped() that began finishes");
    }

    // E1/decision 5: an administrative stop_with_reason(Kill) stays an
    // administrative stop ending Kill, and stopped() runs.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn administrative_kill_reason_runs_stopped_and_ends_kill() {
        let seen = Seen::default();
        let actor = Node::new(&seen, true).start();
        settle(&actor).await;
        actor.child_handle().stop_with_reason(ExitReason::Kill);
        assert_eq!(ended(&actor.child_handle()).await, ExitReason::Kill);
        assert!(seen.stopped());
    }

    // E2: exit(Normal) is ignored by a non-trapping actor and trapped as an
    // Exit from EXTERNAL by a trapping one; any other reason ends a
    // non-trapping actor with it, without stopped().
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn exit_signals_follow_exit_signal_2() {
        let plain_seen = Seen::default();
        let plain = Node::new(&plain_seen, false).start();
        settle(&plain).await;
        plain.child_handle().exit(ExitReason::Normal);
        settle(&plain).await;
        assert!(
            plain.child_handle().is_alive(),
            "Normal stopped a non-trapping actor"
        );

        let trap_seen = Seen::default();
        let trapper = Node::new(&trap_seen, true).start();
        settle(&trapper).await;
        trapper.child_handle().exit(ExitReason::Normal);
        trapper.child_handle().exit(ExitReason::Shutdown);
        settle(&trapper).await;
        assert_eq!(
            trap_seen.exits(),
            vec![
                Exit {
                    from: ActorId::EXTERNAL,
                    reason: ExitReason::Normal
                },
                Exit {
                    from: ActorId::EXTERNAL,
                    reason: ExitReason::Shutdown
                },
            ]
        );
        assert!(trapper.child_handle().is_alive());

        plain.child_handle().exit(ExitReason::Shutdown);
        assert_eq!(ended(&plain.child_handle()).await, ExitReason::Shutdown);
        assert!(
            !plain_seen.stopped(),
            "stopped() ran after an untrapped signal"
        );
        trapper.child_handle().stop();
        ended(&trapper.child_handle()).await;
    }

    // E2: ctx.exit sends from the calling actor; a Kill from an actor is a kill.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ctx_exit_sends_from_the_calling_actor() {
        let (a_seen, b_seen) = (Seen::default(), Seen::default());
        let a = Node::new(&a_seen, false).start();
        let b = Node::new(&b_seen, true).start();
        settle(&b).await;
        let bh = b.child_handle();
        let bh2 = bh.clone();
        on(&a, move |ctx| {
            ctx.exit(&bh2, ExitReason::Panic("from a".into()))
        })
        .await;
        settle(&b).await;
        assert_eq!(
            b_seen.exits(),
            vec![Exit {
                from: a.id(),
                reason: ExitReason::Panic("from a".into())
            }]
        );
        let bh3 = bh.clone();
        on(&a, move |ctx| ctx.exit(&bh3, ExitReason::Kill)).await;
        assert_eq!(ended(&bh).await, ExitReason::Killed);
        a.child_handle().stop();
        ended(&a.child_handle()).await;
    }

    // E4 + decision 1: link/monitor to an actor past its death point give
    // NoProc; a non-trapping caller exits NoProc without stopped();
    // try_link returns Err(NoProc) and keeps nothing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn registrations_after_death_report_noproc() {
        let dead_seen = Seen::default();
        let dead = Node::new(&dead_seen, false).start();
        dead.child_handle()
            .stop_with_reason(ExitReason::Panic("gone".into()));
        assert!(matches!(
            ended(&dead.child_handle()).await,
            ExitReason::Panic(_)
        ));
        let d = dead.child_handle();

        let trap_seen = Seen::default();
        let trapper = Node::new(&trap_seen, true).start();
        let (d1, d2) = (d.clone(), d.clone());
        on(&trapper, move |ctx| {
            ctx.link(&d1);
            ctx.monitor(&d2);
        })
        .await;
        settle(&trapper).await;
        assert_eq!(
            trap_seen.exits(),
            vec![Exit {
                from: d.id(),
                reason: ExitReason::NoProc
            }]
        );
        let downs = trap_seen.downs();
        assert_eq!(downs.len(), 1);
        assert_eq!(downs[0].reason, ExitReason::NoProc);

        let try_seen = Seen::default();
        let trier = Node::new(&try_seen, false).start();
        let result = Arc::new(Mutex::new(None));
        let (d3, r) = (d.clone(), result.clone());
        on(&trier, move |ctx| {
            *r.lock().unwrap() = Some(ctx.try_link(&d3))
        })
        .await;
        assert_eq!(*result.lock().unwrap(), Some(Err(NoProc)));
        settle(&trier).await;
        assert!(trier.child_handle().is_alive());

        let plain_seen = Seen::default();
        let plain = Node::new(&plain_seen, false).start();
        let d4 = d.clone();
        on(&plain, move |ctx| ctx.link(&d4)).await;
        assert_eq!(ended(&plain.child_handle()).await, ExitReason::NoProc);
        assert!(!plain_seen.stopped());

        trapper.child_handle().stop();
        trier.child_handle().stop();
        ended(&trapper.child_handle()).await;
        ended(&trier.child_handle()).await;
    }

    // Sender lifetime / semantics X2: a Context kept by a helper after its
    // actor exited cannot link: no record, no notice, no NoProc.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn link_from_a_dead_actor_is_inert() {
        let (dead_seen, peer_seen) = (Seen::default(), Seen::default());
        let dead = Node::new(&dead_seen, true).start();
        let peer = Node::new(&peer_seen, true).start();
        settle(&peer).await;
        let kept = dead.context();
        dead.child_handle().stop();
        ended(&dead.child_handle()).await;
        kept.link(&peer.child_handle());
        assert_eq!(kept.try_link(&peer.child_handle()), Ok(()));
        // The live peer gets no edge: its own exit notifies nobody.
        let gone = Node::new(&Seen::default(), false).start();
        kept.link(&gone.child_handle());
        gone.child_handle().stop();
        ended(&gone.child_handle()).await;
        peer.child_handle().exit(ExitReason::Panic("peer".into()));
        settle(&peer).await;
        peer.child_handle()
            .stop_with_reason(ExitReason::Panic("peer".into()));
        assert!(matches!(
            ended(&peer.child_handle()).await,
            ExitReason::Panic(_)
        ));
        assert!(dead_seen.exits().is_empty());
        assert_eq!(dead.child_handle().exit_reason(), Some(ExitReason::Normal));
    }

    pub struct Parent {
        pub child: Option<ActorRef<Node>>,
        pub child_seen: Seen,
    }
    impl Actor for Parent {}
    struct Spawn;
    impl Message for Spawn {
        type Result = ChildHandle;
    }
    impl Handler<Spawn> for Parent {
        async fn handle(
            &mut self,
            _: Spawn,
            reply: spawned_concurrency::reply::PendingReply<'_, ChildHandle>,
            ctx: &Context<Self>,
        ) {
            let child = Node::new(&self.child_seen, true).start_linked(ctx);
            reply.reply(child.child_handle());
            self.child = Some(child);
        }
    }

    // E3: a start_linked parent's exit, Normal included, ends a trapping
    // child through stopped(); exit_received never sees it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn parent_exit_ends_a_trapping_child_normally() {
        for parent_reason in [ExitReason::Normal, ExitReason::Shutdown] {
            let child_seen = Seen::default();
            let parent = Parent {
                child: None,
                child_seen: child_seen.clone(),
            }
            .start();
            let child = parent.request(Spawn).await.unwrap();
            parent
                .child_handle()
                .stop_with_reason(parent_reason.clone());
            assert_eq!(ended(&child).await, parent_reason);
            assert!(child_seen.stopped(), "the parent exit skipped stopped()");
            assert!(
                child_seen.exits().is_empty(),
                "exit_received saw the parent exit"
            );
        }
    }

    // Phase 0: signals that arrive while started() runs are resolved when it
    // returns, with the trap flag read then.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn signals_during_started_are_resolved_with_the_flag_set_in_started() {
        struct Slow(Seen, Option<tokio::sync::oneshot::Receiver<()>>);
        impl Actor for Slow {
            async fn started(&mut self, ctx: &Context<Self>) {
                self.1.take().unwrap().await.unwrap();
                ctx.trap_exit(true);
            }
            async fn exit_received(&mut self, exit: Exit, _: &Context<Self>) {
                self.0.exits.lock().unwrap().push(exit);
            }
        }
        impl Handler<Ping> for Slow {
            async fn handle(
                &mut self,
                _: Ping,
                reply: spawned_concurrency::reply::PendingReply<'_, u8>,
                _: &Context<Self>,
            ) {
                reply.reply(7);
            }
        }
        let seen = Seen::default();
        let (go, gate) = tokio::sync::oneshot::channel();
        let actor = Slow(seen.clone(), Some(gate)).start();
        actor.child_handle().exit(ExitReason::Shutdown);
        go.send(()).unwrap();
        assert_eq!(actor.request(Ping).await.unwrap(), 7);
        assert_eq!(
            seen.exits(),
            vec![Exit {
                from: ActorId::EXTERNAL,
                reason: ExitReason::Shutdown
            }]
        );
        actor.child_handle().stop();
        ended(&actor.child_handle()).await;
    }

    // Decision 8: a late non-kill signal replaces the requested reason of a
    // non-trapping actor before D; a running stopped() finishes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn late_signal_replaces_the_requested_reason() {
        let seen = Seen::default();
        let (entered_tx, entered) = tokio::sync::oneshot::channel();
        let (release, release_rx) = tokio::sync::oneshot::channel();
        let mut node = Node::new(&seen, false);
        node.in_stopped = Some((entered_tx, release_rx));
        let actor = node.start();
        settle(&actor).await;
        let child = actor.child_handle();
        child.stop();
        entered.await.unwrap();
        child.exit(ExitReason::Shutdown);
        release.send(()).unwrap();
        assert_eq!(ended(&child).await, ExitReason::Shutdown);
        assert!(seen.stopped());
    }
}

mod threads_mode {
    use super::*;
    use spawned_concurrency::threads::{Actor, ActorStart, Context, Handler};
    use spawned_concurrency::ChildHandle;

    struct Node(Seen, bool);
    impl Actor for Node {
        fn started(&mut self, ctx: &Context<Self>) {
            if self.1 {
                ctx.trap_exit(true);
            }
        }
        fn stopped(&mut self, _: &Context<Self>) {
            self.0.stopped.store(true, Ordering::SeqCst);
        }
        fn exit_received(&mut self, exit: Exit, _: &Context<Self>) {
            self.0.exits.lock().unwrap().push(exit);
        }
    }
    impl Handler<Ping> for Node {
        fn handle(
            &mut self,
            _: Ping,
            reply: spawned_concurrency::reply::PendingReply<'_, u8>,
            _: &Context<Self>,
        ) {
            reply.reply(7);
        }
    }
    fn ended(child: &ChildHandle) -> ExitReason {
        let (tx, rx) = std::sync::mpsc::channel();
        let c = child.clone();
        std::thread::spawn(move || tx.send(c.wait_exit_blocking()).unwrap());
        rx.recv_timeout(LIMIT).expect("actor did not exit")
    }

    #[test]
    fn kill_exit_and_noproc_in_threads_mode() {
        // Kill: untrappable, Killed, no stopped(); the trapping peer gets Killed.
        let (target_seen, peer_seen) = (Seen::default(), Seen::default());
        let target = Node(target_seen.clone(), true).start();
        let peer = Node(peer_seen.clone(), true).start();
        assert_eq!(target.request(Ping).unwrap(), 7);
        assert_eq!(peer.request(Ping).unwrap(), 7);
        peer.context().link(&target.child_handle());
        target.child_handle().kill();
        assert_eq!(ended(&target.child_handle()), ExitReason::Killed);
        assert!(!target_seen.stopped());
        assert_eq!(peer.request(Ping).unwrap(), 7);
        assert_eq!(
            peer_seen.exits(),
            vec![Exit {
                from: target.id(),
                reason: ExitReason::Killed
            }]
        );
        // NoProc for a late link and try_link.
        assert_eq!(peer.context().try_link(&target.child_handle()), Err(NoProc));
        peer.context().link(&target.child_handle());
        assert_eq!(peer.request(Ping).unwrap(), 7);
        assert_eq!(peer_seen.exits().last().unwrap().reason, ExitReason::NoProc);
        // exit(Normal) ignored by a non-trapping actor; exit(Shutdown) ends it
        // without stopped().
        let plain_seen = Seen::default();
        let plain = Node(plain_seen.clone(), false).start();
        plain.child_handle().exit(ExitReason::Normal);
        assert_eq!(plain.request(Ping).unwrap(), 7);
        plain.child_handle().exit(ExitReason::Shutdown);
        assert_eq!(ended(&plain.child_handle()), ExitReason::Shutdown);
        assert!(!plain_seen.stopped());
        peer.child_handle().stop();
        assert_eq!(ended(&peer.child_handle()), ExitReason::Normal);
    }
}
