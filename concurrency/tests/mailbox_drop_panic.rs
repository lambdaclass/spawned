//! Run abort-sensitive checks in a subprocess so CI survives a regression.
#![cfg(feature = "async")]
use spawned_concurrency::{
    message::Message,
    system::ActorSystem,
    tasks::{Actor, ActorStart, Backend, Context, Handler},
    Down, Exit, ExitReason,
};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::{mpsc, oneshot};
#[path = "support/drop_subprocess.rs"]
mod subprocess;
const LIMIT: Duration = Duration::from_secs(10);

struct Hold(oneshot::Sender<()>, oneshot::Receiver<()>, bool);
impl Message for Hold {
    type Result = ();
}
struct Bomb(Arc<AtomicUsize>);
impl Message for Bomb {
    type Result = ();
}
impl Drop for Bomb {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("queued message destructor");
    }
}
struct Victim(Arc<AtomicUsize>, bool);
impl Drop for Victim {
    fn drop(&mut self) {
        assert!(!self.1, "actor state destructor");
    }
}
impl Actor for Victim {}
impl Handler<Hold> for Victim {
    async fn handle(&mut self, msg: Hold, _: &Context<Self>) {
        msg.0.send(()).unwrap();
        let _ = msg.1.await;
        assert!(!msg.2, "original handler failure");
    }
}
impl Handler<Bomb> for Victim {
    async fn handle(&mut self, _: Bomb, _: &Context<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
struct Ping;
impl Message for Ping {
    type Result = ();
}
struct Observer(mpsc::UnboundedSender<(bool, ExitReason)>);
impl Actor for Observer {
    async fn exit_received(&mut self, exit: Exit, _: &Context<Self>) {
        self.0.send((true, exit.reason)).unwrap();
    }
}
impl Handler<Down> for Observer {
    async fn handle(&mut self, down: Down, _: &Context<Self>) {
        self.0.send((false, down.reason)).unwrap();
    }
}
impl Handler<Ping> for Observer {
    async fn handle(&mut self, _: Ping, _: &Context<Self>) {}
}

#[test]
fn queued_destructor_panics_do_not_abort_or_kill_trapping_observers() {
    subprocess::run("mailbox_drop_child", "validated 30 queued panic cases");
}

#[test]
#[ignore = "subprocess helper, exercised by the enabled parent regression"]
fn mailbox_drop_child() {
    if std::env::var_os("SPAWNED_MAILBOX_DROP_CHILD").is_none() {
        return;
    }
    // Positive control for stdout drainage: this exceeds typical pipe capacity.
    println!("{}", "x".repeat(128 * 1024));
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        // Async, blocking, dedicated thread, managed async, managed native roots.
        for placement in 0..5 {
            for bombs in [1, 2, 256] {
                for handler_panics in [false, true] {
                    let system = ActorSystem::new().unwrap();
                    let group = match placement {
                        3 => Some(system.async_group("panic-async", 1).unwrap()),
                        4 => Some(system.thread_group("panic-native", 1).unwrap()),
                        _ => None,
                    };
                    let handled = Arc::new(AtomicUsize::new(0));
                    let dropped = Arc::new(AtomicUsize::new(0));
                    let victim = match &group {
                        Some(group) => Victim(handled.clone(), false).start_on(group).unwrap(),
                        None => {
                            Victim(handled.clone(), false).start_with_backend(match placement {
                                0 => Backend::Async,
                                1 => Backend::Blocking,
                                _ => Backend::Thread,
                            })
                        }
                    };
                    let (send, mut events) = mpsc::unbounded_channel();
                    let observer = Observer(send).start();
                    let ctx = observer.context();
                    ctx.trap_exit(true);
                    ctx.link(&victim.child_handle());
                    let _monitor = ctx.monitor(&victim.child_handle());
                    let (entered, arrival) = oneshot::channel();
                    let (release, gate) = oneshot::channel();
                    victim.send(Hold(entered, gate, handler_panics)).unwrap();
                    spawned_rt::tasks::timeout(LIMIT, arrival)
                        .await
                        .unwrap()
                        .unwrap();
                    let replies: Vec<_> = (0..bombs)
                        .map(|_| victim.request_raw(Bomb(dropped.clone())).unwrap())
                        .collect();
                    victim.context().stop();
                    release.send(()).unwrap();
                    let reason = spawned_rt::tasks::timeout(LIMIT, victim.wait_exit())
                        .await
                        .unwrap();
                    if handler_panics {
                        assert_eq!(
                            reason,
                            ExitReason::Panic("panic in handler: original handler failure".into())
                        );
                    } else {
                        assert_eq!(
                            reason,
                            ExitReason::Panic(
                                "queued message destructor: queued message destructor".into()
                            )
                        );
                    }
                    let first = spawned_rt::tasks::timeout(LIMIT, events.recv())
                        .await
                        .unwrap()
                        .unwrap();
                    let second = spawned_rt::tasks::timeout(LIMIT, events.recv())
                        .await
                        .unwrap()
                        .unwrap();
                    assert_ne!(first.0, second.0, "expected one Exit and one Down");
                    assert_eq!(first.1, reason);
                    assert_eq!(second.1, reason);
                    observer.request(Ping).await.unwrap(); // trapping observer survives
                    assert_eq!(dropped.load(Ordering::SeqCst), bombs);
                    assert_eq!(handled.load(Ordering::SeqCst), 0);
                    for reply in replies {
                        assert!(spawned_rt::tasks::timeout(LIMIT, reply)
                            .await
                            .unwrap()
                            .is_err());
                    }
                    observer.context().stop();
                    spawned_rt::tasks::timeout(LIMIT, observer.join())
                        .await
                        .unwrap();
                    assert!(
                        matches!(
                            events.try_recv(),
                            Err(mpsc::error::TryRecvError::Disconnected)
                        ),
                        "extra notification after observer cleanup"
                    );
                    assert!(system.shutdown(LIMIT).await.complete);
                }
            }
        }
        #[cfg(feature = "metrics")]
        {
            let snapshot = spawned_concurrency::metrics::snapshot();
            assert_eq!(snapshot.actors_active, 0);
            assert_eq!(snapshot.handlers_active, 0);
            assert_eq!(snapshot.mailbox_depth, 0);
        }
    });
    runtime.shutdown_timeout(LIMIT);
    println!("validated 30 queued panic cases");
}

