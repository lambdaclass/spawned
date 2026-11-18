//! G7: clean exits that carry a payload, OTP's `{shutdown, Term}`
//! (`ExitReason::Shutdown(Some(why))`, docs/design/semantics.md L4).
//!
//! The payload only explains the stop. Every policy treats it exactly like
//! a bare `Shutdown`: `stopped()` runs on a self-stop and is skipped on an
//! untrapped signal, a non-trapping linked peer dies with the same full
//! reason, a trapping one gets it in `Exit`, monitors get it in `Down`,
//! `join()` returns it, a Transient child is not restarted, and the metrics
//! count it as a shutdown, not a crash.
#![cfg(feature = "async")]
// Real Tokio runtimes and OS threads: not run under the simulator.
#![cfg(not(spawned_sim))]
use spawned_concurrency::{message::Message, Down, Exit, ExitReason};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

const LIMIT: Duration = Duration::from_secs(10);
const WHY: &str = "peer disconnected: TooManyPeers";

fn why() -> ExitReason {
    ExitReason::shutdown_with(WHY)
}

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
/// Stop the receiving actor with the given reason (`ctx.stop_with_reason`).
struct StopWith(ExitReason);
impl Message for StopWith {
    type Result = ();
}

mod tasks_mode {
    use super::*;
    use spawned_concurrency::supervision::{Backoff, ChildSpec, RestartMode, SupervisorConfig};
    use spawned_concurrency::system::{ActorSystem, ExecutionGroup};
    use spawned_concurrency::tasks::{Actor, ActorRef, ActorStart, Context, Handler};
    use spawned_concurrency::ChildHandle;

    struct Node(Seen, bool);
    impl Actor for Node {
        async fn started(&mut self, ctx: &Context<Self>) {
            if self.1 {
                ctx.trap_exit(true);
            }
        }
        async fn stopped(&mut self, _: &Context<Self>) {
            self.0.stopped.store(true, Ordering::SeqCst);
        }
        async fn exit_received(&mut self, exit: Exit, _: &Context<Self>) {
            self.0.exits.lock().unwrap().push(exit);
        }
    }
    impl Handler<Ping> for Node {
        async fn handle(
            &mut self,
            _: Ping,
            reply: spawned_concurrency::PendingReply<'_, u8>,
            _: &Context<Self>,
        ) {
            reply.reply(7);
        }
    }
    impl Handler<StopWith> for Node {
        async fn handle(
            &mut self,
            msg: StopWith,
            reply: spawned_concurrency::PendingReply<'_, ()>,
            ctx: &Context<Self>,
        ) {
            ctx.stop_with_reason(msg.0);
            reply.reply(());
        }
    }
    impl Handler<Down> for Node {
        async fn handle(
            &mut self,
            down: Down,
            reply: spawned_concurrency::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            self.0.downs.lock().unwrap().push(down);
            reply.reply(());
        }
    }

    async fn ended(child: &ChildHandle) -> ExitReason {
        tokio::time::timeout(LIMIT, child.wait_exit_async())
            .await
            .expect("actor did not exit")
    }
    async fn settle(actor: &ActorRef<Node>) {
        assert_eq!(actor.request(Ping).await.unwrap(), 7);
    }
    async fn wait_for_down(watcher: &ActorRef<Node>, seen: &Seen) -> Down {
        let began = Instant::now();
        while seen.downs().is_empty() {
            assert!(began.elapsed() < LIMIT, "no Down");
            settle(watcher).await;
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        seen.downs().remove(0)
    }

    /// A self-stop with a payload: `stopped()` runs, `join()` and `Down`
    /// carry the full reason, a trapping peer's `Exit` carries it, and a
    /// non-trapping linked peer dies with it (without `stopped()`).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn payload_reaches_join_down_exit_and_linked_peers() {
        let (target_seen, plain_seen, trap_seen, watch_seen) = (
            Seen::default(),
            Seen::default(),
            Seen::default(),
            Seen::default(),
        );
        let target = Node(target_seen.clone(), false).spawn();
        let plain = Node(plain_seen.clone(), false).spawn();
        let trapper = Node(trap_seen.clone(), true).spawn();
        let watcher = Node(watch_seen.clone(), false).spawn();
        settle(&trapper).await;
        let t = target.child_handle();
        plain.context().link(&t);
        trapper.context().link(&t);
        watcher.context().monitor(&t);

        target.request(StopWith(why())).await.unwrap();
        tokio::time::timeout(LIMIT, target.join()).await.unwrap();
        assert_eq!(ended(&t).await, why());
        assert_eq!(t.exit_reason().unwrap().shutdown_payload(), Some(WHY));
        assert!(target_seen.stopped(), "a self-stop skipped stopped()");

        assert_eq!(ended(&plain.child_handle()).await, why());
        assert!(!plain_seen.stopped(), "an untrapped link ran stopped()");

        settle(&trapper).await;
        assert_eq!(trap_seen.exits(), vec![Exit::new(t.id(), why())]);

        let down = wait_for_down(&watcher, &watch_seen).await;
        assert_eq!(down.reason, why());
        assert_eq!(down.reason.to_string(), format!("shutdown: {WHY}"));

        for actor in [&trapper, &watcher] {
            actor.child_handle().stop();
            assert_eq!(ended(&actor.child_handle()).await, ExitReason::Normal);
        }
    }

