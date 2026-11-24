//! Introspection tier 0 (docs/design/introspection.md; semantics.md section
//! 13): `info()` is out of band (I1), its lifecycle walks Starting, Idle,
//! InTurn, Exiting, Dead (I3: `published` exactly when `wait_exit()` is
//! ready), labels, links, monitors, parent, placement, supervisor position
//! and registered name. Both modes, every tasks placement.
#![cfg_attr(spawned_sim, allow(unused_imports, dead_code))]

use spawned_concurrency::introspection::{
    ActorInfo, Field, Lifecycle, Parent, Placement, Unavailable,
};
use spawned_concurrency::message::Message;
use spawned_concurrency::{ExitReason, PendingReply};
use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant};

const LIMIT: Duration = Duration::from_secs(5);

/// Poll `info` until `pred` holds (out-of-band reads never wait for a turn,
/// so only the actor's own progress is awaited).
fn eventually(mut info: impl FnMut() -> ActorInfo, pred: impl Fn(&ActorInfo) -> bool) -> ActorInfo {
    let until = Instant::now() + LIMIT;
    loop {
        let now = info();
        if pred(&now) {
            return now;
        }
        assert!(Instant::now() < until, "timed out; last info: {now:?}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn tier1_unavailable(info: &ActorInfo) {
    assert_eq!(info.activity, Field::Unavailable(Unavailable::Feature));
    assert_eq!(info.mailbox, Field::Unavailable(Unavailable::Feature));
}

/// A turn that blocks its thread until released (a wedged handler).
struct Wedge {
    entered: std_mpsc::Sender<()>,
    release: std_mpsc::Receiver<()>,
}
impl Message for Wedge {
    type Result = ();
}

struct Ping;
impl Message for Ping {
    type Result = ();
}

#[cfg(all(feature = "async", not(spawned_sim)))]
mod tasks_mode {
    use super::*;
    use spawned_concurrency::monitor::Down;
    use spawned_concurrency::supervision::{ChildSpec, SupervisorConfig};
    use spawned_concurrency::system::{ActorSystem, ExecutionGroup};
    use spawned_concurrency::tasks::{Actor, ActorRef, ActorStart, Backend, Context, Handler};
    use spawned_concurrency::ActorId;
    use spawned_concurrency::ReplyTo;
    use tokio::sync::oneshot;

    #[derive(Clone, Copy, Debug)]
    enum Place {
        Async,
        Blocking,
        Thread,
        AsyncGroup,
        ThreadGroup,
    }
    const PLACES: [Place; 5] = [
        Place::Async,
        Place::Blocking,
        Place::Thread,
        Place::AsyncGroup,
        Place::ThreadGroup,
    ];
    struct Placement_ {
        system: ActorSystem,
        async_group: ExecutionGroup,
        thread_group: ExecutionGroup,
    }
    impl Placement_ {
        fn new() -> Self {
            let system = ActorSystem::new().unwrap();
            let async_group = system.async_group("intro-async", 2).unwrap();
            let thread_group = system.thread_group("intro-threads", 16).unwrap();
            Self {
                system,
                async_group,
                thread_group,
            }
        }
        fn start<A: Actor>(&self, actor: A, place: Place) -> ActorRef<A> {
            match place {
                Place::Async => actor.with_options(Backend::Async).spawn().unwrap(),
                Place::Blocking => actor.with_options(Backend::Blocking).spawn().unwrap(),
                Place::Thread => actor.with_options(Backend::Thread).spawn().unwrap(),
                Place::AsyncGroup => actor.with_options(&self.async_group).spawn().unwrap(),
                Place::ThreadGroup => actor.with_options(&self.thread_group).spawn().unwrap(),
            }
        }
        fn expected(&self, place: Place) -> Placement {
            match place {
                Place::Async => Placement::Async,
                Place::Blocking => Placement::Blocking,
                Place::Thread => Placement::Thread,
                Place::AsyncGroup => Placement::Group {
                    name: "intro-async".into(),
                },
                Place::ThreadGroup => Placement::Group {
                    name: "intro-threads".into(),
                },
            }
        }
        async fn finish(self) {
            assert!(self.system.shutdown(LIMIT).await.complete);
        }
    }

    #[derive(Default)]
    struct Worker {
        deferred: Option<ReplyTo<()>>,
    }
    impl Actor for Worker {}
    impl Handler<Wedge> for Worker {
        async fn handle(&mut self, msg: Wedge, reply: PendingReply<'_, ()>, _: &Context<Self>) {
            msg.entered.send(()).unwrap();
            // Blocks this turn's thread: no await point until released.
            let _ = msg.release.recv_timeout(LIMIT);
            reply.reply(());
        }
    }
    impl Handler<Ping> for Worker {
        async fn handle(&mut self, _: Ping, reply: PendingReply<'_, ()>, _: &Context<Self>) {
            reply.reply(());
        }
    }
    /// Defers its reply and keeps the token.
    struct Defer;
    impl Message for Defer {
        type Result = ();
    }
    impl Handler<Defer> for Worker {
        async fn handle(&mut self, _: Defer, reply: PendingReply<'_, ()>, _: &Context<Self>) {
            self.deferred = Some(reply.defer().unwrap());
        }
    }
    impl Handler<Down> for Worker {
        async fn handle(&mut self, _: Down, reply: PendingReply<'_, ()>, _: &Context<Self>) {
            reply.reply(());
        }
    }

    /// I1 on every placement: `info()` answers while a handler blocks its
    /// thread, and reports the turn.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn info_returns_during_a_wedged_turn() {
        let placement = Placement_::new();
        for place in PLACES {
            let actor = placement.start(Worker::default(), place);
            let (entered_tx, entered) = std_mpsc::channel();
            let (release_tx, release) = std_mpsc::channel();
            actor
                .send(Wedge {
                    entered: entered_tx,
                    release,
                })
                .unwrap();
            entered.recv_timeout(LIMIT).unwrap();
            let began = Instant::now();
            let info = actor.info();
            assert!(began.elapsed() < Duration::from_secs(1), "{place:?}");
            assert_eq!(info.lifecycle, Lifecycle::InTurn, "{place:?}");
            assert_eq!(info.placement, placement.expected(place), "{place:?}");
            assert_eq!(actor.child_handle().info().lifecycle, Lifecycle::InTurn);
            tier1_unavailable(&info);
            release_tx.send(()).unwrap();
            let actor2 = actor.clone();
            eventually(move || actor2.info(), |i| i.lifecycle == Lifecycle::Idle);
            actor.child_handle().stop();
            actor.join().await;
        }
        placement.finish().await;
    }

    /// The tier 0 lifecycle, in order: Starting, Idle, InTurn, Exiting,
    /// Dead { published: false } while the state's destructor runs (after D,
    /// before publication: sends are `NoProc`, `wait_exit` not ready), then
    /// Dead { published: true }.
    struct Phases {
        start: Option<oneshot::Receiver<()>>,
        stop: Option<oneshot::Receiver<()>>,
        drop_gate: Option<std_mpsc::Receiver<()>>,
    }
    impl Actor for Phases {
        async fn started(&mut self, _: &Context<Self>) {
            let _ = self.start.take().unwrap().await;
        }
        async fn stopped(&mut self, _: &Context<Self>) {
            let _ = self.stop.take().unwrap().await;
        }
    }
    impl Drop for Phases {
        fn drop(&mut self) {
            if let Some(gate) = self.drop_gate.take() {
                let _ = gate.recv_timeout(LIMIT);
            }
        }
    }
    impl Handler<Wedge> for Phases {
        async fn handle(&mut self, msg: Wedge, reply: PendingReply<'_, ()>, _: &Context<Self>) {
            msg.entered.send(()).unwrap();
            let _ = msg.release.recv_timeout(LIMIT);
            reply.reply(());
        }
    }
    impl Handler<Ping> for Phases {
        async fn handle(&mut self, _: Ping, reply: PendingReply<'_, ()>, _: &Context<Self>) {
            reply.reply(());
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn lifecycle_walks_starting_idle_in_turn_exiting_dead() {
        let placement = Placement_::new();
        for place in PLACES {
            let (start_tx, start) = oneshot::channel();
            let (stop_tx, stop) = oneshot::channel();
            let (drop_tx, drop_gate) = std_mpsc::channel();
            let actor = placement.start(
                Phases {
                    start: Some(start),
                    stop: Some(stop),
                    drop_gate: Some(drop_gate),
                },
                place,
            );
            let a = actor.clone();
            eventually(move || a.info(), |i| i.lifecycle == Lifecycle::Starting);
            start_tx.send(()).unwrap();
            let a = actor.clone();
            eventually(move || a.info(), |i| i.lifecycle == Lifecycle::Idle);
            let (entered_tx, entered) = std_mpsc::channel();
            let (release_tx, release) = std_mpsc::channel();
            actor
                .send(Wedge {
                    entered: entered_tx,
                    release,
                })
                .unwrap();
            entered.recv_timeout(LIMIT).unwrap();
            assert_eq!(actor.info().lifecycle, Lifecycle::InTurn, "{place:?}");
            release_tx.send(()).unwrap();
            actor.child_handle().stop();
            let a = actor.clone();
            eventually(move || a.info(), |i| i.lifecycle == Lifecycle::Exiting);
            stop_tx.send(()).unwrap();
            let a = actor.clone();
            let dead = eventually(
                move || a.info(),
                |i| matches!(i.lifecycle, Lifecycle::Dead { .. }),
            );
            assert_eq!(
                dead.lifecycle,
                Lifecycle::Dead {
                    reason: ExitReason::Normal,
                    published: false
                },
                "{place:?}"
            );
            // Between D and publication: NoProc, wait_exit not ready.
            assert!(actor.send(Ping).is_err());
            assert!(actor.exit_reason().is_none());
            drop_tx.send(()).unwrap();
            assert_eq!(actor.wait_exit().await, ExitReason::Normal);
            assert_eq!(
                actor.info().lifecycle,
                Lifecycle::Dead {
                    reason: ExitReason::Normal,
                    published: true
                }
            );
            actor.join().await;
        }
        placement.finish().await;
    }

    /// Labels, type, parent, trap_exit, links, monitors, deferred replies.
    struct Linker {
        child: Option<ActorRef<Worker>>,
    }
    impl Actor for Linker {}
    struct Setup {
        peer: spawned_concurrency::ChildHandle,
    }
    impl Message for Setup {
        type Result = ActorId;
    }
    impl Handler<Setup> for Linker {
        async fn handle(
            &mut self,
            msg: Setup,
            reply: PendingReply<'_, ActorId>,
            ctx: &Context<Self>,
        ) {
            ctx.set_label("peer 0x3a1f");
            ctx.trap_exit(true);
            ctx.link(&msg.peer);
            ctx.monitor(&msg.peer);
            ctx.monitor(&msg.peer);
            let child = Worker::default().start_link(ctx).actor_ref().unwrap();
            let id = child.id();
            self.child = Some(child);
            assert_eq!(ctx.info().lifecycle, Lifecycle::InTurn);
            reply.reply(id);
        }
    }
    impl Handler<Down> for Linker {
        async fn handle(&mut self, _: Down, reply: PendingReply<'_, ()>, _: &Context<Self>) {
            reply.reply(());
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn links_monitors_trap_exit_parent_and_label() {
        let peer = Worker::default().spawn();
        let linker = Linker { child: None }.spawn();
        let before = linker.info();
        assert_eq!(before.label, None);
        assert!(before.actor_type.ends_with("Linker"));
        assert_eq!(before.parent, Parent::None);
        assert_eq!(before.system, None);
        assert_eq!(before.supervisor, None);
        assert_eq!(before.name, None);
        let child_id = linker
            .request(Setup {
                peer: peer.child_handle(),
            })
            .await
            .unwrap();
        let info = linker.info();
        assert_eq!(info.label.as_deref(), Some("peer 0x3a1f"));
        assert!(info.trap_exit);
        assert_eq!(info.links, 2);
        assert_eq!(info.monitors, 2);
        let handle = linker.child_handle();
        let mut expected = vec![peer.id(), child_id];
        expected.sort_by_key(|id| id.to_string().parse::<u64>().unwrap());
        assert_eq!(handle.links(), expected);
        assert_eq!(handle.monitors(), vec![peer.id(), peer.id()]);
        assert_eq!(peer.child_handle().links(), vec![linker.id()]);
        // Peer death: the link and monitors go.
        peer.child_handle().stop();
        peer.join().await;
        let l = linker.child_handle();
        eventually(move || l.info(), |i| i.links == 1 && i.monitors == 0);
        assert!(linker.child_handle().monitors().is_empty());
        linker.child_handle().stop();
        linker.join().await;
    }

    #[tokio::test]
    async fn start_linked_child_names_its_parent_and_deferred_replies_count() {
        struct Parent_;
        impl Actor for Parent_ {}
        struct Spawn;
        impl Message for Spawn {
            type Result = ActorRef<Worker>;
        }
        impl Handler<Spawn> for Parent_ {
            async fn handle(
                &mut self,
                _: Spawn,
                reply: PendingReply<'_, ActorRef<Worker>>,
                ctx: &Context<Self>,
            ) {
                reply.reply(Worker::default().start_link(ctx).actor_ref().unwrap());
            }
        }
        let parent = Parent_.spawn();
        let child = parent.request(Spawn).await.unwrap();
        assert_eq!(child.info().parent, Parent::Actor(parent.id()));
        let _rx = child
            .send_request(Defer, spawned_concurrency::Deadline::Never)
            .unwrap();
        child.request(Ping).await.unwrap();
        assert_eq!(child.info().deferred_outstanding, 1);
        assert_eq!(child.info().jobs_running, 0);
        // A parent's Normal exit does not end a non-trapping child (OTP).
        parent.child_handle().stop();
        parent.join().await;
        child.child_handle().stop();
        child.join().await;
    }

    /// A handle outlives every `ActorRef`: `ChildHandle::info` still answers
    /// (dead, published, with its type and label).
    #[tokio::test]
    async fn child_handle_info_after_every_ref_is_dropped() {
        struct Labelled;
        impl Actor for Labelled {
            async fn started(&mut self, ctx: &Context<Self>) {
                ctx.set_label("gone");
            }
        }
        let actor = Labelled.spawn();
        let handle = actor.child_handle();
        actor.child_handle().stop();
        actor.join().await;
        drop(actor);
        let info = handle.info();
        assert_eq!(
            info.lifecycle,
            Lifecycle::Dead {
                reason: ExitReason::Normal,
                published: true
            }
        );
        assert!(info.actor_type.ends_with("Labelled"));
        assert_eq!(info.label.as_deref(), Some("gone"));
        assert!(handle.links().is_empty() && handle.monitors().is_empty());
    }

    /// Managed actors: system id, group placement, registered name,
    /// supervisor position (generation and restarts across a restart).
    struct Named;
    impl Actor for Named {
        async fn started(&mut self, ctx: &Context<Self>) {
            ctx.register_name("introspection-named").unwrap();
        }
    }
    struct Crash;
    impl Message for Crash {
        type Result = ();
    }
    impl Handler<Crash> for Named {
        async fn handle(&mut self, _: Crash, _: PendingReply<'_, ()>, _: &Context<Self>) {
            panic!("crash for a restart");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn supervised_position_system_and_name() {
        let system = ActorSystem::new().unwrap();
        let group = system.async_group("intro-sup", 2).unwrap();
        let mut builder = system.supervisor(SupervisorConfig::default()).unwrap();
        let child = builder
            .add(ChildSpec::new("named", &group, |_| async {
                Ok::<_, String>(Named)
            }))
            .unwrap();
        let _sup = builder.start().await.unwrap();
        let first = child.wait_ready().await.unwrap();
        let info = first.info();
        assert_eq!(info.system, Some(system.id()));
        assert_eq!(info.parent, Parent::System);
        assert_eq!(
            info.placement,
            Placement::Group {
                name: "intro-sup".into()
            }
        );
        assert_eq!(info.name.as_deref(), Some("introspection-named"));
        let position = info.supervisor.unwrap();
        assert_eq!(
            (
                position.child.as_str(),
                position.generation,
                position.restarts
            ),
            ("named", 1, 0)
        );
        first.send(Crash).unwrap();
        let second = child.wait_for_generation(1).await.unwrap();
        let position = second.info().supervisor.unwrap();
        assert_eq!((position.generation, position.restarts), (2, 1));
        // The first instance stays dead with its own position.
        assert!(matches!(first.info().lifecycle, Lifecycle::Dead { .. }));
        assert_eq!(first.info().supervisor.unwrap().generation, 1);
        assert!(system.shutdown(LIMIT).await.complete);
    }
}

#[cfg(not(spawned_sim))]
mod threads_mode {
    use super::*;
    use spawned_concurrency::monitor::Down;
    use spawned_concurrency::threads::{Actor, ActorRef, ActorStart, Context, Handler};

    struct Worker;
    impl Actor for Worker {}
    impl Handler<Wedge> for Worker {
        fn handle(&mut self, msg: Wedge, reply: PendingReply<'_, ()>, _: &Context<Self>) {
            msg.entered.send(()).unwrap();
            let _ = msg.release.recv_timeout(LIMIT);
            reply.reply(());
        }
    }
    impl Handler<Ping> for Worker {
        fn handle(&mut self, _: Ping, reply: PendingReply<'_, ()>, _: &Context<Self>) {
            reply.reply(());
        }
    }
    impl Handler<Down> for Worker {
        fn handle(&mut self, _: Down, reply: PendingReply<'_, ()>, _: &Context<Self>) {
            reply.reply(());
        }
    }

    #[test]
    fn info_returns_during_a_wedged_turn() {
        let actor = Worker.spawn();
        let (entered_tx, entered) = std_mpsc::channel();
        let (release_tx, release) = std_mpsc::channel();
        actor
            .send(Wedge {
                entered: entered_tx,
                release,
            })
            .unwrap();
        entered.recv_timeout(LIMIT).unwrap();
        let info = actor.info();
        assert_eq!(info.lifecycle, Lifecycle::InTurn);
        assert_eq!(info.placement, Placement::Native);
        assert_eq!(info.parent, Parent::None);
        tier1_unavailable(&info);
        release_tx.send(()).unwrap();
        let a = actor.clone();
        eventually(move || a.info(), |i| i.lifecycle == Lifecycle::Idle);
        actor.child_handle().stop();
        actor.join();
    }

    struct Phases {
        start: std_mpsc::Receiver<()>,
        stop: std_mpsc::Receiver<()>,
        drop_gate: Option<std_mpsc::Receiver<()>>,
    }
    impl Actor for Phases {
        fn started(&mut self, _: &Context<Self>) {
            let _ = self.start.recv_timeout(LIMIT);
        }
        fn stopped(&mut self, _: &Context<Self>) {
            let _ = self.stop.recv_timeout(LIMIT);
        }
    }
    impl Drop for Phases {
        fn drop(&mut self) {
            if let Some(gate) = self.drop_gate.take() {
                let _ = gate.recv_timeout(LIMIT);
            }
        }
    }
    impl Handler<Ping> for Phases {
        fn handle(&mut self, _: Ping, reply: PendingReply<'_, ()>, _: &Context<Self>) {
            reply.reply(());
        }
    }

    #[test]
    fn lifecycle_walks_starting_idle_exiting_dead() {
        let (start_tx, start) = std_mpsc::channel();
        let (stop_tx, stop) = std_mpsc::channel();
        let (drop_tx, drop_gate) = std_mpsc::channel();
        let actor = Phases {
            start,
            stop,
            drop_gate: Some(drop_gate),
        }
        .spawn();
        assert_eq!(actor.info().lifecycle, Lifecycle::Starting);
        start_tx.send(()).unwrap();
        let a = actor.clone();
        eventually(move || a.info(), |i| i.lifecycle == Lifecycle::Idle);
        actor.child_handle().stop();
        let a = actor.clone();
        eventually(move || a.info(), |i| i.lifecycle == Lifecycle::Exiting);
        stop_tx.send(()).unwrap();
        let a = actor.clone();
        let dead = eventually(
            move || a.info(),
            |i| matches!(i.lifecycle, Lifecycle::Dead { .. }),
        );
        assert_eq!(
            dead.lifecycle,
            Lifecycle::Dead {
                reason: ExitReason::Normal,
                published: false
            }
        );
        assert!(actor.send(Ping).is_err());
        assert!(actor.exit_reason().is_none());
        drop_tx.send(()).unwrap();
        assert_eq!(actor.wait_exit(), ExitReason::Normal);
        assert_eq!(
            actor.child_handle().info().lifecycle,
            Lifecycle::Dead {
                reason: ExitReason::Normal,
                published: true
            }
        );
    }

    struct Linker;
    impl Actor for Linker {}
    struct Setup(spawned_concurrency::ChildHandle);
    impl Message for Setup {
        type Result = ActorRef<Worker>;
    }
    impl Handler<Setup> for Linker {
        fn handle(
            &mut self,
            msg: Setup,
            reply: PendingReply<'_, ActorRef<Worker>>,
            ctx: &Context<Self>,
        ) {
            ctx.set_label("chain");
            ctx.trap_exit(true);
            ctx.link(&msg.0);
            ctx.monitor(&msg.0);
            assert_eq!(ctx.info().lifecycle, Lifecycle::InTurn);
            reply.reply(Worker.start_link(ctx).unwrap());
        }
    }
    impl Handler<Down> for Linker {
        fn handle(&mut self, _: Down, reply: PendingReply<'_, ()>, _: &Context<Self>) {
            reply.reply(());
        }
    }

    #[test]
    fn links_monitors_trap_exit_parent_and_label() {
        let peer = Worker.spawn();
        let linker = Linker.spawn();
        let child = linker.request(Setup(peer.child_handle())).unwrap();
        let info = linker.info();
        assert_eq!(info.label.as_deref(), Some("chain"));
        assert!(info.actor_type.ends_with("Linker"));
        assert!(info.trap_exit);
        assert_eq!(info.links, 2);
        assert_eq!(info.monitors, 1);
        assert_eq!(linker.child_handle().monitors(), vec![peer.id()]);
        assert_eq!(child.info().parent, Parent::Actor(linker.id()));
        peer.child_handle().stop();
        peer.join();
        let l = linker.child_handle();
        eventually(move || l.info(), |i| i.links == 1 && i.monitors == 0);
        linker.child_handle().stop();
        linker.join();
        child.child_handle().stop();
        child.join();
    }
}

/// I1 and I12 (reads part) under the simulator: `info()`, `links()` and
/// `monitors()` take no scheduling decision, register no waker and draw no
/// randomness, so a seed's decision tape and schedule are identical with and
/// without them.
#[cfg(all(spawned_sim, feature = "async"))]
mod sim_mode {
    use super::*;
    use spawned_concurrency::monitor::Down;
    use spawned_concurrency::tasks::{Actor, ActorRef, ActorStart, Context, Handler};
    use spawned_rt::tasks::sim;

    struct Node {
        peer: Option<ActorRef<Node>>,
    }
    impl Actor for Node {}
    impl Handler<Ping> for Node {
        async fn handle(&mut self, _: Ping, reply: PendingReply<'_, ()>, _: &Context<Self>) {
            spawned_rt::tasks::sleep(Duration::from_millis(1)).await;
            if let Some(peer) = &self.peer {
                let _ = peer.send(Ping);
            }
            reply.reply(());
        }
    }
    impl Handler<Down> for Node {
        async fn handle(&mut self, _: Down, reply: PendingReply<'_, ()>, _: &Context<Self>) {
            reply.reply(());
        }
    }

    async fn body(read: bool) {
        let a = Node { peer: None }.spawn();
        let b = Node {
            peer: Some(a.clone()),
        }
        .spawn();
        b.context().link(&a.child_handle());
        b.context().monitor(&a.child_handle());
        let read_all = |phase: &str| {
            if read {
                for actor in [&a, &b] {
                    let info = actor.info();
                    assert!(!info.actor_type.is_empty(), "{phase}");
                    let handle = actor.child_handle();
                    let _ = (handle.info(), handle.links(), handle.monitors());
                }
            }
        };
        for _ in 0..4 {
            read_all("running");
            let _ = b.send(Ping);
            spawned_rt::tasks::yield_now().await;
            read_all("yielded");
            b.request(Ping).await.unwrap();
        }
        b.child_handle().stop();
        read_all("stopping");
        b.join().await;
        a.child_handle().stop();
        a.join().await;
        read_all("dead");
    }

    #[test]
    fn reads_do_not_change_the_tape() {
        for seed in 0..16 {
            let without = sim::run_once(seed, None, &|_| body(false));
            let with = sim::run_once(seed, None, &|_| body(true));
            assert_eq!(without.failure, None, "seed {seed}");
            assert_eq!(with.failure, None, "seed {seed}");
            assert_eq!(without.tape, with.tape, "seed {seed}");
            assert_eq!(
                without.stats.schedule_hash, with.stats.schedule_hash,
                "seed {seed}"
            );
        }
    }
}

/// The lifecycle reaches `Dead { published: true }` with the final reason:
/// once on Tokio, and over seeded schedules under the simulator.
#[cfg(feature = "async")]
mod seeded {
    use super::*;
    use spawned_concurrency::tasks::{Actor, ActorStart, Context, Handler};

    struct Echo;
    impl Actor for Echo {}
    impl Handler<Ping> for Echo {
        async fn handle(&mut self, _: Ping, reply: PendingReply<'_, ()>, _: &Context<Self>) {
            reply.reply(());
        }
    }

    #[spawned_concurrency::sim_test]
    async fn idle_then_dead_and_published_after_wait_exit() {
        let a = Echo.spawn();
        a.request(Ping).await.unwrap();
        assert_eq!(a.info().lifecycle, Lifecycle::Idle);
        a.child_handle().stop();
        assert_eq!(a.wait_exit().await, ExitReason::Normal);
        assert_eq!(
            a.info().lifecycle,
            Lifecycle::Dead {
                reason: ExitReason::Normal,
                published: true
            }
        );
    }
}
