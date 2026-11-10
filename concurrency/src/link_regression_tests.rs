//! Deterministic lifecycle regressions for links (including unpublished completion).
use crate::{
    child_handle::{ActorId, ChildHandle},
    error::ExitReason,
    link::{self, Exit},
    tasks::{Actor, ActorStart, Context},
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

struct Observer(Arc<Mutex<Vec<Exit>>>);
impl Actor for Observer {
    async fn exit_received(&mut self, exit: Exit, _: &Context<Self>) {
        self.0.lock().unwrap().push(exit);
    }
}
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn linking_in_post_drain_pre_completion_window_delivers_exit() {
    let rt = runtime();
    let events = Arc::new(Mutex::new(Vec::new()));
    let observer = {
        let _entered = rt.enter();
        Observer(events.clone()).start()
    };
    observer.context().trap_exit(true);
    let peer_id = ActorId::next();
    let peer_links = link::new_link_table();
    let (_completion_tx, completion_rx) = spawned_rt::tasks::watch::channel(None);
    // This is the actual state between target propagation and publication.
    let reason = ExitReason::Panic("closing before completion".into());
    link::propagate_exit(peer_id, &peer_links, &reason);
    let peer = ChildHandle::from_tasks(
        peer_id,
        spawned_rt::tasks::CancellationToken::new(),
        completion_rx,
        link::new_trap_exit_flag(),
        peer_links,
        link::new_linked_exit_reason(),
        Arc::new(|_| Ok(())),
    );
    observer.context().link(&peer);
    let delivered = rt.block_on(async {
        tokio::time::timeout(Duration::from_millis(250), async {
            loop {
                if !events.lock().unwrap().is_empty() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .is_ok()
    });
    observer.child_handle().stop();
    rt.block_on(observer.wait_exit());
    assert!(
        delivered,
        "exit lost between link-table drain and completion publication"
    );
    assert_eq!(
        *events.lock().unwrap(),
        vec![Exit {
            from: peer_id,
            reason
        }]
    );
}

struct Idler;
impl Actor for Idler {}
#[test]
fn never_polled_actor_propagates_runtime_shutdown_to_peer() {
    #[cfg(feature = "backend-experiments")]
    {
        let selector = std::env::var("SPAWNED_EXPERIMENT_SCHEDULER").unwrap_or_default();
        if selector == "sharded"
            || spawned_rt::tasks::experimental::dispatcher::policy_for_scheduler(&selector)
                .is_some()
        {
            return; // Those process-owned pools deliberately outlive the caller runtime.
        }
    }

    let owner = runtime();
    let survivor = runtime();
    let target = {
        let _entered = owner.enter();
        Idler.start()
    };
    let peer = {
        let _entered = survivor.enter();
        Idler.start()
    };
    peer.context().link(&target.child_handle());
    // The owner runtime never polls its actor. Dropping it must still close links.
    drop(owner);
    let result = survivor.block_on(async {
        tokio::time::timeout(Duration::from_millis(250), peer.wait_exit()).await
    });
    peer.child_handle().stop();
    survivor.block_on(peer.wait_exit());
    assert_eq!(
        result,
        Ok(ExitReason::Kill),
        "aborted actor failed to signal its live peer"
    );
}

struct Ping;
impl crate::message::Message for Ping {
    type Result = ();
}
impl crate::tasks::Handler<Ping> for Idler {
    async fn handle(&mut self, _: Ping, _: &Context<Self>) {}
}

#[test]
fn polled_actor_propagates_runtime_shutdown_to_peer() {
    #[cfg(feature = "backend-experiments")]
    {
        let selector = std::env::var("SPAWNED_EXPERIMENT_SCHEDULER").unwrap_or_default();
        if selector == "sharded"
            || spawned_rt::tasks::experimental::dispatcher::policy_for_scheduler(&selector)
                .is_some()
        {
            return;
        }
    }
    let owner = runtime();
    let survivor = runtime();
    let target = {
        let _entered = owner.enter();
        Idler.start()
    };
    owner.block_on(target.request(Ping)).unwrap();
    let peer = {
        let _entered = survivor.enter();
        Idler.start()
    };
    peer.context().link(&target.child_handle());
    drop(owner);
    let result = survivor
        .block_on(async { tokio::time::timeout(Duration::from_secs(1), peer.wait_exit()).await });
    peer.child_handle().stop();
    survivor.block_on(peer.wait_exit());
    assert_eq!(result, Ok(ExitReason::Kill));
}

struct MonitorObserver(Option<spawned_rt::tasks::oneshot::Sender<crate::Down>>);
impl Actor for MonitorObserver {}
impl crate::tasks::Handler<crate::Down> for MonitorObserver {
    async fn handle(&mut self, down: crate::Down, _: &Context<Self>) {
        self.0.take().unwrap().send(down).unwrap();
    }
}
#[test]
fn never_polled_actor_notifies_monitor_on_runtime_shutdown() {
    #[cfg(feature = "backend-experiments")]
    {
        let selector = std::env::var("SPAWNED_EXPERIMENT_SCHEDULER").unwrap_or_default();
        if selector == "sharded"
            || spawned_rt::tasks::experimental::dispatcher::policy_for_scheduler(&selector)
                .is_some()
        {
            return;
        }
    }
    let owner = runtime();
    let survivor = runtime();
    let target = {
        let _entered = owner.enter();
        Idler.start()
    };
    let (tx, rx) = spawned_rt::tasks::oneshot::channel();
    let observer = {
        let _entered = survivor.enter();
        MonitorObserver(Some(tx)).start()
    };
    {
        let _entered = survivor.enter();
        observer.context().monitor(&target.child_handle());
    }
    drop(owner);
    let result =
        survivor.block_on(async { tokio::time::timeout(Duration::from_secs(1), rx).await });
    observer.child_handle().stop();
    survivor.block_on(observer.wait_exit());
    assert_eq!(result.unwrap().unwrap().reason, ExitReason::Kill);
}

struct Stop;
impl crate::message::Message for Stop {
    type Result = ();
}
struct Crash;
impl crate::message::Message for Crash {
    type Result = ();
}
impl crate::tasks::Handler<Crash> for Idler {
    async fn handle(&mut self, _: Crash, _: &Context<Self>) {
        panic!("linked failure during another actor's stopped hook");
    }
}
struct Stopping {
    entered: Option<spawned_rt::tasks::oneshot::Sender<()>>,
    release: Option<spawned_rt::tasks::oneshot::Receiver<()>>,
}
impl Actor for Stopping {
    async fn stopped(&mut self, _: &Context<Self>) {
        self.entered.take().unwrap().send(()).unwrap();
        self.release.take().unwrap().await.unwrap();
    }
}
impl crate::tasks::Handler<Stop> for Stopping {
    async fn handle(&mut self, _: Stop, ctx: &Context<Self>) {
        ctx.stop();
    }
}
#[test]
fn late_link_failure_cannot_relabel_an_already_requested_normal_stop() {
    let rt = runtime();
    let (entered_tx, entered_rx) = spawned_rt::tasks::oneshot::channel();
    let (release_tx, release_rx) = spawned_rt::tasks::oneshot::channel();
    let (stopping, failing, survivor) = {
        let _entered = rt.enter();
        (
            Stopping {
                entered: Some(entered_tx),
                release: Some(release_rx),
            }
            .start(),
            Idler.start(),
            Idler.start(),
        )
    };
    stopping.context().link(&failing.child_handle());
    stopping.context().link(&survivor.child_handle());
    stopping.send(Stop).unwrap();
    rt.block_on(entered_rx).unwrap();
    failing.send(Crash).unwrap();
    assert!(matches!(
        rt.block_on(failing.wait_exit()),
        ExitReason::Panic(_)
    ));
    release_tx.send(()).unwrap();
    assert_eq!(rt.block_on(stopping.wait_exit()), ExitReason::Normal);
    assert!(survivor.child_handle().is_alive());
    survivor.child_handle().stop();
    rt.block_on(survivor.wait_exit());
}

struct LinkedAtStart {
    parent: ActorId,
    seen: Option<spawned_rt::tasks::oneshot::Sender<bool>>,
}
impl Actor for LinkedAtStart {
    async fn started(&mut self, ctx: &Context<Self>) {
        let handle = ctx.actor_ref().child_handle();
        self.seen
            .take()
            .unwrap()
            .send(link::has_link(handle.links(), self.parent))
            .unwrap();
        ctx.stop();
    }
}
#[test]
fn start_linked_registers_before_async_started() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let parent = {
        let _entered = rt.enter();
        Idler.start()
    };
    for _ in 0..32 {
        let (tx, rx) = spawned_rt::tasks::oneshot::channel();
        let child = {
            let _entered = rt.enter();
            LinkedAtStart {
                parent: parent.id(),
                seen: Some(tx),
            }
            .start_linked(&parent.context())
        };
        assert!(rt.block_on(rx).unwrap());
        assert_eq!(rt.block_on(child.wait_exit()), ExitReason::Normal);
    }
    parent.child_handle().stop();
    rt.block_on(parent.wait_exit());
}

#[test]
fn never_polled_monitor_owner_releases_waiter_and_refuses_late_registration() {
    #[cfg(feature = "backend-experiments")]
    {
        let selector = std::env::var("SPAWNED_EXPERIMENT_SCHEDULER").unwrap_or_default();
        if selector == "sharded"
            || spawned_rt::tasks::experimental::dispatcher::policy_for_scheduler(&selector)
                .is_some()
        {
            return;
        }
    }
    let owner_rt = runtime();
    let survivor = runtime();
    let (tx, _rx) = spawned_rt::tasks::oneshot::channel();
    let owner = {
        let _entered = owner_rt.enter();
        MonitorObserver(Some(tx)).start()
    };
    let target = {
        let _entered = survivor.enter();
        Idler.start()
    };
    let handle = target.child_handle();
    let probe = handle.lifetime_probe();
    {
        let _entered = survivor.enter();
        owner.context().monitor(&handle);
    }
    drop(handle);
    drop(owner_rt);
    survivor.block_on(async {
        tokio::time::timeout(Duration::from_secs(1), async {
            while probe.upgrade().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    });
    let handle = target.child_handle();
    let probe = handle.lifetime_probe();
    {
        let _entered = survivor.enter();
        owner.context().monitor(&handle);
    }
    drop(handle);
    assert!(
        probe.upgrade().is_none(),
        "dead owner retained a new monitor waiter"
    );
    target.child_handle().stop();
    survivor.block_on(target.wait_exit());
    assert_eq!(owner.child_handle().exit_reason(), Some(ExitReason::Kill));
}

#[test]
fn linked_runtime_abort_is_trappable() {
    #[cfg(feature = "backend-experiments")]
    {
        let selector = std::env::var("SPAWNED_EXPERIMENT_SCHEDULER").unwrap_or_default();
        if selector == "sharded"
            || spawned_rt::tasks::experimental::dispatcher::policy_for_scheduler(&selector)
                .is_some()
        {
            return;
        }
    }
    let owner = runtime();
    let survivor = runtime();
    let events = Arc::new(Mutex::new(Vec::new()));
    let target = {
        let _entered = owner.enter();
        Idler.start()
    };
    let peer = {
        let _entered = survivor.enter();
        Observer(events.clone()).start()
    };
    peer.context().trap_exit(true);
    peer.context().link(&target.child_handle());
    drop(owner);
    survivor.block_on(async {
        tokio::time::timeout(crate::test_wait::WAIT, async {
            while events.lock().unwrap().is_empty() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
    });
    assert!(peer.child_handle().is_alive());
    assert_eq!(
        *events.lock().unwrap(),
        vec![Exit {
            from: target.id(),
            reason: ExitReason::Kill
        }]
    );
    peer.child_handle().stop();
    assert_eq!(survivor.block_on(peer.wait_exit()), ExitReason::Normal);
}

#[test]
fn never_polled_completion_waits_for_actor_payload_destruction() {
    #[cfg(feature = "backend-experiments")]
    {
        let selector = std::env::var("SPAWNED_EXPERIMENT_SCHEDULER").unwrap_or_default();
        if selector == "sharded"
            || spawned_rt::tasks::experimental::dispatcher::policy_for_scheduler(&selector)
                .is_some()
        {
            return;
        }
    }
    struct BlockingDrop {
        entered: std::sync::mpsc::Sender<()>,
        release: std::sync::mpsc::Receiver<()>,
    }
    impl Actor for BlockingDrop {}
    impl Drop for BlockingDrop {
        fn drop(&mut self) {
            let _ = self.entered.send(());
            let _ = self.release.recv();
        }
    }
    let owner = runtime();
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let actor = {
        let _entered = owner.enter();
        BlockingDrop {
            entered: entered_tx,
            release: release_rx,
        }
        .start()
    };
    let shutdown = std::thread::spawn(move || drop(owner));
    let entered = entered_rx.recv_timeout(Duration::from_secs(2));
    let before = actor.child_handle().exit_reason();
    let _ = release_tx.send(());
    shutdown.join().unwrap();
    entered.unwrap();
    assert_eq!(
        before, None,
        "completion published while the actor's destructor was still running"
    );
    assert_eq!(actor.child_handle().exit_reason(), Some(ExitReason::Kill));
}

#[test]
fn linked_runtime_abort_cancels_an_idle_threads_actor() {
    #[cfg(feature = "backend-experiments")]
    {
        let selector = std::env::var("SPAWNED_EXPERIMENT_SCHEDULER").unwrap_or_default();
        if selector == "sharded"
            || spawned_rt::tasks::experimental::dispatcher::policy_for_scheduler(&selector)
                .is_some()
        {
            return;
        }
    }
    use crate::threads::{Actor as ThreadActor, ActorStart as ThreadStart};
    struct ThreadPeer;
    impl ThreadActor for ThreadPeer {}
    impl crate::threads::Handler<Ping> for ThreadPeer {
        fn handle(&mut self, _: Ping, _: &crate::threads::Context<Self>) {}
    }
    let owner = runtime();
    let waiter = runtime();
    let target = {
        let _entered = owner.enter();
        Idler.start()
    };
    let peer = ThreadPeer.start();
    peer.context().link(&target.child_handle());
    drop(owner);
    let result = waiter.block_on(async {
        tokio::time::timeout(
            Duration::from_secs(2),
            peer.child_handle().wait_exit_async(),
        )
        .await
    });
    peer.child_handle().stop();
    if result.is_err() {
        let _ = peer.send(Ping); // Wake cleanup even if the cancellation callback regresses.
    }
    peer.child_handle().wait_exit_blocking();
    assert_eq!(result, Ok(ExitReason::Kill));
}

#[test]
fn dropping_runtime_with_large_polled_link_cycle_reclaims_every_actor() {
    #[cfg(feature = "backend-experiments")]
    {
        let selector = std::env::var("SPAWNED_EXPERIMENT_SCHEDULER").unwrap_or_default();
        if selector == "sharded"
            || spawned_rt::tasks::experimental::dispatcher::policy_for_scheduler(&selector)
                .is_some()
        {
            return;
        }
    }
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Node(Arc<AtomicUsize>);
    impl Actor for Node {}
    impl crate::tasks::Handler<Ping> for Node {
        async fn handle(&mut self, _: Ping, _: &Context<Self>) {}
    }
    impl Drop for Node {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let owner = runtime();
    let dropped = Arc::new(AtomicUsize::new(0));
    let actors: Vec<_> = {
        let _entered = owner.enter();
        (0..4096).map(|_| Node(dropped.clone()).start()).collect()
    };
    for i in 0..actors.len() {
        actors[i]
            .context()
            .link(&actors[(i + 1) % actors.len()].child_handle());
    }
    owner.block_on(async {
        for actor in &actors {
            actor.request(Ping).await.unwrap();
        }
    });
    drop(owner);
    assert_eq!(dropped.load(Ordering::SeqCst), actors.len());
    assert!(actors
        .iter()
        .all(|a| a.child_handle().exit_reason() == Some(ExitReason::Kill)));
}