    /// An exit signal with a payload follows `exit_signal/2` like `Shutdown`:
    /// a non-trapping actor ends with it without `stopped()`, a trapping one
    /// receives it as `Exit`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn exit_signal_with_payload_ends_or_is_trapped() {
        let (plain_seen, trap_seen) = (Seen::default(), Seen::default());
        let plain = Node(plain_seen.clone(), false).spawn();
        let trapper = Node(trap_seen.clone(), true).spawn();
        settle(&plain).await;
        settle(&trapper).await;
        plain.child_handle().exit(why());
        assert_eq!(ended(&plain.child_handle()).await, why());
        assert!(!plain_seen.stopped());
        trapper.child_handle().exit(why());
        settle(&trapper).await;
        assert_eq!(
            trap_seen.exits(),
            vec![Exit::new(spawned_concurrency::ActorId::EXTERNAL, why())]
        );
        trapper.child_handle().stop_with_reason(why());
        assert_eq!(ended(&trapper.child_handle()).await, why());
        assert!(trap_seen.stopped());
    }

    fn config() -> SupervisorConfig {
        SupervisorConfig::default().max_restarts(5).backoff(
            Backoff::default()
                .initial(Duration::from_millis(1))
                .maximum(Duration::from_millis(8))
                .jitter(false)
                .seed(7),
        )
    }

    /// The restart table (V1) on async and native-thread groups: a payload
    /// exit is a Shutdown, so Transient and Temporary stay stopped, Permanent
    /// restarts, the supervisor records no failure and `last_exit` keeps the
    /// payload.
    async fn restart_table_treats_payload_as_shutdown(
        group: impl Fn(&ActorSystem) -> ExecutionGroup,
    ) {
        for mode in [
            RestartMode::Permanent,
            RestartMode::Transient,
            RestartMode::Temporary,
        ] {
            let system = ActorSystem::new().unwrap();
            let group = group(&system);
            let mut tree = system.supervisor(config()).unwrap();
            let child = tree
                .add(
                    ChildSpec::new("child", &group, |_| async {
                        Ok::<_, String>(Node(Seen::default(), false))
                    })
                    .restart(mode),
                )
                .unwrap();
            let supervisor = tokio::time::timeout(LIMIT, tree.start())
                .await
                .unwrap()
                .unwrap();
            let old = child.current().unwrap();
            let generation = child.snapshot().generation;
            old.request(StopWith(why())).await.unwrap();
            assert_eq!(ended(&old.child_handle()).await, why());
            let next = tokio::time::timeout(LIMIT, child.wait_for_generation(generation))
                .await
                .unwrap();
            assert_eq!(
                next.is_ok(),
                mode == RestartMode::Permanent,
                "{mode:?} after {:?}",
                why()
            );
            if next.is_err() {
                assert_eq!(child.snapshot().last_exit, Some(why()), "{mode:?}");
            }
            assert_eq!(supervisor.failure(), None, "{mode:?}");
            assert!(supervisor.shutdown(LIMIT).await.complete);
            assert!(system.shutdown(LIMIT).await.complete);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn async_group_restart_table_treats_payload_as_shutdown() {
        restart_table_treats_payload_as_shutdown(|s| s.async_group("g7", 1).unwrap()).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn thread_group_restart_table_treats_payload_as_shutdown() {
        restart_table_treats_payload_as_shutdown(|s| s.thread_group("g7", 2).unwrap()).await;
    }

    /// Counted as a shutdown, not a crash. No test in this binary panics or
    /// kills, so the abnormal counters must not move.
    #[cfg(feature = "metrics")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn metrics_count_payload_exit_as_shutdown() {
        use spawned_concurrency::metrics;
        let before = metrics::snapshot();
        let actor = Node(Seen::default(), false).spawn();
        actor.request(StopWith(why())).await.unwrap();
        assert_eq!(ended(&actor.child_handle()).await, why());
        let after = metrics::snapshot();
        assert!(after.actors_exited_shutdown_total > before.actors_exited_shutdown_total);
        assert_eq!(
            after.actors_exited_panic_total,
            before.actors_exited_panic_total
        );
        assert_eq!(
            after.actors_exited_kill_total,
            before.actors_exited_kill_total
        );
        assert_eq!(
            after.actors_exited_killed_total,
            before.actors_exited_killed_total
        );
    }
}

mod threads_mode {
    use super::*;
    use spawned_concurrency::threads::{Actor, ActorRef, ActorStart, Context, Handler};
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
            reply: spawned_concurrency::PendingReply<'_, u8>,
            _: &Context<Self>,
        ) {
            reply.reply(7);
        }
    }
    impl Handler<StopWith> for Node {
        fn handle(
            &mut self,
            msg: StopWith,
            reply: spawned_concurrency::PendingReply<'_, ()>,
            ctx: &Context<Self>,
        ) {
            ctx.stop_with_reason(msg.0);
            reply.reply(());
        }
    }
    impl Handler<Down> for Node {
        fn handle(
            &mut self,
            down: Down,
            reply: spawned_concurrency::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            self.0.downs.lock().unwrap().push(down);
            reply.reply(());
        }
    }

    fn ended(child: &ChildHandle) -> ExitReason {
        let (tx, rx) = std::sync::mpsc::channel();
        let c = child.clone();
        std::thread::spawn(move || tx.send(c.wait_exit_blocking()).unwrap());
        rx.recv_timeout(LIMIT).expect("actor did not exit")
    }
    fn settle(actor: &ActorRef<Node>) {
        assert_eq!(actor.request(Ping).unwrap(), 7);
    }

    #[test]
    fn payload_reaches_join_down_exit_and_linked_peers() {
        let (target_seen, plain_seen, trap_seen, watch_seen) = (
            Seen::default(),
            Seen::default(),
            Seen::default(),
            Seen::default(),
        );
        let target = Node(target_seen.clone(), false).spawn();
        let plain = Node(plain_seen.clone(), false).spawn();
        let trapper = Node(trap_seen.clone(), true).spawn();
        let watcher = Node(watch_seen.clone(), false).spawn();
        settle(&trapper);
        let t = target.child_handle();
        plain.context().link(&t);
        trapper.context().link(&t);
        watcher.context().monitor(&t);

        target.request(StopWith(why())).unwrap();
        target.join();
        assert_eq!(ended(&t), why());
        assert!(target_seen.stopped(), "a self-stop skipped stopped()");

        assert_eq!(ended(&plain.child_handle()), why());
        assert!(!plain_seen.stopped(), "an untrapped link ran stopped()");

        settle(&trapper);
        assert_eq!(trap_seen.exits(), vec![Exit::new(t.id(), why())]);

        let began = Instant::now();
        while watch_seen.downs().is_empty() {
            assert!(began.elapsed() < LIMIT, "no Down");
            settle(&watcher);
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(watch_seen.downs()[0].reason, why());

        for actor in [&trapper, &watcher] {
            actor.child_handle().stop();
            assert_eq!(ended(&actor.child_handle()), ExitReason::Normal);
        }
    }

    #[test]
    fn exit_signal_with_payload_ends_or_is_trapped() {
        let (plain_seen, trap_seen) = (Seen::default(), Seen::default());
        let plain = Node(plain_seen.clone(), false).spawn();
        let trapper = Node(trap_seen.clone(), true).spawn();
        settle(&plain);
        settle(&trapper);
        plain.child_handle().exit(why());
        assert_eq!(ended(&plain.child_handle()), why());
        assert!(!plain_seen.stopped());
        trapper.child_handle().exit(why());
        settle(&trapper);
        assert_eq!(
            trap_seen.exits(),
            vec![Exit::new(spawned_concurrency::ActorId::EXTERNAL, why())]
        );
        trapper.child_handle().stop_with_reason(why());
        assert_eq!(ended(&trapper.child_handle()), why());
        assert!(trap_seen.stopped());
    }
}
