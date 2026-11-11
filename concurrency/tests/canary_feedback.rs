#![cfg(feature = "async")]
// Tests that need OS threads are compiled out under spawned_sim.
#![cfg_attr(spawned_sim, allow(unused_imports, dead_code))]
use spawned_concurrency::{
    message::Message,
    supervision::{
        Backoff, ChildSpec, ChildState, ChildUnavailable, RestartMode, SupervisorConfig,
        SupervisorStrategy,
    },
    system::ActorSystem,
    tasks::{Actor, ActorStart, Context, Handler},
    ExitReason,
};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::oneshot;
const LIMIT: Duration = Duration::from_secs(10);

struct Hold {
    entered: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
}
impl Message for Hold {
    type Result = ();
}
struct Quit(ExitReason);
impl Message for Quit {
    type Result = ();
}
struct Worker;
impl Actor for Worker {}
impl Handler<Hold> for Worker {
    async fn handle(
        &mut self,
        hold: Hold,
        reply: spawned_concurrency::reply::PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
        let _ = hold.entered.send(());
        let _ = hold.release.await;
        reply.reply(());
    }
}
impl Handler<Quit> for Worker {
    async fn handle(
        &mut self,
        quit: Quit,
        reply: spawned_concurrency::reply::PendingReply<'_, ()>,
        ctx: &Context<Self>,
    ) {
        ctx.stop_with_reason(quit.0);
        reply.reply(());
    }
}

#[spawned_macros::sim_test]
async fn unfinished_report_identifies_child_and_active_turn() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("canary-diagnostics", 1).unwrap();
    let mut builder = system.supervisor(SupervisorConfig::default()).unwrap();
    let child = builder
        .add(ChildSpec::new("canary-router", &group, |_| async {
            Ok::<_, String>(Worker)
        }))
        .unwrap();
    let _supervisor = builder.start().await.unwrap();
    let actor = child.current().unwrap();
    let (entered, arrival) = oneshot::channel();
    let (release, gate) = oneshot::channel();
    actor
        .send(Hold {
            entered,
            release: gate,
        })
        .unwrap();
    spawned_rt::tasks::timeout(LIMIT, arrival)
        .await
        .unwrap()
        .unwrap();
    let report = system.shutdown(Duration::from_millis(10)).await;
    assert!(!report.complete);
    let detail = report
        .unfinished_actor_details
        .iter()
        .find(|d| d.id == actor.id())
        .unwrap();
    assert!(report.unfinished_actors.contains(&detail.id));
    assert_eq!(detail.name.as_deref(), Some("canary-router"));
    assert_eq!(detail.actor_type, std::any::type_name::<Worker>());
    #[cfg(feature = "diagnostics")]
    {
        assert_eq!(detail.message_type, Some(std::any::type_name::<Hold>()));
        assert!(detail.turn_age.unwrap() >= Duration::from_millis(10));
    }
    #[cfg(not(feature = "diagnostics"))]
    {
        assert_eq!(detail.message_type, None);
        assert_eq!(detail.turn_age, None);
    }
    release.send(()).unwrap();
    let report = report.wait(LIMIT).await;
    assert!(report.complete);
    assert!(report.unfinished_actor_details.is_empty());
    assert_eq!(
        actor.child_handle().exit_reason(),
        Some(ExitReason::Shutdown)
    );
}

