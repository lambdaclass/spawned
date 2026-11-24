//! The unified start contract (docs/design/unified-start.md; semantics.md
//! L1, L2, L8-L10, X5, X8, S6, S7, S9, T5) in both modes. Each test fails
//! when the guarantee it names breaks: a refused start that ran a hook or
//! kept a name, a failed start that ran `stopped()` or answered before its
//! state was dropped, a consumed notice that still arrived, a lost one, a
//! continuation behind a message, a child outside its parent's group.

#![cfg_attr(not(feature = "async"), allow(unused_imports))]
#![cfg_attr(spawned_sim, allow(dead_code, unused_imports))]

use spawned_concurrency::{
    message::Message, ChildHandle, Down, Exit, ExitReason, PendingReply, StartError, StartOutcome,
};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const LIMIT: Duration = Duration::from_secs(10);

#[derive(Default)]
struct Hooks {
    started: AtomicUsize,
    stopped: AtomicUsize,
    dropped: AtomicUsize,
}

struct Work;
impl Message for Work {
    type Result = ();
}
struct Cont;
impl Message for Cont {
    type Result = ();
}
struct Boom;
impl Message for Boom {
    type Result = ();
}

#[derive(Clone, Copy, Debug)]
enum Then {
    Ready,
    Continue,
    Error,
    Ignore,
    Panic,
    Stop,
    CtxStop,
    PanicLater,
}

#[cfg(feature = "async")]
mod tasks_mode {
    use super::*;
    use spawned_concurrency::system::ActorSystem;
    use spawned_concurrency::tasks::{
        Actor, ActorRef, ActorStart, Backend, Context, Handler, Init, Name, StartOptions,
    };

    struct Child {
        then: Then,
        hooks: Arc<Hooks>,
        log: Arc<Mutex<Vec<&'static str>>>,
        gate: Option<tokio::sync::oneshot::Receiver<()>>,
        group: Arc<Mutex<Option<Option<String>>>>,
    }
    impl Child {
        fn new(then: Then) -> Self {
            Self {
                then,
                hooks: Arc::default(),
                log: Arc::default(),
                gate: None,
                group: Arc::default(),
            }
        }
    }
    impl Drop for Child {
        fn drop(&mut self) {
            self.hooks.dropped.fetch_add(1, Ordering::SeqCst);
        }
    }
    impl Actor for Child {
        async fn started(&mut self, ctx: &Context<Self>) -> Init<Self> {
            self.hooks.started.fetch_add(1, Ordering::SeqCst);
            *self.group.lock().unwrap() = Some(ctx.execution_group().and_then(|g| g.name()));
            if let Some(gate) = self.gate.take() {
                let _ = gate.await;
            }
            match self.then {
                Then::Ready => Init::ready(),
                Then::Continue => Init::continue_with(Cont),
                Then::Error => Init::error("bad config"),
                Then::Ignore => Init::ignore(),
                Then::Panic => panic!("init panic"),
                Then::Stop => Init::stop(ExitReason::Shutdown(None)),
                Then::CtxStop => {
                    ctx.stop();
                    Init::ready()
                }
                Then::PanicLater => Init::continue_with(Boom),
            }
        }
        async fn stopped(&mut self, _: &Context<Self>) {
            self.hooks.stopped.fetch_add(1, Ordering::SeqCst);
        }
    }
    impl Handler<Work> for Child {
        async fn handle(&mut self, _: Work, reply: PendingReply<'_, ()>, _: &Context<Self>) {
            self.log.lock().unwrap().push("work");
            reply.reply(());
        }
    }
    impl Handler<Cont> for Child {
        async fn handle(&mut self, _: Cont, _: PendingReply<'_, ()>, _: &Context<Self>) {
            self.log.lock().unwrap().push("continue");
        }
    }
    impl Handler<Boom> for Child {
        async fn handle(&mut self, _: Boom, _: PendingReply<'_, ()>, _: &Context<Self>) {
            panic!("failure right after readiness");
        }
    }

