//! Exit signals, kills, NoProc and parent exits as in OTP
//! (docs/design/otp-exit-signals.md, ranks 5e and 5g). Each test names the
//! design rule it pins; every one fails on v0.7 (the API or the outcome
//! differs there).
#![cfg(feature = "async")]
// Real Tokio runtimes and OS threads (plain `#[tokio::test]`, threads mode):
// not run under the simulator. `exit_signals_sim.rs` explores the same
// orderings under `spawned_sim`.
#![cfg(not(spawned_sim))]
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
        /// Deferred tokens stored by `Hold`.
        pub held: Vec<spawned_concurrency::reply::ReplyTo<u8>>,
        /// stopped() drops the stored tokens before waiting on `in_stopped`.
        pub drop_held_in_stopped: bool,
    }
    impl Node {
        pub fn new(seen: &Seen, trap: bool) -> Self {
            Self {
                seen: seen.clone(),
                trap,
                in_stopped: None,
                panic_in_stopped: false,
                held: Vec::new(),
                drop_held_in_stopped: false,
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
            if self.drop_held_in_stopped {
                self.held.clear();
            }
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
    type Action = Box<dyn FnOnce(&Context<Node>) + Send>;
    struct Do(Action);
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
    /// Defer the request and store its token.
    struct Hold;
    impl Message for Hold {
        type Result = u8;
    }
    impl Handler<Hold> for Node {
        async fn handle(
            &mut self,
            _: Hold,
            reply: spawned_concurrency::reply::PendingReply<'_, u8>,
            _: &Context<Self>,
        ) {
            self.held.push(reply.defer().unwrap());
        }
    }
    /// Panic in a request handler (the request's obligation unwinds).
    struct Boom;
    impl Message for Boom {
        type Result = u8;
    }
    impl Handler<Boom> for Node {
        async fn handle(
            &mut self,
            _: Boom,
            _reply: spawned_concurrency::reply::PendingReply<'_, u8>,
            _: &Context<Self>,
        ) {
            panic!("boom in a request handler");
        }
    }
    /// Block the actor in a handler until released.
    struct Block(tokio::sync::oneshot::Receiver<()>);
    impl Message for Block {
        type Result = ();
    }
    impl Handler<Block> for Node {
        async fn handle(
            &mut self,
            msg: Block,
            reply: spawned_concurrency::reply::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            let _ = msg.0.await;
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

    // E3 with decision 2: supervisor shutdown is the parent's exit signal.
    // A non-trapping child ends Shutdown without stopped(); a trapping child
    // runs stopped(); a child that does not reach its death point within
    // shutdown_timeout is killed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn supervisor_shutdown_is_the_parents_exit_signal() {
        use spawned_concurrency::supervision::{ChildSpec, SupervisorConfig};
        use spawned_concurrency::system::ActorSystem;
        let system = ActorSystem::new().unwrap();
        let group = system.async_group("e3", 1).unwrap();
        let (plain_seen, trap_seen, stuck_seen) =
            (Seen::default(), Seen::default(), Seen::default());
        let (entered_tx, entered) = tokio::sync::oneshot::channel();
        let (hold, hold_rx) = tokio::sync::oneshot::channel::<()>();
        let stuck_args = Arc::new(Mutex::new(Some((entered_tx, hold_rx))));
        let mut tree = system.supervisor(SupervisorConfig::default()).unwrap();
        let (p, t, k) = (plain_seen.clone(), trap_seen.clone(), stuck_seen.clone());
        let plain = tree
            .add(ChildSpec::new("plain", &group, move |_| {
                let seen = p.clone();
                async move { Ok::<_, String>(Node::new(&seen, false)) }
            }))
            .unwrap();
        let trapping = tree
            .add(ChildSpec::new("trapping", &group, move |_| {
                let seen = t.clone();
                async move { Ok::<_, String>(Node::new(&seen, true)) }
            }))
            .unwrap();
        let stuck = tree
            .add(
                ChildSpec::new("stuck", &group, move |_| {
                    let seen = k.clone();
                    let gate = stuck_args.lock().unwrap().take().unwrap();
                    async move {
                        let mut node = Node::new(&seen, true);
                        node.in_stopped = Some(gate);
                        Ok::<_, String>(node)
                    }
                })
                .shutdown_timeout(Duration::from_millis(50)),
            )
            .unwrap();
        let supervisor = tree.start().await.unwrap();
        let (plain, trapping, stuck) = (
            plain.current().unwrap().child_handle(),
            trapping.current().unwrap().child_handle(),
            stuck.current().unwrap().child_handle(),
        );
        let shutdown = tokio::spawn(async move { supervisor.shutdown(LIMIT).await.complete });
        entered.await.unwrap();
        // The stuck stopped() outlives shutdown_timeout (50 ms): the kill
        // lands while it runs; Rust cannot interrupt it, so release it after.
        tokio::time::sleep(Duration::from_millis(300)).await;
        drop(hold);
        assert!(shutdown.await.unwrap());
        assert_eq!(ended(&plain).await, ExitReason::Shutdown);
        assert!(!plain_seen.stopped(), "a non-trapping child ran stopped()");
        assert_eq!(ended(&trapping).await, ExitReason::Shutdown);
        assert!(trap_seen.stopped());
        assert!(
            trap_seen.exits().is_empty(),
            "the parent exit reached exit_received"
        );
        assert_eq!(ended(&stuck).await, ExitReason::Killed);
        assert!(stuck_seen.stopped());
        assert!(system.shutdown(LIMIT).await.complete);
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

    // Context::exit_pending: a trapping supervised child sees the
    // supervisor's queued parent exit in the handlers of messages queued
    // before it; a trapped Exit from a peer is not a pending exit.
    mod exit_pending {
        use super::*;
        use spawned_concurrency::supervision::{ChildSpec, SupervisorConfig};
        use spawned_concurrency::system::ActorSystem;

        type Log = Arc<Mutex<Vec<(&'static str, bool)>>>;
        pub struct Late(Log);
        impl Actor for Late {
            async fn started(&mut self, ctx: &Context<Self>) {
                ctx.trap_exit(true);
                self.0.lock().unwrap().push(("started", ctx.exit_pending()));
            }
            async fn exit_received(&mut self, _: Exit, ctx: &Context<Self>) {
                self.0.lock().unwrap().push(("exit", ctx.exit_pending()));
            }
            async fn stopped(&mut self, ctx: &Context<Self>) {
                self.0.lock().unwrap().push(("stopped", ctx.exit_pending()));
            }
        }
        struct Probe;
        impl Message for Probe {
            type Result = ();
        }
        impl Handler<Probe> for Late {
            async fn handle(
                &mut self,
                _: Probe,
                reply: spawned_concurrency::reply::PendingReply<'_, ()>,
                ctx: &Context<Self>,
            ) {
                self.0.lock().unwrap().push(("probe", ctx.exit_pending()));
                reply.reply(());
            }
        }
        /// Wait (bounded) until a shutdown is pending, then record it.
        struct AwaitShutdown;
        impl Message for AwaitShutdown {
            type Result = ();
        }
        impl Handler<AwaitShutdown> for Late {
            async fn handle(
                &mut self,
                _: AwaitShutdown,
                reply: spawned_concurrency::reply::PendingReply<'_, ()>,
                ctx: &Context<Self>,
            ) {
                let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
                while !ctx.exit_pending() && tokio::time::Instant::now() < deadline {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                self.0.lock().unwrap().push(("await", ctx.exit_pending()));
                reply.reply(());
            }
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn queued_work_sees_the_supervisors_pending_shutdown() {
            let system = ActorSystem::new().unwrap();
            let group = system.async_group("pending", 1).unwrap();
            let log: Log = Arc::default();
            let mut tree = system.supervisor(SupervisorConfig::default()).unwrap();
            let l = log.clone();
            let child = tree
                .add(
                    ChildSpec::new("late", &group, move |_| {
                        let log = l.clone();
                        async move { Ok::<_, String>(Late(log)) }
                    })
                    // No kill escalation while the test waits.
                    .shutdown_timeout(LIMIT),
                )
                .unwrap();
            let supervisor = tree.start().await.unwrap();
            let child = child.current().unwrap();
            // Normal operation, and a trapped Exit from outside: not pending.
            child.request(Probe).await.unwrap();
            child.child_handle().exit(ExitReason::Shutdown);
            child.request(Probe).await.unwrap();
            // Work queued before the supervisor's shutdown.
            child.send(AwaitShutdown).unwrap();
            child.send(Probe).unwrap();
            child.send(Probe).unwrap();
            let handle = child.child_handle();
            assert!(supervisor.shutdown(LIMIT).await.complete);
            assert_eq!(ended(&handle).await, ExitReason::Shutdown);
            assert_eq!(
                *log.lock().unwrap(),
                vec![
                    ("started", false),
                    ("probe", false),
                    ("exit", false),
                    ("probe", false),
                    ("await", true),
                    ("probe", true),
                    ("probe", true),
                    ("stopped", true),
                ]
            );
            assert!(system.shutdown(LIMIT).await.complete);
        }

        // A non-trapping actor: an untrapped signal begins exit, so the
        // handler that is running when it arrives sees it pending; ctx.stop()
        // is pending at once.
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn begun_exits_are_pending() {
            let seen = Seen::default();
            let actor = Node::new(&seen, false).start();
            let pending = Arc::new(Mutex::new(Vec::new()));
            let p = pending.clone();
            on(&actor, move |ctx| {
                p.lock().unwrap().push(ctx.exit_pending());
                ctx.stop();
                p.lock().unwrap().push(ctx.exit_pending());
            })
            .await;
            assert_eq!(ended(&actor.child_handle()).await, ExitReason::Normal);
            assert_eq!(*pending.lock().unwrap(), vec![false, true]);
            let actor = Node::new(&seen, false).start();
            let handle = actor.child_handle();
            let p = pending.clone();
            on(&actor, move |ctx| {
                handle.exit(ExitReason::Shutdown);
                p.lock().unwrap().push(ctx.exit_pending());
            })
            .await;
            assert_eq!(ended(&actor.child_handle()).await, ExitReason::Shutdown);
            assert_eq!(*pending.lock().unwrap(), vec![false, true, true]);
        }
    }

    // Decision 2 is easy to miss: a skipped stopped() is logged at debug
    // (`event = "stopped_skipped"`), for a kill, an untrapped signal and a
    // runtime drop. A current-thread runtime keeps the scoped subscriber on
    // the actors' thread.
    mod stopped_skipped_log {
        use super::*;
        use std::collections::BTreeMap;
        type Fields = BTreeMap<String, String>;
        #[derive(Clone, Default)]
        struct Capture(Arc<Mutex<Vec<(tracing::Level, Fields)>>>);
        impl tracing::Subscriber for Capture {
            fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
                true
            }
            fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
                tracing::span::Id::from_u64(1)
            }
            fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
            fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
            fn enter(&self, _: &tracing::span::Id) {}
            fn exit(&self, _: &tracing::span::Id) {}
            fn event(&self, event: &tracing::Event<'_>) {
                struct Visitor(Fields);
                impl tracing::field::Visit for Visitor {
                    fn record_debug(
                        &mut self,
                        field: &tracing::field::Field,
                        value: &dyn std::fmt::Debug,
                    ) {
                        self.0.insert(field.name().to_owned(), format!("{value:?}"));
                    }
                    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                        self.0.insert(field.name().to_owned(), value.to_owned());
                    }
                }
                let mut visitor = Visitor(Fields::new());
                event.record(&mut visitor);
                self.0
                    .lock()
                    .unwrap()
                    .push((*event.metadata().level(), visitor.0));
            }
        }
        impl Capture {
            /// (cause, reason) of each `stopped_skipped` event for `actor`.
            fn skipped(&self, actor: ActorId) -> Vec<(String, String)> {
                self.0
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|(level, f)| {
                        *level == tracing::Level::DEBUG
                            && f.get("event").map(String::as_str) == Some("stopped_skipped")
                            && f.get("actor") == Some(&actor.to_string())
                    })
                    .map(|(_, f)| (f["cause"].clone(), f["reason"].clone()))
                    .collect()
            }
        }

        #[test]
        fn skipped_stopped_is_logged_at_debug() {
            let capture = Capture::default();
            let _subscriber = tracing::subscriber::set_default(capture.clone());
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let (killed, signalled, trapping) = runtime.block_on(async {
                let seen = Seen::default();
                let killed = Node::new(&seen, true).start();
                settle(&killed).await;
                // Another test thread may have been registering the
                // `stopped_skipped` callsite while `set_default` rebuilt the
                // interest cache, leaving it cached as "never".
                tracing::callsite::rebuild_interest_cache();
                killed.child_handle().kill();
                assert_eq!(ended(&killed.child_handle()).await, ExitReason::Killed);
                let signalled = Node::new(&seen, false).start();
                settle(&signalled).await;
                signalled.child_handle().exit(ExitReason::Shutdown);
                assert_eq!(ended(&signalled.child_handle()).await, ExitReason::Shutdown);
                assert!(!seen.stopped());
                // stopped() runs: nothing logged.
                let trapping = Node::new(&seen, true).start();
                settle(&trapping).await;
                trapping.child_handle().stop();
                assert_eq!(ended(&trapping.child_handle()).await, ExitReason::Normal);
                assert!(seen.stopped());
                (killed.id(), signalled.id(), trapping.id())
            });
            assert_eq!(
                capture.skipped(killed),
                vec![("kill".into(), "killed".into())]
            );
            assert_eq!(
                capture.skipped(signalled),
                vec![("exit_signal".into(), "shutdown".into())]
            );
            assert!(capture.skipped(trapping).is_empty());
            // Runtime drop before stopped() began.
            let dropped = runtime.block_on(async {
                let actor = Node::new(&Seen::default(), true).start();
                settle(&actor).await;
                actor.id()
            });
            drop(runtime);
            assert_eq!(
                capture.skipped(dropped),
                vec![("runtime_drop".into(), "killed".into())]
            );
        }
    }

    // Rank 11 x 5e ("Required changes to rank 11"): callers, `Down` and
    // `wait_exit` see one reason. Each test pins one rule of the design.
    mod caller_errors {
        use super::*;
        use spawned_concurrency::tasks::ActorRef;
        use spawned_concurrency::ActorError;

        async fn answered<R>(
            rx: spawned_concurrency::reply::ReplyReceiver<R>,
        ) -> Result<R, ActorError>
        where
            R: Send + 'static,
        {
            tokio::time::timeout(LIMIT, rx)
                .await
                .expect("the request was never answered")
        }

        /// Block `actor` in a handler; returns the release.
        async fn block(actor: &ActorRef<Node>) -> tokio::sync::oneshot::Sender<()> {
            let (release, gate) = tokio::sync::oneshot::channel();
            actor.send(Block(gate)).unwrap();
            release
        }

        fn stopped_with(result: Result<impl std::fmt::Debug, ActorError>, reason: &ExitReason) {
            match result {
                Err(ActorError::ActorStopped(got)) => assert_eq!(&got, reason),
                other => panic!("expected ActorStopped({reason:?}), got {other:?}"),
            }
        }

        // A request queued, or a token stored, at D is answered with the
        // final reason: Killed after a kill (stopped() skipped).
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn queued_and_stored_requests_get_killed_after_a_kill() {
            let seen = Seen::default();
            let actor = Node::new(&seen, false).start();
            let stored = actor
                .send_request(Hold, spawned_concurrency::DEFAULT_REQUEST_TIMEOUT)
                .unwrap();
            settle(&actor).await;
            let release = block(&actor).await;
            let queued = actor
                .send_request(Ping, spawned_concurrency::DEFAULT_REQUEST_TIMEOUT)
                .unwrap();
            let child = actor.child_handle();
            child.kill();
            release.send(()).unwrap();
            assert_eq!(ended(&child).await, ExitReason::Killed);
            stopped_with(answered(queued).await, &ExitReason::Killed);
            stopped_with(answered(stored).await, &ExitReason::Killed);
        }

        // An untrapped exit signal: queued requests get the signal's reason.
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn queued_request_gets_the_untrapped_signals_reason() {
            let seen = Seen::default();
            let actor = Node::new(&seen, false).start();
            settle(&actor).await;
            let release = block(&actor).await;
            let queued = actor
                .send_request(Ping, spawned_concurrency::DEFAULT_REQUEST_TIMEOUT)
                .unwrap();
            let child = actor.child_handle();
            let reason = ExitReason::Panic("signalled".into());
            child.exit(reason.clone());
            release.send(()).unwrap();
            assert_eq!(ended(&child).await, reason);
            stopped_with(answered(queued).await, &reason);
        }

        // A request whose handler panicked is not answered at once: it waits
        // for D (stopped() runs first) and gets the final reason. A kill
        // during stopped() makes that reason Killed for the caller too.
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn panicked_request_is_answered_at_death_with_the_final_reason() {
            for kill in [false, true] {
                let seen = Seen::default();
                let (entered_tx, entered) = tokio::sync::oneshot::channel();
                let (release, release_rx) = tokio::sync::oneshot::channel();
                let mut node = Node::new(&seen, false);
                node.in_stopped = Some((entered_tx, release_rx));
                let actor = node.start();
                settle(&actor).await;
                let mut crashed = actor
                    .send_request(Boom, spawned_concurrency::DEFAULT_REQUEST_TIMEOUT)
                    .unwrap();
                entered.await.unwrap();
                tokio::time::sleep(Duration::from_millis(50)).await;
                assert!(
                    crashed.try_recv().is_none(),
                    "the panicked request was answered before D"
                );
                let child = actor.child_handle();
                if kill {
                    child.kill();
                }
                release.send(()).unwrap();
                let reason = ended(&child).await;
                if kill {
                    assert_eq!(reason, ExitReason::Killed);
                } else {
                    assert!(matches!(reason, ExitReason::Panic(_)), "{reason:?}");
                }
                stopped_with(answered(crashed).await, &reason);
            }
        }

        // A token dropped while the owner exits is held until D, then
        // answered with the final reason.
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn token_dropped_while_exiting_is_held_until_death() {
            let seen = Seen::default();
            let (entered_tx, entered) = tokio::sync::oneshot::channel();
            let (release, release_rx) = tokio::sync::oneshot::channel();
            let mut node = Node::new(&seen, false);
            node.in_stopped = Some((entered_tx, release_rx));
            node.drop_held_in_stopped = true;
            let actor = node.start();
            let mut stored = actor
                .send_request(Hold, spawned_concurrency::DEFAULT_REQUEST_TIMEOUT)
                .unwrap();
            settle(&actor).await;
            let child = actor.child_handle();
            child.stop_with_reason(ExitReason::Shutdown);
            entered.await.unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert!(
                stored.try_recv().is_none(),
                "a token dropped in stopped() was answered before D"
            );
            release.send(()).unwrap();
            assert_eq!(ended(&child).await, ExitReason::Shutdown);
            stopped_with(answered(stored).await, &ExitReason::Shutdown);
        }

        // A request sent while the actor exits (a slow stopped() of a
        // trapping actor) is admitted and answered at D; after D, send and
        // request report NoProc.
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn admitted_while_exiting_noproc_after_death() {
            let seen = Seen::default();
            let (entered_tx, entered) = tokio::sync::oneshot::channel();
            let (release, release_rx) = tokio::sync::oneshot::channel();
            let mut node = Node::new(&seen, true);
            node.in_stopped = Some((entered_tx, release_rx));
            let actor = node.start();
            settle(&actor).await;
            let child = actor.child_handle();
            child.stop_with_reason(ExitReason::Shutdown);
            entered.await.unwrap();
            let admitted = actor
                .send_request(Ping, spawned_concurrency::DEFAULT_REQUEST_TIMEOUT)
                .expect("admitted while exiting");
            actor.send(Ping).expect("a send while exiting is admitted");
            release.send(()).unwrap();
            assert_eq!(ended(&child).await, ExitReason::Shutdown);
            stopped_with(answered(admitted).await, &ExitReason::Shutdown);
            assert!(matches!(actor.send(Ping), Err(ActorError::NoProc)));
            assert!(matches!(actor.request(Ping).await, Err(ActorError::NoProc)));
            assert!(matches!(
                actor.send_request(Ping, spawned_concurrency::DEFAULT_REQUEST_TIMEOUT),
                Err(ActorError::NoProc)
            ));
            let recipient = actor.recipient::<Ping>();
            assert!(matches!(recipient.send(Ping), Err(ActorError::NoProc)));
        }
    }

    mod startup {
        use super::*;
        use spawned_concurrency::supervision::{
            ChildSpec, ChildState, RestartMode, SupervisorConfig, SupervisorError,
            SupervisorStrategy,
        };
        use spawned_concurrency::system::ActorSystem;
        use std::sync::atomic::AtomicUsize;

        fn terminal(state: ChildState) -> bool {
            matches!(state, ChildState::Stopped | ChildState::Failed)
        }

        // S1: a child's first start that fails aborts start() whatever the
        // restart type: no retry (v0.7 retried Permanent and Transient
        // children and timeouts within the restart budget).
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn first_start_failure_aborts_start_whatever_the_restart_type() {
            for (mode, timeout) in [
                (RestartMode::Permanent, false),
                (RestartMode::Transient, false),
                (RestartMode::Permanent, true),
                (RestartMode::Transient, true),
            ] {
                let system = ActorSystem::new().unwrap();
                let group = system.async_group("s1", 1).unwrap();
                let calls = Arc::new(AtomicUsize::new(0));
                let mut tree = system.supervisor(SupervisorConfig::default()).unwrap();
                let seen = Seen::default();
                tree.add(ChildSpec::new("first", &group, move |_| {
                    let seen = seen.clone();
                    async move { Ok::<_, String>(Node::new(&seen, false)) }
                }))
                .unwrap();
                let counted = calls.clone();
                tree.add(
                    ChildSpec::new("failing", &group, move |_| {
                        counted.fetch_add(1, Ordering::SeqCst);
                        async move {
                            if timeout {
                                std::future::pending::<()>().await;
                            }
                            Err::<Node, _>("boot failed")
                        }
                    })
                    .restart(mode)
                    .startup_timeout(Duration::from_millis(50)),
                )
                .unwrap();
                let result = tokio::time::timeout(LIMIT, tree.start()).await.unwrap();
                match result {
                    Err(SupervisorError::FailedToStartChild { child, .. }) => {
                        assert_eq!(child, "failing")
                    }
                    other => panic!("{mode:?}/{timeout}: unexpected {:?}", other.err()),
                }
                assert_eq!(
                    calls.load(Ordering::SeqCst),
                    1,
                    "{mode:?}: first start retried"
                );
                assert!(system.shutdown(LIMIT).await.complete);
            }
        }

        // S1: a ready child's exit during the initial start is recorded and
        // processed after the tree commits ready, under the strategy (v0.7:
        // OneForOne restarted at once, OneForAll/RestForOne failed start()).
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn ready_child_exit_during_startup_is_processed_afterwards() {
            for strategy in [
                SupervisorStrategy::OneForOne,
                SupervisorStrategy::OneForAll,
                SupervisorStrategy::RestForOne,
            ] {
                let system = ActorSystem::new().unwrap();
                let group = system.async_group("s1", 1).unwrap();
                let config = SupervisorConfig {
                    strategy,
                    ..SupervisorConfig::default()
                };
                let mut tree = system.supervisor(config).unwrap();
                let seen = Seen::default();
                let first = tree
                    .add(ChildSpec::new("first", &group, move |_| {
                        let seen = seen.clone();
                        async move { Ok::<_, String>(Node::new(&seen, false)) }
                    }))
                    .unwrap();
                let (entered_tx, entered) = tokio::sync::oneshot::channel::<()>();
                let (release, release_rx) = tokio::sync::oneshot::channel::<()>();
                let gate = Arc::new(Mutex::new(Some((entered_tx, release_rx))));
                let second = tree
                    .add(ChildSpec::new("second", &group, move |_| {
                        let gate = gate.lock().unwrap().take();
                        let seen = Seen::default();
                        async move {
                            if let Some((entered, release)) = gate {
                                let _ = entered.send(());
                                let _ = release.await;
                            }
                            Ok::<_, String>(Node::new(&seen, false))
                        }
                    }))
                    .unwrap();
                let start = tokio::spawn(tree.start());
                entered.await.unwrap();
                let handle = first.current().unwrap().child_handle();
                handle.kill();
                assert_eq!(ended(&handle).await, ExitReason::Killed);
                tokio::time::sleep(Duration::from_millis(100)).await;
                assert_eq!(
                    first.snapshot().generation,
                    1,
                    "{strategy:?}: restarted before the initial start completed"
                );
                release.send(()).unwrap();
                let supervisor = tokio::time::timeout(LIMIT, start)
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap_or_else(|e| panic!("{strategy:?}: start failed: {e}"));
                tokio::time::timeout(LIMIT, first.wait_for_generation(1))
                    .await
                    .unwrap()
                    .unwrap();
                if strategy == SupervisorStrategy::OneForOne {
                    assert_eq!(second.snapshot().generation, 1);
                } else {
                    tokio::time::timeout(LIMIT, second.wait_for_generation(1))
                        .await
                        .unwrap()
                        .unwrap();
                }
                assert!(supervisor.shutdown(LIMIT).await.complete);
                assert!(system.shutdown(LIMIT).await.complete);
            }
        }

        // S2: start() returns its error only after the rollback finished:
        // every started child is terminal, including one whose stopped()
        // gets stuck (killed at shutdown_timeout, then stuck after a second
        // interval), whose cleanup failure is listed separately.
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn start_returns_its_error_after_rollback() {
            let system = ActorSystem::new().unwrap();
            let group = system.async_group("s2", 2).unwrap();
            let mut tree = system.supervisor(SupervisorConfig::default()).unwrap();
            let slow_seen = Seen::default();
            let s = slow_seen.clone();
            let slow = tree
                .add(ChildSpec::new("slow", &group, move |_| {
                    let seen = s.clone();
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    // stopped() waits until the sender's timer fires.
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        let _ = tx.send(());
                    });
                    let (entered, _) = tokio::sync::oneshot::channel();
                    async move {
                        let mut node = Node::new(&seen, true);
                        node.in_stopped = Some((entered, rx));
                        Ok::<_, String>(node)
                    }
                }))
                .unwrap();
            let (_hold, hold_rx) = tokio::sync::oneshot::channel::<()>();
            let hold_rx = Arc::new(Mutex::new(Some(hold_rx)));
            let stuck = tree
                .add(
                    ChildSpec::new("stuck", &group, move |_| {
                        let rx = hold_rx.lock().unwrap().take().unwrap();
                        let (entered, _) = tokio::sync::oneshot::channel();
                        async move {
                            let mut node = Node::new(&Seen::default(), true);
                            node.in_stopped = Some((entered, rx));
                            Ok::<_, String>(node)
                        }
                    })
                    .shutdown_timeout(Duration::from_millis(50)),
                )
                .unwrap();
            tree.add(
                ChildSpec::new("failing", &group, |_| async {
                    Err::<Node, _>("boot failed")
                })
                .restart(RestartMode::Temporary),
            )
            .unwrap();
            let result = tokio::time::timeout(LIMIT, tree.start()).await.unwrap();
            assert!(slow_seen.stopped(), "start() returned before rollback");
            assert!(terminal(slow.snapshot().state), "{:?}", slow.snapshot());
            assert_eq!(stuck.snapshot().state, ChildState::Failed);
            match result {
                Err(SupervisorError::FailedToStartChild {
                    child,
                    rollback_failures,
                    ..
                }) => {
                    assert_eq!(child, "failing");
                    assert!(
                        rollback_failures.iter().any(|f| f.contains("stuck")),
                        "{rollback_failures:?}"
                    );
                }
                other => panic!("unexpected {:?}", other.err()),
            }
            let _ = system.shutdown(Duration::from_millis(100)).await;
        }
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

    // Rank 11 x 5e in threads mode: the final reason at D, held exit errors,
    // admission while exiting and NoProc after D.
    mod caller_errors {
        use super::*;
        use spawned_concurrency::reply::{PendingReply, ReplyReceiver, ReplyTo};
        use spawned_concurrency::ActorError;
        use std::sync::mpsc;

        #[derive(Default)]
        struct Server {
            held: Vec<ReplyTo<u8>>,
            /// stopped() drops the held tokens, reports entry, then waits.
            in_stopped: Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>,
        }
        impl Actor for Server {
            fn stopped(&mut self, _: &Context<Self>) {
                self.held.clear();
                if let Some((entered, release)) = self.in_stopped.take() {
                    entered.send(()).unwrap();
                    let _ = release.recv();
                }
            }
        }
        impl Handler<Ping> for Server {
            fn handle(&mut self, _: Ping, reply: PendingReply<'_, u8>, _: &Context<Self>) {
                reply.reply(7);
            }
        }
        struct Hold;
        impl Message for Hold {
            type Result = u8;
        }
        impl Handler<Hold> for Server {
            fn handle(&mut self, _: Hold, reply: PendingReply<'_, u8>, _: &Context<Self>) {
                self.held.push(reply.defer().unwrap());
            }
        }
        struct Boom;
        impl Message for Boom {
            type Result = u8;
        }
        impl Handler<Boom> for Server {
            fn handle(&mut self, _: Boom, _reply: PendingReply<'_, u8>, _: &Context<Self>) {
                panic!("boom in a request handler");
            }
        }
        struct Block(mpsc::Receiver<()>);
        impl Message for Block {
            type Result = ();
        }
        impl Handler<Block> for Server {
            fn handle(&mut self, msg: Block, reply: PendingReply<'_, ()>, _: &Context<Self>) {
                let _ = msg.0.recv();
                reply.reply(());
            }
        }

        fn stopped_with<R: std::fmt::Debug>(rx: ReplyReceiver<R>, reason: &ExitReason) {
            match rx.recv() {
                Err(ActorError::ActorStopped(got)) => assert_eq!(&got, reason),
                other => panic!("expected ActorStopped({reason:?}), got {other:?}"),
            }
        }

        fn gated() -> (Server, mpsc::Receiver<()>, mpsc::Sender<()>) {
            let (entered_tx, entered) = mpsc::channel();
            let (release, release_rx) = mpsc::channel();
            let server = Server {
                in_stopped: Some((entered_tx, release_rx)),
                ..Server::default()
            };
            (server, entered, release)
        }

        #[test]
        fn queued_and_stored_requests_get_killed_after_a_kill() {
            let actor = Server::default().start();
            let stored = actor
                .send_request(Hold, spawned_concurrency::DEFAULT_REQUEST_TIMEOUT)
                .unwrap();
            assert_eq!(actor.request(Ping).unwrap(), 7);
            let (release, gate) = mpsc::channel();
            actor.send(Block(gate)).unwrap();
            let queued = actor
                .send_request(Ping, spawned_concurrency::DEFAULT_REQUEST_TIMEOUT)
                .unwrap();
            actor.child_handle().kill();
            release.send(()).unwrap();
            assert_eq!(ended(&actor.child_handle()), ExitReason::Killed);
            stopped_with(queued, &ExitReason::Killed);
            stopped_with(stored, &ExitReason::Killed);
        }

        #[test]
        fn panicked_request_and_dropped_token_wait_for_death() {
            let (server, entered, release) = gated();
            let actor = server.start();
            let mut stored = actor
                .send_request(Hold, spawned_concurrency::DEFAULT_REQUEST_TIMEOUT)
                .unwrap();
            let mut crashed = actor
                .send_request(Boom, spawned_concurrency::DEFAULT_REQUEST_TIMEOUT)
                .unwrap();
            entered.recv_timeout(LIMIT).unwrap();
            std::thread::sleep(Duration::from_millis(50));
            assert!(
                crashed.try_recv().is_none(),
                "panicked request answered before D"
            );
            assert!(
                stored.try_recv().is_none(),
                "dropped token answered before D"
            );
            release.send(()).unwrap();
            let reason = ended(&actor.child_handle());
            assert!(matches!(reason, ExitReason::Panic(_)), "{reason:?}");
            stopped_with(crashed, &reason);
            stopped_with(stored, &reason);
        }

        #[test]
        fn admitted_while_exiting_noproc_after_death() {
            let (server, entered, release) = gated();
            let actor = server.start();
            assert_eq!(actor.request(Ping).unwrap(), 7);
            actor.child_handle().stop_with_reason(ExitReason::Shutdown);
            entered.recv_timeout(LIMIT).unwrap();
            let admitted = actor
                .send_request(Ping, spawned_concurrency::DEFAULT_REQUEST_TIMEOUT)
                .expect("admitted while exiting");
            actor.send(Ping).expect("a send while exiting is admitted");
            release.send(()).unwrap();
            assert_eq!(ended(&actor.child_handle()), ExitReason::Shutdown);
            stopped_with(admitted, &ExitReason::Shutdown);
            assert!(matches!(actor.send(Ping), Err(ActorError::NoProc)));
            assert!(matches!(actor.request(Ping), Err(ActorError::NoProc)));
            assert!(matches!(
                actor.send_request(Ping, spawned_concurrency::DEFAULT_REQUEST_TIMEOUT),
                Err(ActorError::NoProc)
            ));
        }
    }

    // Context::exit_pending in threads mode: the start_linked parent's exit
    // to a trapping child is pending in the handlers of messages queued
    // before it.
    mod exit_pending {
        use super::*;
        use spawned_concurrency::threads::ActorRef;
        use std::sync::mpsc;

        type Log = Arc<Mutex<Vec<(&'static str, bool)>>>;
        pub struct Late(Log);
        impl Actor for Late {
            fn started(&mut self, ctx: &Context<Self>) {
                ctx.trap_exit(true);
            }
            fn stopped(&mut self, ctx: &Context<Self>) {
                self.0.lock().unwrap().push(("stopped", ctx.exit_pending()));
            }
        }
        struct Probe;
        impl Message for Probe {
            type Result = ();
        }
        impl Handler<Probe> for Late {
            fn handle(
                &mut self,
                _: Probe,
                reply: spawned_concurrency::reply::PendingReply<'_, ()>,
                ctx: &Context<Self>,
            ) {
                self.0.lock().unwrap().push(("probe", ctx.exit_pending()));
                reply.reply(());
            }
        }
        struct Block(mpsc::Receiver<()>);
        impl Message for Block {
            type Result = ();
        }
        impl Handler<Block> for Late {
            fn handle(
                &mut self,
                msg: Block,
                reply: spawned_concurrency::reply::PendingReply<'_, ()>,
                ctx: &Context<Self>,
            ) {
                let _ = msg.0.recv();
                self.0.lock().unwrap().push(("block", ctx.exit_pending()));
                reply.reply(());
            }
        }
        struct Parent(Log);
        impl Actor for Parent {}
        struct Spawn;
        impl Message for Spawn {
            type Result = ActorRef<Late>;
        }
        impl Handler<Spawn> for Parent {
            fn handle(
                &mut self,
                _: Spawn,
                reply: spawned_concurrency::reply::PendingReply<'_, ActorRef<Late>>,
                ctx: &Context<Self>,
            ) {
                reply.reply(Late(self.0.clone()).start_linked(ctx));
            }
        }

        #[test]
        fn queued_work_sees_the_parents_pending_exit() {
            let log: Log = Arc::default();
            let parent = Parent(log.clone()).start();
            let child = parent.request(Spawn).unwrap();
            child.request(Probe).unwrap();
            let (release, gate) = mpsc::channel();
            child.send(Block(gate)).unwrap();
            child.send(Probe).unwrap();
            child.send(Probe).unwrap();
            parent.child_handle().stop_with_reason(ExitReason::Shutdown);
            // Link notices are delivered before the parent's exit is
            // published: the child's parent exit is queued now.
            assert_eq!(ended(&parent.child_handle()), ExitReason::Shutdown);
            release.send(()).unwrap();
            assert_eq!(ended(&child.child_handle()), ExitReason::Shutdown);
            assert_eq!(
                *log.lock().unwrap(),
                vec![
                    ("probe", false),
                    ("block", true),
                    ("probe", true),
                    ("probe", true),
                    ("stopped", true),
                ]
            );
        }
    }
}