#[test]
fn abandoned_receivers_and_committed_causes_are_safe() {
    subprocess::run(
        "abandoned_receiver_child",
        "validated abandoned receivers and causes",
    );
}
#[test]
#[ignore = "subprocess helper exercised by enabled parent"]
fn abandoned_receiver_child() {
    if std::env::var_os("SPAWNED_MAILBOX_DROP_CHILD").is_none() {
        return;
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        for polled in [false, true] {
            for state_panics in [false, true] {
                let (send, arrival) = oneshot::channel();
                let (shutdown, stopped) = std::sync::mpsc::channel();
                let (entered, active) = oneshot::channel();
                let (hold, gate) = oneshot::channel();
                let owner = std::thread::spawn(move || {
                    let mut builder = if polled { tokio::runtime::Builder::new_multi_thread() }
                        else { tokio::runtime::Builder::new_current_thread() };
                    let runtime = builder.worker_threads(1).enable_all().build().unwrap();
                    {
                        let _entered = runtime.enter();
                        let actor = Victim(Arc::default(), state_panics).start();
                        if polled { actor.send(Hold(entered, gate, false)).unwrap(); }
                        send.send(actor).unwrap();
                    }
                    stopped.recv_timeout(LIMIT).unwrap();
                    runtime.shutdown_timeout(LIMIT);
                });
                let actor = tokio::time::timeout(LIMIT, arrival).await.unwrap().unwrap();
                if polled { tokio::time::timeout(LIMIT, active).await.unwrap().unwrap(); }
                let dropped = Arc::new(AtomicUsize::new(0));
                let replies: Vec<_> = (0..2).map(|_| actor.request_raw(Bomb(dropped.clone())).unwrap()).collect();
                shutdown.send(()).unwrap();
                let reason = tokio::time::timeout(LIMIT, actor.wait_exit()).await.unwrap();
                if state_panics && !polled { assert!(matches!(reason, ExitReason::Panic(ref message) if message.contains("actor state destructor"))); }
                else { assert_eq!(reason, ExitReason::Kill); }
                assert_eq!(dropped.load(Ordering::SeqCst), 2);
                assert_eq!(actor.child_handle().mailbox_cleanup_failure().as_deref(), Some("queued message destructor"));
                for reply in replies { assert!(tokio::time::timeout(LIMIT, reply).await.unwrap().is_err()); }
                drop(hold);
                owner.join().unwrap();
            }
        }
        // A committed stop on a never-polled startup must also survive a
        // panicking actor-state destructor and two queued destructor failures.
        for committed in [ExitReason::Shutdown, ExitReason::Kill] {
            let owner = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            let actor = {
                let _entered = owner.enter();
                Victim(Arc::default(), true).start()
            };
            let dropped = Arc::new(AtomicUsize::new(0));
            for _ in 0..2 { actor.send(Bomb(dropped.clone())).unwrap(); }
            actor.context().stop_with_reason(committed.clone());
            owner.shutdown_background();
            assert_eq!(tokio::time::timeout(LIMIT, actor.wait_exit()).await.unwrap(), committed);
            assert_eq!(dropped.load(Ordering::SeqCst), 2);
        }
        for (managed, state_panics) in [(false, false), (false, true), (true, false), (true, true)] {
            for committed in [ExitReason::Shutdown, ExitReason::Kill, ExitReason::Panic("linked peer cause".into())] {
                let system = ActorSystem::new().unwrap();
                let group = system.async_group("preserve-cause", 1).unwrap();
                let actor = if managed { Victim(Arc::default(), state_panics).start_on(&group).unwrap() }
                    else { Victim(Arc::default(), state_panics).start() };
                let (entered, arrival) = oneshot::channel();
                let (release, gate) = oneshot::channel();
                actor.send(Hold(entered, gate, false)).unwrap();
                tokio::time::timeout(LIMIT, arrival).await.unwrap().unwrap();
                let dropped = Arc::new(AtomicUsize::new(0));
                for _ in 0..2 { actor.send(Bomb(dropped.clone())).unwrap(); }
                if matches!(committed, ExitReason::Panic(_)) {
                    let peer = Victim(Arc::default(), false).start();
                    actor.context().link(&peer.child_handle());
                    peer.context().stop_with_reason(committed.clone());
                    tokio::time::timeout(LIMIT, peer.wait_exit()).await.unwrap();
                } else { actor.context().stop_with_reason(committed.clone()); }
                release.send(()).unwrap();
                assert_eq!(tokio::time::timeout(LIMIT, actor.wait_exit()).await.unwrap(), committed);
                assert_eq!(dropped.load(Ordering::SeqCst), 2);
                assert_eq!(actor.child_handle().mailbox_cleanup_failure().as_deref(), Some("queued message destructor"));
                assert!(system.shutdown(LIMIT).await.complete);
            }
        }
        // An actor-state panic must not unwind through an unguarded mailbox.
        let actor = Victim(Arc::default(), true).start();
        let (entered, arrival) = oneshot::channel();
        let (release, gate) = oneshot::channel();
        actor.send(Hold(entered, gate, false)).unwrap();
        tokio::time::timeout(LIMIT, arrival).await.unwrap().unwrap();
        let dropped = Arc::new(AtomicUsize::new(0));
        for _ in 0..2 { actor.send(Bomb(dropped.clone())).unwrap(); }
        actor.context().stop(); release.send(()).unwrap();
        assert!(matches!(tokio::time::timeout(LIMIT, actor.wait_exit()).await.unwrap(), ExitReason::Panic(_)));
        assert_eq!(dropped.load(Ordering::SeqCst), 2);
    });
    runtime.shutdown_timeout(LIMIT);
    println!("validated abandoned receivers and causes");
}

