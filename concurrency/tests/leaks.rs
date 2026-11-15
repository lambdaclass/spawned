//! Leak tests: after actors, supervisors and an `ActorSystem` shut down, the
//! resources they owned are released.
//!
//! Observation points, all through public APIs:
//! - actor state: a `Weak` sentinel held in the actor struct (state dropped);
//! - OS threads: a sentinel moved into a thread-local of the actor or group
//!   worker thread, released only by the thread-exit TLS destructor;
//! - Tokio tasks (run loops, monitor waiters, timers): `num_alive_tasks()` of
//!   a dedicated runtime;
//! - registry entries: `registry::registered()`;
//! - user-held references (`Recipient`, which wraps an `ActorRef`/`Context`):
//!   `Arc::downgrade` of the recipient.
//!
//! Not publicly observable (covered only by LeakSanitizer in CI, which reports
//! unreachable allocations at process exit): framework-internal clones of
//! `ActorRef`/`Context`, link tables, and native monitor waiter threads.
// Tests that need OS threads are compiled out under spawned_sim.
#![cfg_attr(spawned_sim, allow(unused_imports, dead_code))]
#[macro_use]
#[path = "support/deadline.rs"]
mod deadline;
use spawned_concurrency::{message::Message, registry, threads};
use std::cell::RefCell;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

const LIMIT: Duration = Duration::from_secs(4);