    #[derive(Default, Clone)]
    struct Seen {
        exits: Arc<Mutex<Vec<Exit>>>,
        downs: Arc<Mutex<Vec<Down>>>,
    }
    struct Parent {
        trap: bool,
        seen: Seen,
    }
    impl Actor for Parent {
        async fn started(&mut self, ctx: &Context<Self>) {
            ctx.trap_exit(self.trap);
        }
        async fn exit_received(&mut self, exit: Exit, _: &Context<Self>) {
            self.seen.exits.lock().unwrap().push(exit);
        }
    }
    impl Handler<Down> for Parent {
        async fn handle(&mut self, down: Down, reply: PendingReply<'_, ()>, _: &Context<Self>) {
            self.seen.downs.lock().unwrap().push(down);
            reply.reply(());
        }
    }
    impl Handler<Work> for Parent {
        async fn handle(&mut self, _: Work, reply: PendingReply<'_, ()>, _: &Context<Self>) {
            reply.reply(());
        }
    }
    async fn parent(trap: bool) -> (ActorRef<Parent>, Seen) {
        let seen = Seen::default();
        let p = Parent {
            trap,
            seen: seen.clone(),
        }
        .start()
        .await
        .unwrap();
        (p, seen)
    }

    /// L9: every row of the 2.3 table, waiting and through spawn + ready();
    /// no failed start runs stopped(), and its state is dropped before the
    /// error is answered (L8, failure after publication).
    #[cfg(not(spawned_sim))] // real Tokio
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn init_outcomes_answer_after_publication_without_stopped() {
        for then in [
            Then::Ready,
            Then::Continue,
            Then::Error,
            Then::Ignore,
            Then::Panic,
            Then::Stop,
            Then::CtxStop,
        ] {
            for waiting in [true, false] {
                let child = Child::new(then);
                let hooks = child.hooks.clone();
                let raw = if waiting {
                    child.start().await.map(|_| ())
                } else {
                    child.spawn().ready().await
                };
                // The verbs and ready() answer ignore as Err(Ignored);
                // from_result gives OTP's three-way split.
                assert_eq!(
                    matches!(raw, Err(StartError::Ignored { pending: None })),
                    matches!(then, Then::Ignore),
                    "{then:?}"
                );
                let result = StartOutcome::from_result(raw);
                let failed = !matches!(result, Ok(StartOutcome::Started(())));
                if failed {
                    // Answered after publication: the state is already gone.
                    assert_eq!(hooks.dropped.load(Ordering::SeqCst), 1, "{then:?}");
                }
                assert_eq!(hooks.stopped.load(Ordering::SeqCst), 0, "{then:?}");
                match (then, result) {
                    (Then::Ready | Then::Continue, Ok(StartOutcome::Started(()))) => {}
                    (
                        Then::Error,
                        Err(StartError::InitFailed {
                            cause,
                            pending: None,
                        }),
                    ) => {
                        assert_eq!(cause, "bad config")
                    }
                    (Then::Ignore, Ok(StartOutcome::Ignored { pending: None })) => {}
                    (
                        Then::Panic,
                        Err(StartError::InitExited {
                            reason: ExitReason::Panic(_),
                            pending: None,
                        }),
                    ) => {}
                    (
                        Then::Stop,
                        Err(StartError::InitExited {
                            reason: ExitReason::Shutdown(None),
                            pending: None,
                        }),
                    ) => {}
                    (
                        Then::CtxStop,
                        Err(StartError::InitExited {
                            reason: ExitReason::Normal,
                            pending: None,
                        }),
                    ) => {}
                    (then, result) => panic!("{then:?} (waiting {waiting}): {result:?}"),
                }
            }
        }
    }