#[test]
fn handler_panic_survives_state_and_queued_destructor_panics() {
    subprocess::run("primary_panic_child", "validated primary handler panic");
}
#[test]
#[ignore = "subprocess helper exercised by enabled parent"]
fn primary_panic_child() {
    if std::env::var_os("SPAWNED_MAILBOX_DROP_CHILD").is_none() {
        return;
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let actor = Victim(Arc::default(), true).start();
        let (send, mut events) = mpsc::unbounded_channel();
        let observer = Observer(send).start();
        observer.context().trap_exit(true);
        observer.context().link(&actor.child_handle());
        let _monitor = observer.context().monitor(&actor.child_handle());
        let (entered, arrival) = oneshot::channel();
        let (release, gate) = oneshot::channel();
        actor.send(Hold(entered, gate, true)).unwrap();
        tokio::time::timeout(LIMIT, arrival).await.unwrap().unwrap();
        let dropped = Arc::new(AtomicUsize::new(0));
        for _ in 0..2 {
            actor.send(Bomb(dropped.clone())).unwrap();
        }
        release.send(()).unwrap();
        let primary = ExitReason::Panic("panic in handler: original handler failure".into());
        assert_eq!(
            tokio::time::timeout(LIMIT, actor.wait_exit())
                .await
                .unwrap(),
            primary
        );
        let first = tokio::time::timeout(LIMIT, events.recv())
            .await
            .unwrap()
            .unwrap();
        let second = tokio::time::timeout(LIMIT, events.recv())
            .await
            .unwrap()
            .unwrap();
        assert_ne!(first.0, second.0);
        assert_eq!(first.1, primary);
        assert_eq!(second.1, primary);
        assert_eq!(dropped.load(Ordering::SeqCst), 2);
        observer.request(Ping).await.unwrap();
        observer.context().stop();
        tokio::time::timeout(LIMIT, observer.join()).await.unwrap();
    });
    println!("validated primary handler panic");
}