fn eventually(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + LIMIT;
    while !done() {
        assert!(Instant::now() < deadline, "leak: {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn released<T: ?Sized>(what: &str, weak: &Weak<T>) {
    eventually(what, || weak.strong_count() == 0);
}

thread_local! {
    static THREAD_PROBE: RefCell<Vec<Arc<()>>> = const { RefCell::new(Vec::new()) };
}

/// Keep `probe` alive until the current OS thread exits. Use only on threads
/// that are expected to exit (dedicated actor threads, group workers).
fn bind_to_current_thread(probe: Arc<()>) {
    THREAD_PROBE.with(|p| p.borrow_mut().push(probe));
}

#[derive(Clone)]
struct Ping;
impl Message for Ping {
    type Result = ();
}

/// Native actor whose state and thread lifetimes are observable.
struct NativeProbe {
    _state: Arc<()>,
    thread: Option<Arc<()>>,
}
impl threads::Actor for NativeProbe {
    fn started(&mut self, _: &threads::Context<Self>) {
        if let Some(probe) = self.thread.take() {
            bind_to_current_thread(probe);
        }
    }
}
impl threads::Handler<Ping> for NativeProbe {
    fn handle(
        &mut self,
        _: Ping,
        reply: spawned_concurrency::PendingReply<'_, ()>,
        _: &threads::Context<Self>,
    ) {
        reply.reply(());
    }
}

#[test]
fn native_actor_thread_and_state_released_after_stop() {
    let state = Arc::new(());
    let thread = Arc::new(());
    let (state_weak, thread_weak) = (Arc::downgrade(&state), Arc::downgrade(&thread));
    let actor = threads::ActorStart::start(NativeProbe {
        _state: state,
        thread: Some(thread),
    });
    actor.request(Ping).unwrap();
    assert_eq!(thread_weak.strong_count(), 1, "probe bound to actor thread");
    let timer = threads::send_interval(Duration::from_secs(3600), actor.context(), Ping);
    actor.context().stop();
    actor.join();
    drop((actor, timer));
    released("native actor state", &state_weak);
    released("native actor thread", &thread_weak);
}

// Reference cycle between two native actors: each holds the other's ActorRef.
struct SetPeer<T>(T);
impl<T: Send + 'static> Message for SetPeer<T> {
    type Result = ();
}
struct NativeA {
    _state: Arc<()>,
    peer: Option<threads::ActorRef<NativeB>>,
}
struct NativeB {
    _state: Arc<()>,
    peer: Option<threads::ActorRef<NativeA>>,
}
impl threads::Actor for NativeA {}
impl threads::Actor for NativeB {}
impl threads::Handler<SetPeer<threads::ActorRef<NativeB>>> for NativeA {
    fn handle(
        &mut self,
        msg: SetPeer<threads::ActorRef<NativeB>>,
        reply: spawned_concurrency::PendingReply<'_, ()>,
        _: &threads::Context<Self>,
    ) {
        self.peer = Some(msg.0);
        reply.reply(());
    }
}
impl threads::Handler<SetPeer<threads::ActorRef<NativeA>>> for NativeB {
    fn handle(
        &mut self,
        msg: SetPeer<threads::ActorRef<NativeA>>,
        reply: spawned_concurrency::PendingReply<'_, ()>,
        _: &threads::Context<Self>,
    ) {
        self.peer = Some(msg.0);
        reply.reply(());
    }
}

#[test]
fn native_actor_reference_cycle_released_after_stop() {
    let (sa, sb) = (Arc::new(()), Arc::new(()));
    let (wa, wb) = (Arc::downgrade(&sa), Arc::downgrade(&sb));
    let a = threads::ActorStart::start(NativeA {
        _state: sa,
        peer: None,
    });
    let b = threads::ActorStart::start(NativeB {
        _state: sb,
        peer: None,
    });
    a.request(SetPeer(b.clone())).unwrap();
    b.request(SetPeer(a.clone())).unwrap();
    a.context().stop();
    b.context().stop();
    a.join();
    b.join();
    drop((a, b));
    released("native cycle: A state", &wa);
    released("native cycle: B state", &wb);
}

#[test]
fn owned_registration_removed_with_its_guard_after_native_stop() {
    let actor = threads::ActorStart::start(NativeProbe {
        _state: Arc::new(()),
        thread: None,
    });
    let name = "leaks-native-owned";
    let guard = registry::register_owned(name, actor.id(), actor.clone()).unwrap();
    actor.context().stop();
    actor.join();
    drop(guard);
    assert!(!registry::registered().iter().any(|n| n == name));
    assert!(registry::whereis::<threads::ActorRef<NativeProbe>>(name).is_none());
}

#[cfg(feature = "async")]
mod async_leaks {
    use super::*;
    use spawned_concurrency::{
        supervision::{Backoff, ChildSpec, SupervisorConfig},
        system::ActorSystem,
        tasks::{self, Actor, ActorRef, ActorStart, Backend, Context, Handler},
        Down,
    };
    use std::sync::Mutex;

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
    }

    fn tasks_drain_to(rt: &tokio::runtime::Runtime, expected: usize, what: &str) {
        let metrics = rt.metrics();
        eventually(what, || metrics.num_alive_tasks() == expected);
    }

    struct Probe {
        _state: Arc<()>,
        thread: Option<Arc<()>>,
        name: Option<&'static str>,
    }
    impl Probe {
        fn new(state: Arc<()>) -> Self {
            Self {
                _state: state,
                thread: None,
                name: None,
            }
        }
    }
    impl Actor for Probe {
        async fn started(&mut self, ctx: &Context<Self>) {
            if let Some(probe) = self.thread.take() {
                bind_to_current_thread(probe);
            }
            if let Some(name) = self.name {
                ctx.register_name(name).unwrap();
            }
        }
    }
    impl Handler<Ping> for Probe {
        async fn handle(
            &mut self,
            _: Ping,
            reply: spawned_concurrency::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            reply.reply(());
        }
    }
    struct Crash;
    impl Message for Crash {
        type Result = ();
    }
    impl Handler<Crash> for Probe {
        async fn handle(
            &mut self,
            _: Crash,
            _reply: spawned_concurrency::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            {
                panic!("injected failure for leak test");
            }
        }
    }
    impl Handler<Down> for Probe {
        async fn handle(
            &mut self,
            _: Down,
            reply: spawned_concurrency::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            reply.reply(());
        }
    }

    // Not simulated: counts Tokio runtime tasks and OS threads.
    #[cfg(not(spawned_sim))]
    #[test]
    fn actor_tasks_threads_and_state_released_on_every_backend() {
        for backend in [Backend::Async, Backend::Blocking, Backend::Thread] {
            let rt = runtime();
            let state = Arc::new(());
            let thread = Arc::new(());
            let (state_weak, thread_weak) = (Arc::downgrade(&state), Arc::downgrade(&thread));
            rt.block_on(async {
                let mut probe = Probe::new(state);
                // Only a dedicated thread is expected to exit with the actor.
                if backend == Backend::Thread {
                    probe.thread = Some(thread);
                }
                let actor = probe.start_with_backend(backend);
                actor.request(Ping).await.unwrap();
                if backend == Backend::Thread {
                    assert_eq!(thread_weak.strong_count(), 1, "probe bound to actor thread");
                }
                let timer = tasks::send_interval(Duration::from_secs(3600), actor.context(), Ping);
                actor.context().stop();
                within!(actor.join()).await;
                drop((actor, timer));
            });
            tasks_drain_to(&rt, 0, &format!("{backend:?}: runtime tasks after stop"));
            released(&format!("{backend:?}: actor state"), &state_weak);
            released(&format!("{backend:?}: actor thread"), &thread_weak);
        }
    }

    // Not simulated: counts Tokio runtime tasks and OS threads.
    #[cfg(not(spawned_sim))]
    #[test]
    fn monitor_waiters_released_on_demonitor_owner_exit_and_target_exit() {
        let rt = runtime();
        rt.block_on(async {
            let owner = Probe::new(Arc::new(())).start();
            let target = Probe::new(Arc::new(())).start();
            owner.request(Ping).await.unwrap();
            target.request(Ping).await.unwrap();
            let baseline = rt.metrics().num_alive_tasks();

            // Demonitor while the target is alive releases the waiter.
            let monitor = owner.context().monitor(&target.child_handle());
            tasks_drain_to(&rt, baseline + 1, "monitor waiter spawned");
            owner.context().demonitor(monitor);
            tasks_drain_to(&rt, baseline, "monitor waiter after demonitor");

            // Owner exit while the target is alive releases the waiter.
            owner.context().monitor(&target.child_handle());
            owner.context().monitor(&target.child_handle());
            tasks_drain_to(&rt, baseline + 2, "two monitor waiters spawned");
            owner.context().stop();
            within!(owner.join()).await;
            tasks_drain_to(&rt, baseline - 1, "monitor waiters after owner exit");

            // Target exit delivers Down and releases the waiter.
            let watcher = Probe::new(Arc::new(())).start();
            watcher.context().monitor(&target.child_handle());
            target.request(Crash).await.unwrap_err();
            within!(target.join()).await;
            watcher.request(Ping).await.unwrap();
            tasks_drain_to(&rt, baseline - 1, "monitor waiter after target exit");
            watcher.context().stop();
            within!(watcher.join()).await;
        });
        tasks_drain_to(&rt, 0, "runtime tasks after all actors stopped");
    }

    // Reference cycle between async actors: ActorRefs in state plus
    // Recipients, whose Arc we can observe directly.
    struct AsyncA {
        _state: Arc<()>,
        peer: Option<ActorRef<AsyncB>>,
        recipient: Option<tasks::Recipient<Ping>>,
    }
    struct AsyncB {
        _state: Arc<()>,
        peer: Option<ActorRef<AsyncA>>,
        recipient: Option<tasks::Recipient<Ping>>,
    }
    impl Actor for AsyncA {}
    impl Actor for AsyncB {}
    impl Handler<Ping> for AsyncA {
        async fn handle(
            &mut self,
            _: Ping,
            reply: spawned_concurrency::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            reply.reply(());
        }
    }
    impl Handler<Ping> for AsyncB {
        async fn handle(
            &mut self,
            _: Ping,
            reply: spawned_concurrency::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            reply.reply(());
        }
    }
    impl Handler<SetPeer<(ActorRef<AsyncB>, tasks::Recipient<Ping>)>> for AsyncA {
        async fn handle(
            &mut self,
            msg: SetPeer<(ActorRef<AsyncB>, tasks::Recipient<Ping>)>,
            reply: spawned_concurrency::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            self.peer = Some(msg.0 .0);
            self.recipient = Some(msg.0 .1);
            reply.reply(());
        }
    }
    impl Handler<SetPeer<(ActorRef<AsyncA>, tasks::Recipient<Ping>)>> for AsyncB {
        async fn handle(
            &mut self,
            msg: SetPeer<(ActorRef<AsyncA>, tasks::Recipient<Ping>)>,
            reply: spawned_concurrency::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            self.peer = Some(msg.0 .0);
            self.recipient = Some(msg.0 .1);
            reply.reply(());
        }
    }

    // Not simulated: counts Tokio runtime tasks and OS threads.
    #[cfg(not(spawned_sim))]
    #[test]
    fn actor_reference_cycle_released_after_stop_on_every_backend() {
        for backend in [Backend::Async, Backend::Blocking, Backend::Thread] {
            let rt = runtime();
            let (sa, sb) = (Arc::new(()), Arc::new(()));
            let (wa, wb) = (Arc::downgrade(&sa), Arc::downgrade(&sb));
            let (ra_weak, rb_weak) = rt.block_on(async {
                let a = AsyncA {
                    _state: sa,
                    peer: None,
                    recipient: None,
                }
                .start_with_backend(backend);
                let b = AsyncB {
                    _state: sb,
                    peer: None,
                    recipient: None,
                }
                .start_with_backend(backend);
                let ra = a.recipient::<Ping>();
                let rb = b.recipient::<Ping>();
                let weaks = (Arc::downgrade(&ra), Arc::downgrade(&rb));
                // A holds B's ref and recipient; B holds A's.
                a.request(SetPeer((b.clone(), rb))).await.unwrap();
                b.request(SetPeer((a.clone(), ra))).await.unwrap();
                // Links add framework-held references in both directions.
                a.context().link(&b.child_handle());
                a.context().stop();
                b.context().stop();
                within!(a.join()).await;
                within!(b.join()).await;
                weaks
            });
            tasks_drain_to(&rt, 0, &format!("{backend:?}: cycle runtime tasks"));
            released(&format!("{backend:?}: cycle A state"), &wa);
            released(&format!("{backend:?}: cycle B state"), &wb);
            released(&format!("{backend:?}: cycle A recipient"), &ra_weak);
            released(&format!("{backend:?}: cycle B recipient"), &rb_weak);
        }
    }

    /// Actors are not reference counted: dropping every external handle of an
    /// idle cycle does not stop it. This documents the contract (an explicit
    /// stop or an owner is required) rather than a leak in the framework.
    // Not simulated: counts Tokio runtime tasks and OS threads.
    #[cfg(not(spawned_sim))]
    #[test]
    fn unstopped_cycle_keeps_running_until_explicit_stop() {
        let rt = runtime();
        let (sa, sb) = (Arc::new(()), Arc::new(()));
        let (wa, wb) = (Arc::downgrade(&sa), Arc::downgrade(&sb));
        let (ha, hb) = rt.block_on(async {
            let a = AsyncA {
                _state: sa,
                peer: None,
                recipient: None,
            }
            .start();
            let b = AsyncB {
                _state: sb,
                peer: None,
                recipient: None,
            }
            .start();
            a.request(SetPeer((b.clone(), b.recipient::<Ping>())))
                .await
                .unwrap();
            b.request(SetPeer((a.clone(), a.recipient::<Ping>())))
                .await
                .unwrap();
            (a.child_handle(), b.child_handle())
        });
        std::thread::sleep(Duration::from_millis(50));
        assert!(ha.is_alive() && hb.is_alive());
        assert!(wa.strong_count() == 1 && wb.strong_count() == 1);
        ha.stop();
        hb.stop();
        released("unstopped cycle: A state after stop", &wa);
        released("unstopped cycle: B state after stop", &wb);
        tasks_drain_to(&rt, 0, "unstopped cycle: runtime tasks after stop");
    }

    // Not simulated: counts Tokio runtime tasks and OS threads.
    #[cfg(not(spawned_sim))]
    #[test]
    fn supervisor_and_system_release_generations_registrations_and_workers() {
        let rt = runtime();
        let generations = Arc::new(());
        let workers = Arc::new(());
        let (gen_weak, worker_weak) = (Arc::downgrade(&generations), Arc::downgrade(&workers));
        let names = ["leaks-supervised-a", "leaks-supervised-b"];
        rt.block_on(async {
            let system = ActorSystem::new().unwrap();
            let group = system.async_group("leaks", 1).unwrap();
            let mut tree = system
                .supervisor(
                    SupervisorConfig::default().max_restarts(100).backoff(
                        Backoff::default()
                            .initial(Duration::ZERO)
                            .maximum(Duration::ZERO)
                            .jitter(false),
                    ),
                )
                .unwrap();
            let mut children = Vec::new();
            for name in names {
                let (generations, workers) = (generations.clone(), workers.clone());
                children.push(
                    tree.add(ChildSpec::new(name, &group, move |_| {
                        let (generations, workers) = (generations.clone(), workers.clone());
                        async move {
                            Ok::<_, String>(Probe {
                                _state: generations,
                                // Group worker threads exit when the system shuts down.
                                thread: Some(workers),
                                name: Some(name),
                            })
                        }
                    }))
                    .unwrap(),
                );
            }
            let supervisor = within!(tree.start()).await.unwrap();
            // Several generations of the first child.
            for generation in 1..=3 {
                let current = children[0]
                    .wait_for_generation(generation - 1)
                    .await
                    .unwrap();
                current.request(Crash).await.unwrap_err();
                tokio::time::timeout(LIMIT, children[0].wait_for_generation(generation))
                    .await
                    .unwrap()
                    .unwrap();
            }
            for name in names {
                assert!(registry::registered().iter().any(|n| n == name));
            }
            assert!(worker_weak.strong_count() > 1, "probes bound to workers");
            assert!(supervisor.shutdown(LIMIT).await.complete);
            assert!(system.shutdown(LIMIT).await.complete);
            drop(children);
        });
        drop(generations);
        drop(workers);
        for name in names {
            assert!(
                !registry::registered().iter().any(|n| n == name),
                "leak: registry entry {name} after shutdown"
            );
        }
        released("supervised generations' state", &gen_weak);
        released("execution group worker threads", &worker_weak);
        tasks_drain_to(&rt, 0, "caller runtime tasks after system shutdown");
    }

    // Not simulated: counts Tokio runtime tasks and OS threads.
    #[cfg(not(spawned_sim))]
    #[test]
    fn system_actors_and_helpers_released_after_system_shutdown() {
        let rt = runtime();
        let state = Arc::new(());
        let worker = Arc::new(());
        let helper = Arc::new(());
        let (state_weak, worker_weak, helper_weak) = (
            Arc::downgrade(&state),
            Arc::downgrade(&worker),
            Arc::downgrade(&helper),
        );
        let held: Arc<Mutex<Option<Arc<()>>>> = Arc::new(Mutex::new(Some(helper)));
        rt.block_on(async {
            let system = ActorSystem::new().unwrap();
            let group = system.async_group("leaks-system", 1).unwrap();
            let actor = Probe {
                _state: state,
                thread: Some(worker),
                name: Some("leaks-system-actor"),
            }
            .start_on(&group)
            .unwrap();
            actor.request(Ping).await.unwrap();
            assert_eq!(worker_weak.strong_count(), 1, "probe bound to worker");
            // A helper parked forever must be cancelled and dropped.
            let payload = held.lock().unwrap().take().unwrap();
            let _parked = actor
                .context()
                .spawn_helper(async move {
                    let _payload = payload;
                    futures::future::pending::<()>().await;
                })
                .unwrap();
            assert!(system.shutdown(LIMIT).await.complete);
            assert!(!actor.child_handle().is_alive());
        });
        assert!(!registry::registered()
            .iter()
            .any(|n| n == "leaks-system-actor"));
        released("system actor state", &state_weak);
        released("system group worker thread", &worker_weak);
        released("parked helper payload", &helper_weak);
    }
}