    /// L2/L8: readiness is final. A child that fails right after readiness,
    /// with its waiter not yet polled, still answers Started, and the Down
    /// of its later death reaches the monitoring parent.
    #[cfg(not(spawned_sim))] // real Tokio
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ready_then_immediate_crash_with_unpolled_waiter_returns_started_and_delivers_down() {
        let (p, seen) = parent(false).await;
        let starting = Child::new(Then::PanicLater).start_monitor(&p.context());
        let child = starting.actor_ref().unwrap();
        assert!(matches!(child.wait_exit().await, ExitReason::Panic(_)));
        let (_, monitor) = starting.await.unwrap();
        tokio::time::timeout(LIMIT, async {
            while seen.downs.lock().unwrap().is_empty() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        let down = seen.downs.lock().unwrap()[0].clone();
        assert_eq!(down.monitor_ref, monitor);
        assert!(matches!(down.reason, ExitReason::Panic(_)));
    }

    /// X8: a failed start's notice is consumed for a trapping parent (Exit)
    /// and a monitor (Down); a non-trapping parent still exits on a
    /// non-Normal failure, and a Normal one spares it.
    #[cfg(not(spawned_sim))] // real Tokio
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn failed_start_notices_follow_the_disposition_table() {
        let (p, seen) = parent(true).await;
        assert!(Child::new(Then::Panic)
            .start_link(&p.context())
            .await
            .is_err());
        assert!(Child::new(Then::Error)
            .start_monitor(&p.context())
            .await
            .is_err());
        p.request(Work).await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(seen.exits.lock().unwrap().is_empty(), "Exit not consumed");
        assert!(seen.downs.lock().unwrap().is_empty(), "Down not consumed");

        let (spared, _) = parent(false).await;
        assert!(Child::new(Then::Error)
            .start_link(&spared.context())
            .await
            .is_err());
        spared.request(Work).await.unwrap();
        assert!(spared.child_handle().is_alive());

        let (ended, _) = parent(false).await;
        assert!(Child::new(Then::Panic)
            .start_link(&ended.context())
            .await
            .is_err());
        assert!(matches!(ended.wait_exit().await, ExitReason::Panic(_)));
    }

    /// X8 abandonment: a dropped Starting hands its diverted notice on.
    #[cfg(not(spawned_sim))] // real Tokio
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn abandoned_start_monitor_still_delivers_down() {
        let (p, seen) = parent(false).await;
        let starting = Child::new(Then::Error).start_monitor(&p.context());
        let child = starting.actor_ref().unwrap();
        drop(starting);
        child.wait_exit().await;
        tokio::time::timeout(LIMIT, async {
            while seen.downs.lock().unwrap().is_empty() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(seen.downs.lock().unwrap().len(), 1);
    }

    /// T5: the init timeout answers InitTimeout and never reaches a waiting
    /// parent (trapping or not).
    #[cfg(not(spawned_sim))] // real Tokio
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn init_timeout_never_reaches_the_parent() {
        for trap in [true, false] {
            let (p, seen) = parent(trap).await;
            let (release, gate) = tokio::sync::oneshot::channel();
            let mut child = Child::new(Then::Ready);
            child.gate = Some(gate);
            let hooks = child.hooks.clone();
            let starting = child
                .with_options(StartOptions::new().init_timeout(Duration::from_millis(20)))
                .start_link(&p.context());
            tokio::time::sleep(Duration::from_millis(60)).await;
            release.send(()).unwrap();
            assert!(matches!(
                starting.await,
                Err(StartError::InitTimeout { pending: None })
            ));
            assert_eq!(hooks.stopped.load(Ordering::SeqCst), 0);
            p.request(Work).await.unwrap();
            assert!(seen.exits.lock().unwrap().is_empty());
            assert!(
                p.child_handle().is_alive(),
                "the timeout's Killed reached P"
            );
        }
    }

    /// L10: the continuation runs before messages sent during started().
    #[cfg(not(spawned_sim))] // real Tokio
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn continuation_runs_before_messages_sent_during_started() {
        let (release, gate) = tokio::sync::oneshot::channel();
        let mut child = Child::new(Then::Continue);
        child.gate = Some(gate);
        let log = child.log.clone();
        let child = child.spawn();
        child.send(Work).unwrap();
        release.send(()).unwrap();
        child.request(Work).await.unwrap();
        assert_eq!(*log.lock().unwrap(), ["continue", "work", "work"]);
    }

    /// X5/L8: a start through a parent whose exit began is refused; nothing
    /// ran and nothing remains.
    #[cfg(not(spawned_sim))] // real Tokio
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn start_link_through_dead_context_is_refused() {
        let (p, _) = parent(false).await;
        let ctx = p.context();
        p.child_handle().stop();
        p.wait_exit().await;
        let child = Child::new(Then::Ready);
        let hooks = child.hooks.clone();
        assert!(matches!(
            child.start_link(&ctx).await,
            Err(StartError::ParentExited)
        ));
        assert_eq!(hooks.started.load(Ordering::SeqCst), 0);
        assert_eq!(hooks.dropped.load(Ordering::SeqCst), 1);
    }

    /// S7: link and monitor children inherit the parent's group; plain
    /// starts never inherit; placement_of opts in.
    #[cfg(not(spawned_sim))] // real Tokio
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn link_and_monitor_inherit_the_parents_group() {
        let system = ActorSystem::new().unwrap();
        for group in [
            system.async_group("inherit-async", 2).unwrap(),
            system.thread_group("inherit-threads", 4).unwrap(),
        ] {
            let p = Parent {
                trap: false,
                seen: Seen::default(),
            }
            .with_options(&group)
            .start()
            .await
            .unwrap();
            let name = group.name();
            for verb in 0..4 {
                let child = Child::new(Then::Ready);
                let seen_group = child.group.clone();
                let started = match verb {
                    0 => child.start_link(&p.context()).await.map(|_| ()),
                    1 => child.start_monitor(&p.context()).await.map(|_| ()),
                    2 => child.start().await.map(|_| ()),
                    _ => child
                        .with_options(StartOptions::new().placement_of(&p.context()))
                        .start()
                        .await
                        .map(|_| ()),
                };
                assert!(matches!(started, Ok(())));
                let expected = if verb == 2 { None } else { name.clone() };
                assert_eq!(*seen_group.lock().unwrap(), Some(expected), "verb {verb}");
            }
            p.child_handle().stop();
            p.wait_exit().await;
        }
        assert!(system.shutdown(LIMIT).await.complete);
    }

    /// S7: an inherited full thread group refuses with Capacity (no
    /// fallback), and nothing of the refused start is observable.
    #[cfg(not(spawned_sim))] // real Tokio
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn inherited_start_into_full_thread_group_fails_capacity() {
        let system = ActorSystem::new().unwrap();
        let group = system.thread_group("inherit-full", 1).unwrap();
        let p = Parent {
            trap: false,
            seen: Seen::default(),
        }
        .with_options(&group)
        .start()
        .await
        .unwrap();
        let child = Child::new(Then::Ready);
        let hooks = child.hooks.clone();
        assert!(matches!(
            child.start_link(&p.context()).await,
            Err(StartError::Capacity)
        ));
        assert_eq!(hooks.started.load(Ordering::SeqCst), 0);
        p.child_handle().stop();
        p.wait_exit().await;
        assert!(system.shutdown(LIMIT).await.complete);
    }

    /// S9: typed names: a clash reports the holder; a failed named start can
    /// be retried at once (the name is free when the error is answered); a
    /// non-actor holder reports no handle.
    #[cfg(not(spawned_sim))] // real Tokio
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn names_are_committed_and_released_atomically() {
        static NAME: Name<Child> = Name::new("start-contract-tasks");
        let holder = Child::new(Then::Ready)
            .with_options(StartOptions::new().name(NAME.clone()))
            .start()
            .await
            .unwrap();
        assert_eq!(NAME.whereis().map(|a| a.id()), Some(holder.id()));
        let loser = Child::new(Then::Ready);
        let hooks = loser.hooks.clone();
        match loser
            .with_options(StartOptions::new().name(NAME.clone()))
            .start()
            .await
        {
            Err(StartError::AlreadyStarted { existing, name }) => {
                assert_eq!(name, NAME.key());
                assert_eq!(existing.map(|h: ChildHandle| h.id()), Some(holder.id()));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(hooks.started.load(Ordering::SeqCst), 0);
        holder.child_handle().stop();
        holder.wait_exit().await;
        // A failed start answers after the name's release.
        for _ in 0..3 {
            assert!(Child::new(Then::Error)
                .with_options(StartOptions::new().name(NAME.clone()))
                .start()
                .await
                .is_err());
        }
        assert!(NAME.whereis().is_none());
        spawned_concurrency::registry::register("start-contract-plain", 7u8).unwrap();
        match Child::new(Then::Ready)
            .with_options(StartOptions::new().name("start-contract-plain"))
            .spawn()
        {
            Err(StartError::AlreadyStarted { existing: None, .. }) => {}
            other => panic!("{other:?}"),
        }
        spawned_concurrency::registry::unregister("start-contract-plain");
    }

    /// Review #291 item 6: a name held through `ctx.register_name` reports
    /// its holder in `AlreadyStarted { existing }`.
    #[cfg(not(spawned_sim))] // real Tokio
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn already_started_reports_a_ctx_registered_holder() {
        struct Registrar;
        impl Actor for Registrar {
            async fn started(&mut self, ctx: &Context<Self>) {
                ctx.register_name("start-contract-ctx").unwrap();
            }
        }
        let system = spawned_concurrency::system::ActorSystem::new().unwrap();
        let group = system.async_group("start-contract-ctx", 2).unwrap();
        let holder = Registrar.with_options(&group).start().await.unwrap();
        match Child::new(Then::Ready)
            .with_options(StartOptions::new().name("start-contract-ctx"))
            .start()
            .await
        {
            Err(StartError::AlreadyStarted { existing, .. }) => {
                assert_eq!(existing.map(|h: ChildHandle| h.id()), Some(holder.id()));
            }
            other => panic!("{other:?}"),
        }
        holder.child_handle().stop();
        holder.wait_exit().await;
        assert!(system.shutdown(Duration::from_secs(5)).await.complete);
    }

    /// Options are validated before anything exists.
    #[cfg(not(spawned_sim))] // real Tokio
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn invalid_options_are_refused() {
        for options in [
            StartOptions::new().init_timeout(Duration::ZERO),
            StartOptions::new().max_outstanding(0),
            StartOptions::new().max_native_jobs(0),
            StartOptions::new().backend(Backend::Thread).stack_size(0),
            StartOptions::new().stack_size(1 << 20),
        ] {
            let child = Child::new(Then::Ready);
            let hooks = child.hooks.clone();
            assert!(matches!(
                child.with_options(options).spawn(),
                Err(StartError::InvalidOptions(_))
            ));
            assert_eq!(hooks.started.load(Ordering::SeqCst), 0);
        }
        let ok = Child::new(Then::Ready)
            .with_options(
                StartOptions::new()
                    .backend(Backend::Thread)
                    .stack_size(1 << 20),
            )
            .start()
            .await;
        assert!(ok.is_ok());
    }

    /// L9: a trapping child whose parent exits while it initialises commits
    /// ready, runs its continuation, then ends from the queued parent exit.
    #[cfg(not(spawned_sim))] // real Tokio
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn trapping_child_with_parent_exit_in_started_commits_ready_then_ends() {
        struct Trapping(
            Arc<Mutex<Vec<&'static str>>>,
            Option<tokio::sync::oneshot::Receiver<()>>,
        );
        impl Actor for Trapping {
            async fn started(&mut self, ctx: &Context<Self>) -> Init<Self> {
                ctx.trap_exit(true);
                let _ = self.1.take().unwrap().await;
                Init::continue_with(Cont)
            }
            async fn stopped(&mut self, _: &Context<Self>) {
                self.0.lock().unwrap().push("stopped");
            }
        }
        impl Handler<Cont> for Trapping {
            async fn handle(&mut self, _: Cont, _: PendingReply<'_, ()>, _: &Context<Self>) {
                self.0.lock().unwrap().push("continue");
            }
        }
        let (p, _) = parent(false).await;
        let (release, gate) = tokio::sync::oneshot::channel();
        let log: Arc<Mutex<Vec<&'static str>>> = Arc::default();
        let starting = Trapping(log.clone(), Some(gate)).start_link(&p.context());
        let child = starting.actor_ref().unwrap();
        p.child_handle().stop();
        p.wait_exit().await;
        release.send(()).unwrap();
        assert!(starting.await.is_ok());
        assert_eq!(child.wait_exit().await, ExitReason::Normal);
        assert_eq!(*log.lock().unwrap(), ["continue", "stopped"]);
    }

    /// A runtime dropped before the child ran: the start is decided at its
    /// death point (`Killed`) and answered, never left pending forever.
    #[cfg(not(spawned_sim))] // real Tokio runtimes
    #[test]
    fn runtime_drop_before_started_answers_init_exited_killed() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let child = Child::new(Then::Ready);
        let hooks = child.hooks.clone();
        let starting = {
            let _entered = rt.enter();
            child.start()
        };
        drop(rt);
        let other = tokio::runtime::Runtime::new().unwrap();
        let result = other.block_on(async { tokio::time::timeout(LIMIT, starting).await });
        assert!(
            matches!(
                result,
                Ok(Err(StartError::InitExited {
                    reason: ExitReason::Killed,
                    pending: None
                }))
            ),
            "{result:?}"
        );
        assert_eq!(hooks.started.load(Ordering::SeqCst), 0);
        assert_eq!(hooks.dropped.load(Ordering::SeqCst), 1);
    }

    /// The init timer's pending answer: a started() that never returns
    /// keeps the instance, answered with its handle at the cleanup
    /// deadline (stuck policy).
    #[cfg(not(spawned_sim))] // real Tokio
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stuck_started_reports_pending_after_the_timeout() {
        stuck_started_reports_pending().await;
    }

    /// Review #291 item 3: the tasks-mode init timer runs on the runtime's
    /// clock. Under the simulator an hour-long `init_timeout` expires in
    /// virtual time, before a two-hour `started()` returns (a real-time
    /// timer thread would let the start succeed).
    #[cfg(spawned_sim)]
    #[spawned_macros::sim_test]
    async fn init_timer_uses_the_runtime_clock() {
        struct Slow;
        impl Actor for Slow {
            async fn started(&mut self, _: &Context<Self>) {
                spawned_rt::tasks::sleep(Duration::from_secs(7200)).await;
            }
        }
        let began = spawned_rt::time::Instant::now();
        let result = Slow
            .with_options(StartOptions::new().init_timeout(Duration::from_secs(3600)))
            .start()
            .await;
        assert!(
            matches!(result, Err(StartError::InitTimeout { .. })),
            "{result:?}"
        );
        // At the timeout plus the cleanup deadline, not at started()'s end.
        assert!(
            began.elapsed() < Duration::from_secs(3700),
            "{:?}",
            began.elapsed()
        );
    }

    async fn stuck_started_reports_pending() {
        static STUCK: AtomicBool = AtomicBool::new(true);
        struct Stuck;
        impl Actor for Stuck {
            async fn started(&mut self, _: &Context<Self>) {
                while STUCK.load(Ordering::SeqCst) {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            }
        }
        let result = Stuck
            .with_options(StartOptions::new().init_timeout(Duration::from_millis(10)))
            .start()
            .await;
        let pending = match result {
            Err(StartError::InitTimeout {
                pending: Some(pending),
            }) => pending,
            other => panic!("{other:?}"),
        };
        assert!(pending.is_alive());
        STUCK.store(false, Ordering::SeqCst);
        assert_eq!(pending.wait_exit_async().await, ExitReason::Killed);
    }
}

mod threads_mode {
    use super::*;
    use spawned_concurrency::threads::{
        Actor, ActorRef, ActorStart, Context, Handler, Init, Name, StartOptions,
    };

    struct Child {
        then: Then,
        hooks: Arc<Hooks>,
        log: Arc<Mutex<Vec<&'static str>>>,
    }
    impl Child {
        fn new(then: Then) -> Self {
            Self {
                then,
                hooks: Arc::default(),
                log: Arc::default(),
            }
        }
    }
    impl Drop for Child {
        fn drop(&mut self) {
            self.hooks.dropped.fetch_add(1, Ordering::SeqCst);
        }
    }
    impl Actor for Child {
        fn started(&mut self, ctx: &Context<Self>) -> Init<Self> {
            self.hooks.started.fetch_add(1, Ordering::SeqCst);
            match self.then {
                Then::Ready => Init::ready(),
                Then::Continue => Init::continue_with(Cont),
                Then::Error => Init::error("bad config"),
                Then::Ignore => Init::ignore(),
                Then::Panic => panic!("init panic"),
                Then::Stop => Init::stop(ExitReason::Shutdown(None)),
                Then::CtxStop => {
                    ctx.stop();
                    Init::ready()
                }
                Then::PanicLater => Init::continue_with(Boom),
            }
        }
        fn stopped(&mut self, _: &Context<Self>) {
            self.hooks.stopped.fetch_add(1, Ordering::SeqCst);
        }
    }
    impl Handler<Work> for Child {
        fn handle(&mut self, _: Work, reply: PendingReply<'_, ()>, _: &Context<Self>) {
            self.log.lock().unwrap().push("work");
            reply.reply(());
        }
    }
    impl Handler<Cont> for Child {
        fn handle(&mut self, _: Cont, _: PendingReply<'_, ()>, _: &Context<Self>) {
            self.log.lock().unwrap().push("continue");
        }
    }
    impl Handler<Boom> for Child {
        fn handle(&mut self, _: Boom, _: PendingReply<'_, ()>, _: &Context<Self>) {
            panic!("failure right after readiness");
        }
    }

    struct Parent {
        exits: Arc<Mutex<Vec<Exit>>>,
        downs: Arc<Mutex<Vec<Down>>>,
    }
    impl Actor for Parent {
        fn exit_received(&mut self, exit: Exit, _: &Context<Self>) {
            self.exits.lock().unwrap().push(exit);
        }
    }
    impl Handler<Down> for Parent {
        fn handle(&mut self, down: Down, reply: PendingReply<'_, ()>, _: &Context<Self>) {
            self.downs.lock().unwrap().push(down);
            reply.reply(());
        }
    }
    impl Handler<Work> for Parent {
        fn handle(&mut self, _: Work, reply: PendingReply<'_, ()>, _: &Context<Self>) {
            reply.reply(());
        }
    }
    type Log<T> = Arc<Mutex<Vec<T>>>;
    fn parent(trap: bool) -> (ActorRef<Parent>, Log<Exit>, Log<Down>) {
        let (exits, downs): (Log<Exit>, Log<Down>) = Default::default();
        let p = Parent {
            exits: exits.clone(),
            downs: downs.clone(),
        }
        .spawn();
        p.context().trap_exit(trap);
        p.request(Work).unwrap();
        (p, exits, downs)
    }

    #[test]
    fn init_outcomes_answer_after_publication_without_stopped() {
        for then in [
            Then::Ready,
            Then::Continue,
            Then::Error,
            Then::Ignore,
            Then::Panic,
            Then::Stop,
            Then::CtxStop,
        ] {
            for waiting in [true, false] {
                let child = Child::new(then);
                let hooks = child.hooks.clone();
                let raw = if waiting {
                    child.start().map(|_| ())
                } else {
                    child.spawn().ready()
                };
                assert_eq!(
                    matches!(raw, Err(StartError::Ignored { pending: None })),
                    matches!(then, Then::Ignore),
                    "{then:?}"
                );
                let result = StartOutcome::from_result(raw);
                if !matches!(result, Ok(StartOutcome::Started(()))) {
                    assert_eq!(hooks.dropped.load(Ordering::SeqCst), 1, "{then:?}");
                }
                assert_eq!(hooks.stopped.load(Ordering::SeqCst), 0, "{then:?}");
                let ok = matches!(
                    (then, &result),
                    (Then::Ready | Then::Continue, Ok(StartOutcome::Started(())))
                        | (
                            Then::Error,
                            Err(StartError::InitFailed { pending: None, .. })
                        )
                        | (Then::Ignore, Ok(StartOutcome::Ignored { pending: None }))
                        | (
                            Then::Panic,
                            Err(StartError::InitExited {
                                reason: ExitReason::Panic(_),
                                pending: None,
                            }),
                        )
                        | (
                            Then::Stop,
                            Err(StartError::InitExited {
                                reason: ExitReason::Shutdown(None),
                                pending: None,
                            }),
                        )
                        | (
                            Then::CtxStop,
                            Err(StartError::InitExited {
                                reason: ExitReason::Normal,
                                pending: None,
                            }),
                        )
                );
                assert!(ok, "{then:?} (waiting {waiting}): {result:?}");
            }
        }
    }

    #[test]
    fn failed_start_notices_follow_the_disposition_table() {
        let (p, exits, downs) = parent(true);
        assert!(Child::new(Then::Panic).start_link(&p.context()).is_err());
        assert!(Child::new(Then::Error).start_monitor(&p.context()).is_err());
        p.request(Work).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        assert!(exits.lock().unwrap().is_empty(), "Exit not consumed");
        assert!(downs.lock().unwrap().is_empty(), "Down not consumed");
        let (ended, _, _) = parent(false);
        assert!(Child::new(Then::Panic)
            .start_link(&ended.context())
            .is_err());
        assert!(matches!(ended.wait_exit(), ExitReason::Panic(_)));
    }

    #[test]
    fn ready_then_immediate_crash_returns_started_and_delivers_down() {
        let (p, _, downs) = parent(false);
        let (child, monitor) = Child::new(Then::PanicLater)
            .start_monitor(&p.context())
            .unwrap();
        assert!(matches!(child.wait_exit(), ExitReason::Panic(_)));
        let deadline = std::time::Instant::now() + LIMIT;
        while downs.lock().unwrap().is_empty() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(downs.lock().unwrap()[0].monitor_ref, monitor);
    }

    #[test]
    fn continuation_runs_first() {
        let child = Child::new(Then::Continue);
        let log = child.log.clone();
        let child = child.start().unwrap();
        child.request(Work).unwrap();
        assert_eq!(*log.lock().unwrap(), ["continue", "work"]);
    }

    #[test]
    fn names_and_refusals() {
        static NAME: Name<Child> = Name::new("start-contract-threads");
        let holder = Child::new(Then::Ready)
            .with_options(StartOptions::new().name(NAME.clone()))
            .start()
            .unwrap();
        assert_eq!(NAME.whereis().map(|a| a.id()), Some(holder.id()));
        let loser = Child::new(Then::Ready);
        let hooks = loser.hooks.clone();
        assert!(matches!(
            loser
                .with_options(StartOptions::new().name(NAME.clone()))
                .start(),
            Err(StartError::AlreadyStarted {
                existing: Some(_),
                ..
            })
        ));
        assert_eq!(hooks.started.load(Ordering::SeqCst), 0);
        holder.context().stop();
        holder.wait_exit();
        assert!(NAME.whereis().is_none());
        assert!(matches!(
            Child::new(Then::Ready)
                .with_options(StartOptions::new().init_timeout(Duration::ZERO))
                .spawn(),
            Err(StartError::InvalidOptions(_))
        ));
        let (p, _, _) = parent(false);
        let ctx = p.context();
        p.context().stop();
        p.wait_exit();
        assert!(matches!(
            Child::new(Then::Ready).start_link(&ctx),
            Err(StartError::ParentExited)
        ));
    }

    /// Review #291 item 7 (threads): a parent killed while it blocks in
    /// `start_link` returns `ParentExited` at once and dies `Killed`; it does
    /// not wait for the child's `started()` (OTP: the caller dies at once).
    #[test]
    fn parent_killed_while_awaiting_start_link_abandons_the_wait() {
        type Gate = std::sync::mpsc::Receiver<()>;
        struct Gated(Mutex<Option<Gate>>, std::sync::mpsc::Sender<()>);
        impl Actor for Gated {
            fn started(&mut self, _: &Context<Self>) {
                let _ = self.1.send(());
                if let Some(gate) = self.0.lock().unwrap().take() {
                    let _ = gate.recv_timeout(Duration::from_secs(10));
                }
            }
        }
        struct Waiter;
        impl Actor for Waiter {}
        type Done = std::sync::mpsc::Sender<Result<(), StartError>>;
        struct LinkGated(Mutex<Option<(Gated, Done)>>);
        impl Message for LinkGated {
            type Result = ();
        }
        impl Handler<LinkGated> for Waiter {
            fn handle(&mut self, msg: LinkGated, reply: PendingReply<'_, ()>, ctx: &Context<Self>) {
                reply.reply(());
                let (child, done) = msg.0.lock().unwrap().take().unwrap();
                let _ = done.send(child.start_link(ctx).map(|_| ()));
            }
        }
        let (release, gate) = std::sync::mpsc::channel();
        let (done_tx, done) = std::sync::mpsc::channel();
        let (entered_tx, entered) = std::sync::mpsc::channel();
        let p = Waiter.start().unwrap();
        p.request(LinkGated(Mutex::new(Some((
            Gated(Mutex::new(Some(gate)), entered_tx),
            done_tx,
        )))))
        .unwrap();
        // The child is in started(): the parent waits in start_link.
        entered.recv_timeout(Duration::from_secs(5)).unwrap();
        p.child_handle().kill();
        let result = done
            .recv_timeout(Duration::from_secs(5))
            .expect("the wait was abandoned");
        assert!(
            matches!(result, Err(StartError::ParentExited)),
            "{result:?}"
        );
        let until = std::time::Instant::now() + Duration::from_secs(5);
        while p.child_handle().exit_reason().is_none() {
            assert!(std::time::Instant::now() < until, "the parent did not die");
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(p.child_handle().exit_reason(), Some(ExitReason::Killed));
        release.send(()).unwrap();
    }
}