// Not simulated: uses spawn_blocking.
#[cfg(not(spawned_sim))]
#[spawned_macros::sim_test]
async fn completed_turn_is_not_reported_while_helper_cleanup_remains() {
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("canary-helper", 1).unwrap();
    let actor = Worker.start_on(&group).unwrap();
    actor.ready().await.unwrap();
    actor.context().register_name("canary-standalone").unwrap();
    let (entered, arrival) = oneshot::channel();
    let (release, gate) = std::sync::mpsc::channel();
    let helper = actor
        .context()
        .spawn_blocking_helper(move || {
            let _ = entered.send(());
            gate.recv_timeout(LIMIT)
                .expect("test did not release blocking helper");
        })
        .unwrap();
    spawned_rt::tasks::timeout(LIMIT, arrival)
        .await
        .unwrap()
        .unwrap();
    // The actor completes a turn and exits, but the helper retains its scope.
    actor.request(Quit(ExitReason::Shutdown)).await.unwrap();
    spawned_rt::tasks::timeout(LIMIT, actor.join())
        .await
        .unwrap();
    let report = system.shutdown(Duration::from_millis(10)).await;
    assert!(!report.complete);
    let detail = report
        .unfinished_actor_details
        .iter()
        .find(|d| d.id == actor.id())
        .unwrap();
    assert_eq!(detail.name.as_deref(), Some("canary-standalone"));
    assert_eq!(detail.message_type, None);
    assert_eq!(detail.turn_age, None);
    assert!(report
        .exits
        .iter()
        .any(|e| e.id == actor.id() && e.reason == ExitReason::Shutdown));
    release.send(()).unwrap();
    helper.join().await.unwrap();
    assert!(report.wait(LIMIT).await.complete);
}

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
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
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