/// The async leak checks that do not need OS threads or Tokio's task
/// metrics, on the simulator over seeds `0..SPAWNED_SIM_SEEDS` (default 8):
/// live tasks are counted from the simulator's own task table and every
/// release is awaited inside the run (nothing runs once `block_on` returns).
#[cfg(all(spawned_sim, feature = "async"))]
mod sim_leaks {
    use super::*;
    use spawned_concurrency::{
        system::ActorSystem,
        tasks::{self, Actor, ActorStart, Context, Handler},
        Down,
    };
    use spawned_rt::tasks::{sim, Runtime};

    fn seeds() -> std::ops::Range<u64> {
        0..std::env::var("SPAWNED_SIM_SEEDS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(8)
    }

    /// Yield until `done`, failing after 100,000 scheduling points.
    async fn settle(what: &str, mut done: impl FnMut() -> bool) {
        for _ in 0..100_000 {
            if done() {
                return;
            }
            sim::yield_now().await;
        }
        panic!("leak: {what}");
    }

    struct Probe {
        _state: Arc<()>,
    }
    impl Actor for Probe {}
    impl Handler<Ping> for Probe {
        async fn handle(
            &mut self,
            _: Ping,
            reply: spawned_concurrency::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            reply.reply(());
        }
    }
    impl Handler<Down> for Probe {
        async fn handle(
            &mut self,
            _: Down,
            reply: spawned_concurrency::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            reply.reply(());
        }
    }

    /// A holds B's ref and recipient, B holds A's: a reference cycle through
    /// actor state, as in the Tokio cycle test.
    struct A {
        _state: Arc<()>,
        peer: Option<(tasks::ActorRef<B>, tasks::Recipient<Ping>)>,
    }
    struct B {
        _state: Arc<()>,
        peer: Option<(tasks::ActorRef<A>, tasks::Recipient<Ping>)>,
    }
    impl Actor for A {}
    impl Actor for B {}
    impl Handler<Ping> for A {
        async fn handle(
            &mut self,
            _: Ping,
            reply: spawned_concurrency::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            reply.reply(());
        }
    }
    impl Handler<Ping> for B {
        async fn handle(
            &mut self,
            _: Ping,
            reply: spawned_concurrency::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            reply.reply(());
        }
    }
    impl Handler<SetPeer<(tasks::ActorRef<B>, tasks::Recipient<Ping>)>> for A {
        async fn handle(
            &mut self,
            msg: SetPeer<(tasks::ActorRef<B>, tasks::Recipient<Ping>)>,
            reply: spawned_concurrency::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            self.peer = Some(msg.0);
            reply.reply(());
        }
    }
    impl Handler<SetPeer<(tasks::ActorRef<A>, tasks::Recipient<Ping>)>> for B {
        async fn handle(
            &mut self,
            msg: SetPeer<(tasks::ActorRef<A>, tasks::Recipient<Ping>)>,
            reply: spawned_concurrency::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            self.peer = Some(msg.0);
            reply.reply(());
        }
    }

    #[test]
    fn stopped_actors_cycles_and_timers_release_tasks_and_state() {
        for seed in seeds() {
            let rt = Runtime::with_seed(seed);
            let (state, sa, sb) = (Arc::new(()), Arc::new(()), Arc::new(()));
            let weak = [&state, &sa, &sb].map(Arc::downgrade);
            rt.block_on(async {
                let baseline = rt.stats().tasks;
                let probe = Probe { _state: state }.start();
                probe.request(Ping).await.unwrap();
                let timer = tasks::send_interval(Duration::from_secs(3600), probe.context(), Ping);
                let a = A {
                    _state: sa,
                    peer: None,
                }
                .start();
                let b = B {
                    _state: sb,
                    peer: None,
                }
                .start();
                let (ra, rb) = (a.recipient::<Ping>(), b.recipient::<Ping>());
                let recipients = [Arc::downgrade(&ra), Arc::downgrade(&rb)];
                a.request(SetPeer((b.clone(), rb))).await.unwrap();
                b.request(SetPeer((a.clone(), ra))).await.unwrap();
                a.context().link(&b.child_handle());
                probe.context().stop();
                a.context().stop();
                b.context().stop();
                within!(probe.join()).await;
                within!(a.join()).await;
                within!(b.join()).await;
                drop((probe, timer, a, b));
                settle(&format!("seed {seed}: actor state"), || {
                    weak.iter().all(|w| w.strong_count() == 0)
                })
                .await;
                settle(&format!("seed {seed}: cycle recipients"), || {
                    recipients.iter().all(|w| w.strong_count() == 0)
                })
                .await;
                settle(&format!("seed {seed}: simulated tasks"), || {
                    rt.stats().tasks == baseline
                })
                .await;
            });
        }
    }

    #[test]
    fn monitor_waiters_released_on_demonitor_and_exit() {
        for seed in seeds() {
            let rt = Runtime::with_seed(seed);
            rt.block_on(async {
                let owner = Probe {
                    _state: Arc::new(()),
                }
                .start();
                let target = Probe {
                    _state: Arc::new(()),
                }
                .start();
                owner.request(Ping).await.unwrap();
                target.request(Ping).await.unwrap();
                let baseline = rt.stats().tasks;
                let monitor = owner.context().monitor(&target.child_handle());
                owner.context().demonitor(monitor);
                settle(&format!("seed {seed}: waiter after demonitor"), || {
                    rt.stats().tasks == baseline
                })
                .await;
                owner.context().monitor(&target.child_handle());
                owner.context().monitor(&target.child_handle());
                owner.context().stop();
                within!(owner.join()).await;
                // The owner's own task is gone too.
                settle(&format!("seed {seed}: waiters after owner exit"), || {
                    rt.stats().tasks < baseline
                })
                .await;
                target.context().stop();
                within!(target.join()).await;
            });
        }
    }

    #[test]
    fn system_actor_and_parked_helper_released_after_shutdown() {
        for seed in seeds() {
            let rt = Runtime::with_seed(seed);
            let (state, helper) = (Arc::new(()), Arc::new(()));
            let weak = [&state, &helper].map(Arc::downgrade);
            let name = "leaks-sim-system-actor";
            rt.block_on(async {
                let system = ActorSystem::new().unwrap();
                let group = system.async_group("leaks-sim", 1).unwrap();
                let actor = Probe { _state: state }.start_on(&group).unwrap();
                actor.request(Ping).await.unwrap();
                actor.context().register_name(name).unwrap();
                let _parked = actor
                    .context()
                    .spawn_helper(async move {
                        let _payload = helper;
                        futures::future::pending::<()>().await;
                    })
                    .unwrap();
                assert!(system.shutdown(Duration::from_secs(5)).await.complete);
                assert!(!actor.child_handle().is_alive());
                drop(actor);
                settle(
                    &format!("seed {seed}: system actor state and helper"),
                    || weak.iter().all(|w| w.strong_count() == 0),
                )
                .await;
            });
            assert!(
                !registry::registered().iter().any(|n| n == name),
                "seed {seed}: registry entry after shutdown"
            );
        }
    }

    struct Crash;
    impl Message for Crash {
        type Result = ();
    }
    impl Handler<Crash> for Probe {
        async fn handle(
            &mut self,
            _: Crash,
            _reply: spawned_concurrency::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            panic!("injected failure for leak test");
        }
    }

    /// A monitor whose target exits delivers `Down` and releases its waiter.
    #[test]
    fn monitor_waiter_released_on_target_exit() {
        for seed in seeds() {
            let rt = Runtime::with_seed(seed);
            rt.block_on(async {
                let watcher = Probe {
                    _state: Arc::new(()),
                }
                .start();
                let target = Probe {
                    _state: Arc::new(()),
                }
                .start();
                watcher.request(Ping).await.unwrap();
                target.request(Ping).await.unwrap();
                let baseline = rt.stats().tasks;
                watcher.context().monitor(&target.child_handle());
                target.request(Crash).await.unwrap_err();
                within!(target.join()).await;
                watcher.request(Ping).await.unwrap();
                // The target's task and the waiter are both gone.
                settle(&format!("seed {seed}: waiter after target exit"), || {
                    rt.stats().tasks < baseline
                })
                .await;
                watcher.context().stop();
                within!(watcher.join()).await;
            });
        }
    }

    /// Restarted generations of a supervised child, and their registrations,
    /// are released when the supervisor and the system shut down.
    #[test]
    fn supervisor_generations_and_registrations_released() {
        use spawned_concurrency::supervision::{Backoff, ChildSpec, SupervisorConfig};
        for seed in seeds() {
            let rt = Runtime::with_seed(seed);
            let generations = Arc::new(());
            let gen_weak = Arc::downgrade(&generations);
            let name = "leaks-sim-supervised";
            rt.block_on(async move {
                let system = ActorSystem::new().unwrap();
                let group = system.async_group("leaks-sim-sup", 1).unwrap();
                let mut tree = system
                    .supervisor(
                        SupervisorConfig::default().max_restarts(100).backoff(
                            Backoff::default()
                                .initial(Duration::ZERO)
                                .maximum(Duration::ZERO)
                                .jitter(false),
                        ),
                    )
                    .unwrap();
                let child = tree
                    .add(ChildSpec::new(name, &group, move |_| {
                        let generations = generations.clone();
                        async move {
                            Ok::<_, String>(Named {
                                _probe: Probe {
                                    _state: generations,
                                },
                                name,
                            })
                        }
                    }))
                    .unwrap();
                let supervisor = within!(tree.start()).await.unwrap();
                for generation in 1..=3 {
                    let current = within!(child.wait_for_generation(generation - 1))
                        .await
                        .unwrap();
                    current.request(Crash).await.unwrap_err();
                    within!(child.wait_for_generation(generation))
                        .await
                        .unwrap();
                }
                assert!(registry::registered().iter().any(|n| n == name));
                assert!(supervisor.shutdown(Duration::from_secs(5)).await.complete);
                assert!(system.shutdown(Duration::from_secs(5)).await.complete);
                drop(child);
                settle(&format!("seed {seed}: supervised generations"), || {
                    gen_weak.strong_count() == 0
                })
                .await;
            });
            assert!(
                !registry::registered().iter().any(|n| n == name),
                "seed {seed}: registry entry after shutdown"
            );
        }
    }

    /// A probe that registers a name when it starts.
    struct Named {
        _probe: Probe,
        name: &'static str,
    }
    impl Actor for Named {
        async fn started(&mut self, ctx: &Context<Self>) {
            ctx.register_name(self.name).unwrap();
        }
    }
    impl Handler<Crash> for Named {
        async fn handle(
            &mut self,
            _: Crash,
            _reply: spawned_concurrency::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            panic!("injected failure for leak test");
        }
    }
}