#[spawned_macros::sim_test]
async fn supervision_logs_restart_budget_exhaustion_and_give_up() {
    let capture = Capture::default();
    // One thread (and one run per seed) under spawned_sim: a scoped default.
    #[cfg(spawned_sim)]
    let _subscriber = tracing::subscriber::set_default(capture.clone());
    #[cfg(not(spawned_sim))]
    tracing::subscriber::set_global_default(capture.clone()).unwrap();
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("canary-tracing", 1).unwrap();
    let mut builder = system
        .supervisor(SupervisorConfig {
            max_restarts: 1,
            backoff: Backoff {
                initial: Duration::ZERO,
                maximum: Duration::ZERO,
                jitter: false,
                ..Backoff::default()
            },
            ..SupervisorConfig::default()
        })
        .unwrap();
    let child = builder
        .add(
            ChildSpec::new("canary-retry", &group, |_| async {
                Ok::<_, String>(Worker)
            })
            .restart(RestartMode::Transient),
        )
        .unwrap();
    let supervisor = builder.start().await.unwrap();
    child
        .current()
        .unwrap()
        .request(Quit(ExitReason::Kill))
        .await
        .unwrap();
    let replacement = spawned_rt::tasks::timeout(LIMIT, child.wait_for_generation(1))
        .await
        .unwrap()
        .unwrap();
    replacement.request(Quit(ExitReason::Kill)).await.unwrap();
    spawned_rt::tasks::timeout(LIMIT, supervisor.join())
        .await
        .unwrap();
    assert!(supervisor
        .failure()
        .unwrap()
        .contains("restart budget exhausted"));
    let mut failing = system.supervisor(SupervisorConfig::default()).unwrap();
    failing
        .add(
            ChildSpec::<Worker>::new("canary-give-up", &group, |_| async {
                Err::<Worker, _>("cannot initialize")
            })
            .restart(RestartMode::Temporary),
        )
        .unwrap();
    assert!(failing.start().await.is_err());
    // Not simulated: the cohort case blocks a spawn_blocking helper.
    #[cfg(not(spawned_sim))]
    {
        // The healthy collateral child takes the last retry grant. Its failing
        // sibling must give up before a 60-second backoff, and lookups must report
        // owner failure even while the healthy sibling's owned cleanup is pending.
        let mut cohort = system
            .supervisor(SupervisorConfig {
                strategy: SupervisorStrategy::OneForAll,
                max_restarts: 1,
                backoff: Backoff {
                    initial: Duration::from_secs(60),
                    maximum: Duration::from_secs(60),
                    jitter: false,
                    ..Backoff::default()
                },
                ..SupervisorConfig::default()
            })
            .unwrap();
        let (release, gate) = std::sync::mpsc::channel();
        let gate = Arc::new(Mutex::new(Some(gate)));
        let healthy = cohort
            .add(
                ChildSpec::new("canary-healthy", &group, move |ctx| {
                    let gate = gate.clone();
                    async move {
                        if ctx.generation() == 2 {
                            let gate = gate.lock().unwrap().take().unwrap();
                            let (entered, arrival) = oneshot::channel();
                            ctx.spawn_blocking_helper(move || {
                                entered.send(()).unwrap();
                                gate.recv_timeout(LIMIT).unwrap();
                            })
                            .unwrap();
                            arrival.await.unwrap();
                        }
                        Ok::<_, String>(Worker)
                    }
                })
                .shutdown_timeout(LIMIT),
            )
            .unwrap();
        let origin = cohort
            .add(ChildSpec::new("canary-origin", &group, |_| async {
                Ok::<_, String>(Worker)
            }))
            .unwrap();
        let cohort = cohort.start().await.unwrap();
        origin
            .current()
            .unwrap()
            .request(Quit(ExitReason::Panic("canary failure".into())))
            .await
            .unwrap();
        assert!(matches!(
            spawned_rt::tasks::timeout(LIMIT, origin.wait_for_generation(1))
                .await
                .unwrap(),
            Err(ChildUnavailable(ChildState::Failed))
        ));
        assert!(matches!(
            origin.current(),
            Err(ChildUnavailable(ChildState::Failed))
        ));
        assert!(matches!(
            healthy.current(),
            Err(ChildUnavailable(ChildState::Failed))
        ));
        assert_eq!(healthy.snapshot().generation, 2);
        release.send(()).unwrap();
        spawned_rt::tasks::timeout(LIMIT, cohort.join())
            .await
            .unwrap();
    }
    assert!(system.shutdown(LIMIT).await.complete);
    let events = capture.0.lock().unwrap();
    for (event, level, generation) in [
        ("restart_scheduled", tracing::Level::INFO, "1"),
        ("restart_started", tracing::Level::INFO, "2"),
        ("restart_budget_exhausted", tracing::Level::WARN, "2"),
    ] {
        assert!(
            events.iter().any(|(l, f)| *l == level
                && f.get("event").map(String::as_str) == Some(event)
                && f.get("child").map(String::as_str) == Some("canary-retry")
                && f.get("generation").map(String::as_str) == Some(generation)
                && f.get("reason").is_some_and(|r| r.contains("Kill"))),
            "missing {event}: {events:?}"
        );
    }
    assert!(events
        .iter()
        .any(|(level, fields)| *level == tracing::Level::WARN
            && fields.get("event").map(String::as_str) == Some("supervision_give_up")
            && fields.get("child").map(String::as_str) == Some("canary-give-up")
            && fields
                .get("error")
                .is_some_and(|e| e.contains("non-restartable child"))));
    let exhausted: Vec<_> = events
        .iter()
        .filter(|(_, fields)| {
            fields.get("event").map(String::as_str) == Some("restart_budget_exhausted")
        })
        .collect();
    assert_eq!(
        exhausted.len(),
        if cfg!(spawned_sim) { 1 } else { 2 },
        "duplicate or missing exhaustion events: {events:?}"
    );
    if !cfg!(spawned_sim) {
        assert!(exhausted
            .iter()
            .any(|(_, fields)| fields.get("child").map(String::as_str) == Some("canary-origin")));
    }
    assert!(!exhausted
        .iter()
        .any(|(_, fields)| fields.get("child").map(String::as_str) == Some("canary-healthy")));
    assert!(!events
        .iter()
        .any(
            |(_, fields)| fields.get("child").map(String::as_str) == Some("canary-origin")
                && fields.get("event").map(String::as_str) == Some("restart_scheduled")
        ));
}

#[spawned_macros::sim_test]
async fn unmanaged_helpers_return_a_specific_error_without_executing_work() {
    let actor = Worker.start();
    assert!(matches!(
        actor.context().spawn_helper(async {}),
        Err(spawned_concurrency::system::SystemError::UnmanagedActor)
    ));
    assert!(matches!(
        actor
            .context()
            .spawn_blocking_helper(|| panic!("must not run")),
        Err(spawned_concurrency::system::SystemError::UnmanagedActor)
    ));
    actor.context().stop();
    spawned_rt::tasks::timeout(LIMIT, actor.join())
        .await
        .unwrap();
}

#[spawned_macros::sim_test]
async fn panicked_turn_is_cleared_before_stopped_hook() {
    struct Panicking {
        entered: Option<oneshot::Sender<()>>,
        release: Option<oneshot::Receiver<()>>,
    }
    impl Actor for Panicking {
        async fn stopped(&mut self, _: &Context<Self>) {
            let _ = self.entered.take().unwrap().send(());
            let _ = self.release.take().unwrap().await;
        }
    }
    impl Handler<Quit> for Panicking {
        async fn handle(
            &mut self,
            _: Quit,
            _reply: spawned_concurrency::reply::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            panic!("diagnostic turn cleanup regression");
        }
    }
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("canary-panic", 1).unwrap();
    let (entered, arrival) = oneshot::channel();
    let (release, gate) = oneshot::channel();
    let actor = Panicking {
        entered: Some(entered),
        release: Some(gate),
    }
    .start_on(&group)
    .unwrap();
    actor.ready().await.unwrap();
    assert!(actor.request(Quit(ExitReason::Kill)).await.is_err());
    spawned_rt::tasks::timeout(LIMIT, arrival)
        .await
        .unwrap()
        .unwrap();
    let report = system.shutdown(Duration::from_millis(10)).await;
    let detail = report
        .unfinished_actor_details
        .iter()
        .find(|d| d.id == actor.id())
        .unwrap();
    assert_eq!(detail.message_type, None);
    assert_eq!(detail.turn_age, None);
    release.send(()).unwrap();
    assert!(report.wait(LIMIT).await.complete);
    // The system's kill at grace reached it inside stopped(), before its
    // death point: Killed replaces the panic (E1).
    assert_eq!(actor.child_handle().exit_reason(), Some(ExitReason::Killed));
}

#[cfg(feature = "diagnostics")]
#[spawned_macros::sim_test]
async fn trapped_exit_diagnostics_use_the_public_signal_name() {
    struct Observer {
        entered: Option<oneshot::Sender<()>>,
        release: Option<oneshot::Receiver<()>>,
    }
    impl Actor for Observer {
        async fn started(&mut self, ctx: &Context<Self>) {
            ctx.trap_exit(true);
        }
        async fn exit_received(&mut self, _: spawned_concurrency::Exit, _: &Context<Self>) {
            self.entered.take().unwrap().send(()).unwrap();
            self.release.take().unwrap().await.unwrap();
        }
    }
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("exit-diagnostic", 1).unwrap();
    let (entered, arrival) = oneshot::channel();
    let (release, gate) = oneshot::channel();
    let observer = Observer {
        entered: Some(entered),
        release: Some(gate),
    }
    .start_on(&group)
    .unwrap();
    observer.ready().await.unwrap();
    let source = Worker.start_on(&group).unwrap();
    source.ready().await.unwrap();
    observer.context().link(&source.child_handle());
    source
        .child_handle()
        .stop_with_reason(ExitReason::Panic("reported failure".into()));
    spawned_rt::tasks::timeout(LIMIT, arrival)
        .await
        .unwrap()
        .unwrap();
    let report = system.shutdown(Duration::from_millis(10)).await;
    let detail = report
        .unfinished_actor_details
        .iter()
        .find(|d| d.id == observer.id())
        .unwrap();
    assert_eq!(detail.message_type, Some("Exit"));
    release.send(()).unwrap();
    assert!(report.wait(LIMIT).await.complete);
}
