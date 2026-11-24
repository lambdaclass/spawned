//! `spawned::crash` reports (docs/design/introspection.md, "Structured
//! reports"; OTP's proc_lib crash report): emitted for an abnormal reason the
//! actor produced itself (a panic in `started()`, a handler or `stopped()`;
//! a self-exit with an abnormal reason), never for a clean exit or for a
//! reason a kill or an untrapped exit signal imposed. The field names are a
//! contract. Both modes.
//!
//! One global capturing subscriber for this binary, installed before any
//! actor runs in each test; tests filter events by actor id.
#![cfg(not(spawned_sim))]

use spawned_concurrency::message::Message;
use spawned_concurrency::{ActorId, ExitReason, PendingReply};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

type Fields = BTreeMap<String, String>;

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<(String, tracing::Level, Fields)>>>);
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
            fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
                self.0.insert(field.name().to_owned(), value.to_string());
            }
        }
        let mut visitor = Visitor(Fields::new());
        event.record(&mut visitor);
        let meta = event.metadata();
        self.0
            .lock()
            .unwrap()
            .push((meta.target().to_owned(), *meta.level(), visitor.0));
    }
}

fn capture() -> &'static Capture {
    static CAPTURE: OnceLock<Capture> = OnceLock::new();
    CAPTURE.get_or_init(|| {
        let capture = Capture::default();
        tracing::subscriber::set_global_default(capture.clone()).unwrap();
        capture
    })
}

/// The crash reports of `actor`.
fn reports(actor: ActorId) -> Vec<Fields> {
    capture()
        .0
        .lock()
        .unwrap()
        .iter()
        .filter(|(target, level, f)| {
            target == "spawned::crash"
                && *level == tracing::Level::ERROR
                && f.get("actor_id") == Some(&actor.to_string())
        })
        .map(|(_, _, f)| f.clone())
        .collect()
}

/// Reports are emitted before publication, so after `wait_exit` one is
/// either there or never comes.
fn one_report(actor: ActorId) -> Fields {
    let found = reports(actor);
    assert_eq!(
        found.len(),
        1,
        "expected one crash report for {actor}: {found:?}"
    );
    found.into_iter().next().unwrap()
}

struct Boom;
impl Message for Boom {
    type Result = ();
}
struct Quit(ExitReason);
impl Message for Quit {
    type Result = ();
}

#[cfg(feature = "async")]
mod tasks_mode {
    use super::*;

    const CONTRACT: [&str; 10] = [
        "actor_id",
        "system",
        "actor_type",
        "label",
        "name",
        "reason",
        "ancestors",
        "generation",
        "group",
        "links",
    ];

    use spawned_concurrency::supervision::{ChildSpec, SupervisorConfig};
    use spawned_concurrency::system::ActorSystem;
    use spawned_concurrency::tasks::{Actor, ActorStart, Context, Handler};

    #[derive(Default)]
    struct Crashy {
        panic_in_started: bool,
        panic_in_stopped: bool,
    }
    impl Actor for Crashy {
        async fn started(&mut self, ctx: &Context<Self>) {
            ctx.set_label("crashy");
            if self.panic_in_started {
                panic!("boom in started");
            }
        }
        async fn stopped(&mut self, _: &Context<Self>) {
            if self.panic_in_stopped {
                panic!("boom in stopped");
            }
        }
    }
    impl Handler<Boom> for Crashy {
        async fn handle(&mut self, _: Boom, _: PendingReply<'_, ()>, _: &Context<Self>) {
            panic!("boom in handler");
        }
    }
    impl Handler<Quit> for Crashy {
        async fn handle(&mut self, msg: Quit, reply: PendingReply<'_, ()>, ctx: &Context<Self>) {
            ctx.stop_with_reason(msg.0);
            reply.reply(());
        }
    }

    #[tokio::test]
    async fn handler_panic_is_reported_with_the_contract_fields() {
        capture();
        let peer = Crashy::default().spawn();
        let actor = Crashy::default().spawn();
        actor.context().link(&peer.child_handle());
        actor.send(Boom).unwrap();
        assert!(matches!(actor.wait_exit().await, ExitReason::Panic(_)));
        let report = one_report(actor.id());
        for field in [
            "actor_id",
            "actor_type",
            "label",
            "reason",
            "ancestors",
            "links",
        ] {
            assert!(report.contains_key(field), "missing {field}: {report:?}");
        }
        assert!(report
            .keys()
            .all(|k| CONTRACT.contains(&k.as_str()) || k == "message"));
        assert!(report["actor_type"].ends_with("Crashy"));
        assert_eq!(report["label"], "crashy");
        assert!(report["reason"].contains("boom in handler"), "{report:?}");
        assert_eq!(report["links"], peer.id().to_string());
        // The linked peer was ended by an untrapped signal: no report.
        assert!(matches!(peer.wait_exit().await, ExitReason::Panic(_)));
        assert!(reports(peer.id()).is_empty());
    }

    #[tokio::test]
    async fn panics_in_started_and_stopped_are_reported() {
        capture();
        let started = Crashy {
            panic_in_started: true,
            ..Default::default()
        }
        .spawn();
        assert!(matches!(started.wait_exit().await, ExitReason::Panic(_)));
        assert!(one_report(started.id())["reason"].contains("boom in started"));
        let stopped = Crashy {
            panic_in_stopped: true,
            ..Default::default()
        }
        .spawn();
        stopped.child_handle().stop();
        assert!(matches!(stopped.wait_exit().await, ExitReason::Panic(_)));
        assert!(one_report(stopped.id())["reason"].contains("boom in stopped"));
    }

    #[tokio::test]
    async fn clean_exits_kills_and_signals_are_not_reported() {
        capture();
        for reason in [
            ExitReason::Normal,
            ExitReason::Shutdown(None),
            ExitReason::shutdown_with("bye"),
        ] {
            let actor = Crashy::default().spawn();
            actor.request(Quit(reason.clone())).await.unwrap();
            assert_eq!(actor.wait_exit().await, reason);
            assert!(reports(actor.id()).is_empty());
        }
        let killed = Crashy::default().spawn();
        killed.child_handle().kill();
        assert_eq!(killed.wait_exit().await, ExitReason::Killed);
        assert!(reports(killed.id()).is_empty());
        let signalled = Crashy::default().spawn();
        signalled.child_handle().exit(ExitReason::Kill.clone());
        let signalled2 = Crashy::default().spawn();
        signalled2
            .child_handle()
            .exit(ExitReason::Panic("remote".into()));
        assert_eq!(
            signalled2.wait_exit().await,
            ExitReason::Panic("remote".into())
        );
        assert!(reports(signalled2.id()).is_empty());
        signalled.wait_exit().await;
        assert!(reports(signalled.id()).is_empty());
    }

    #[tokio::test]
    async fn self_exit_with_an_abnormal_reason_is_reported() {
        capture();
        let actor = Crashy::default().spawn();
        actor.request(Quit(ExitReason::Kill)).await.unwrap();
        assert_eq!(actor.wait_exit().await, ExitReason::Kill);
        assert_eq!(one_report(actor.id())["reason"], "kill");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn supervised_report_names_system_group_generation_and_ancestors() {
        capture();
        let system = ActorSystem::new().unwrap();
        let group = system.async_group("crash-group", 1).unwrap();
        let mut builder = system.supervisor(SupervisorConfig::default()).unwrap();
        let child = builder
            .add(ChildSpec::new("crashy", &group, |_| async {
                Ok::<_, String>(Crashy::default())
            }))
            .unwrap();
        let _sup = builder.start().await.unwrap();
        let first = child.wait_ready().await.unwrap();
        first.send(Boom).unwrap();
        child.wait_for_generation(1).await.unwrap();
        let report = one_report(first.id());
        assert_eq!(report["system"], system.id().to_string());
        assert_eq!(report["group"], "crash-group");
        assert_eq!(report["generation"], "1");
        assert_eq!(report["ancestors"], "supervisor child crashy");
        assert!(system.shutdown(LIMIT).await.complete);
    }

    const LIMIT: Duration = Duration::from_secs(5);
}

/// A status callback's panic: the caller gets `CallbackFailed`, and a
/// `spawned::status` WARN event carries the message with the actor's id,
/// type and label so it can be found.
#[cfg(feature = "async")]
mod status_log {
    use super::*;
    use spawned_concurrency::tasks::{Actor, ActorStart, Context};
    use spawned_concurrency::ActorError;

    struct Labelled;
    impl Actor for Labelled {
        async fn started(&mut self, ctx: &Context<Self>) {
            ctx.set_label("peer 0x77");
        }
    }

    #[tokio::test]
    async fn callback_failure_is_logged_with_the_actor() {
        capture();
        let actor = Labelled.start().await.unwrap();
        let failed = actor.get_state(|_| panic!("status callback boom")).await;
        assert!(
            matches!(failed, Err(ActorError::CallbackFailed)),
            "{failed:?}"
        );
        let id = actor.id().to_string();
        let logged: Vec<Fields> = capture()
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|(target, level, f)| {
                target == "spawned::status"
                    && *level == tracing::Level::WARN
                    && f.get("actor_id") == Some(&id)
            })
            .map(|(_, _, f)| f.clone())
            .collect();
        assert_eq!(logged.len(), 1, "{logged:?}");
        assert_eq!(logged[0]["label"], "peer 0x77");
        assert!(logged[0]["actor_type"].ends_with("Labelled"));
        assert!(logged[0]["error"].contains("status callback boom"));
        actor.child_handle().stop();
        actor.join().await;
    }
}

/// `spawned::supervisor` reports (OTP's SASL supervisor reports):
/// `child_terminated` (unexpected exit), `child_restarted`, `start_failed`,
/// `restart_intensity_reached`, `shutdown_error` (killed after its shutdown
/// time, then stuck if the kill does not end it), with the contract fields.
#[cfg(feature = "async")]
mod supervisor_reports {
    use super::*;
    use spawned_concurrency::supervision::{Backoff, ChildSpec, SupervisorConfig};
    use spawned_concurrency::system::ActorSystem;
    use spawned_concurrency::tasks::{Actor, Context, Handler};

    const LIMIT: Duration = Duration::from_secs(5);
    const FIELDS: [&str; 9] = [
        "supervisor",
        "child",
        "generation",
        "reason",
        "restarts_in_window",
        "max_restarts",
        "window_ms",
        "action",
        "event",
    ];

    /// `(level, fields)` of every report of supervisor `name`.
    fn of(name: &str) -> Vec<(tracing::Level, Fields)> {
        capture()
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|(target, _, f)| {
                target == "spawned::supervisor"
                    && f.get("supervisor").map(String::as_str) == Some(name)
            })
            .map(|(_, level, f)| (*level, f.clone()))
            .collect()
    }
    fn event<'a>(
        reports: &'a [(tracing::Level, Fields)],
        name: &str,
    ) -> Vec<&'a (tracing::Level, Fields)> {
        reports.iter().filter(|(_, f)| f["event"] == name).collect()
    }
    fn config(name: &str, max_restarts: usize) -> SupervisorConfig {
        SupervisorConfig::default()
            .name(name)
            .max_restarts(max_restarts)
            .backoff(
                Backoff::default()
                    .initial(Duration::from_millis(1))
                    .maximum(Duration::from_millis(2))
                    .jitter(false),
            )
    }

    struct Worker;
    impl Actor for Worker {}
    impl Handler<Boom> for Worker {
        async fn handle(&mut self, _: Boom, _: PendingReply<'_, ()>, _: &Context<Self>) {
            panic!("worker boom");
        }
    }
    /// Blocks its thread until released: misses any shutdown time.
    struct Wedge(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>);
    impl Message for Wedge {
        type Result = ();
    }
    impl Handler<Wedge> for Worker {
        async fn handle(&mut self, msg: Wedge, _: PendingReply<'_, ()>, _: &Context<Self>) {
            msg.0.send(()).unwrap();
            let _ = msg.1.recv_timeout(LIMIT);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn restart_reports_terminated_and_restarted() {
        capture();
        let system = ActorSystem::new().unwrap();
        let group = system.async_group("reports-restart", 1).unwrap();
        let mut builder = system.supervisor(config("sup-restart", 5)).unwrap();
        let child = builder
            .add(ChildSpec::new("w", &group, |_| async {
                Ok::<_, String>(Worker)
            }))
            .unwrap();
        let _sup = builder.start().await.unwrap();
        child.wait_ready().await.unwrap().send(Boom).unwrap();
        child.wait_for_generation(1).await.unwrap();
        assert!(system.shutdown(LIMIT).await.complete);
        let reports = of("sup-restart");
        let terminated = event(&reports, "child_terminated");
        assert_eq!(terminated.len(), 1, "{reports:?}");
        let (level, f) = terminated[0];
        assert_eq!(*level, tracing::Level::WARN);
        assert!(FIELDS.iter().all(|k| f.contains_key(*k)), "{f:?}");
        assert_eq!(f["system"], system.id().to_string());
        assert_eq!((f["child"].as_str(), f["generation"].as_str()), ("w", "1"));
        assert!(f["reason"].contains("worker boom"));
        assert_eq!(f["action"], "restart");
        assert_eq!(
            (f["max_restarts"].as_str(), f["window_ms"].as_str()),
            ("5", "30000")
        );
        let restarted = event(&reports, "child_restarted");
        assert_eq!(restarted.len(), 1);
        assert_eq!(restarted[0].0, tracing::Level::INFO);
        assert_eq!(restarted[0].1["generation"], "2");
        // The supervisor's own shutdown stops the children: not reported.
        assert_eq!(event(&reports, "child_terminated").len(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn intensity_and_start_failures_are_errors() {
        capture();
        let system = ActorSystem::new().unwrap();
        let group = system.async_group("reports-intensity", 1).unwrap();
        let mut builder = system.supervisor(config("sup-intensity", 0)).unwrap();
        let child = builder
            .add(ChildSpec::new("w", &group, |_| async {
                Ok::<_, String>(Worker)
            }))
            .unwrap();
        let sup = builder.start().await.unwrap();
        child.wait_ready().await.unwrap().send(Boom).unwrap();
        sup.join().await;
        let reports = of("sup-intensity");
        let reached = event(&reports, "restart_intensity_reached");
        assert_eq!(reached.len(), 1, "{reports:?}");
        assert_eq!(reached[0].0, tracing::Level::ERROR);
        // The exit's report names what the budget decided: no restart.
        let terminated = event(&reports, "child_terminated");
        assert_eq!(terminated.len(), 1, "{reports:?}");
        assert_eq!(terminated[0].1["action"], "shutdown");

        let mut builder = system.supervisor(config("sup-start", 5)).unwrap();
        builder
            .add(ChildSpec::new("bad", &group, |_| async {
                Err::<Worker, _>("no database")
            }))
            .unwrap();
        assert!(builder.start().await.is_err());
        let reports = of("sup-start");
        let failed = event(&reports, "start_failed");
        assert_eq!(failed.len(), 1, "{reports:?}");
        assert_eq!(failed[0].0, tracing::Level::ERROR);
        assert_eq!(failed[0].1["child"], "bad");
        assert!(system.shutdown(LIMIT).await.complete);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_child_killed_at_its_shutdown_time_is_a_shutdown_error() {
        capture();
        let system = ActorSystem::new().unwrap();
        let group = system.async_group("reports-shutdown", 2).unwrap();
        let mut builder = system.supervisor(config("sup-shutdown", 5)).unwrap();
        let child = builder
            .add(
                ChildSpec::new("slow", &group, |_| async { Ok::<_, String>(Worker) })
                    // Wide enough that the release after the kill lands
                    // within the second interval (no `stuck`).
                    .shutdown_timeout(Duration::from_millis(250)),
            )
            .unwrap();
        let sup = builder.start().await.unwrap();
        let (entered_tx, entered) = std::sync::mpsc::channel();
        let (release_tx, release) = std::sync::mpsc::channel();
        child
            .wait_ready()
            .await
            .unwrap()
            .send(Wedge(entered_tx, release))
            .unwrap();
        entered.recv_timeout(LIMIT).unwrap();
        sup.stop();
        let until = Instant::now() + LIMIT;
        while event(&of("sup-shutdown"), "shutdown_error").is_empty() {
            assert!(Instant::now() < until, "no shutdown_error");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        release_tx.send(()).unwrap();
        sup.join().await;
        let reports = of("sup-shutdown");
        let error = event(&reports, "shutdown_error");
        assert_eq!(error.len(), 1);
        assert_eq!(error[0].0, tracing::Level::ERROR);
        assert_eq!(
            (error[0].1["child"].as_str(), error[0].1["action"].as_str()),
            ("slow", "killed")
        );
        assert!(system.shutdown(LIMIT).await.complete);
    }

    /// A child the kill does not end within a second interval: `killed`,
    /// then `stuck` (the slot fails the tree, V3).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_child_the_kill_does_not_end_is_reported_stuck() {
        capture();
        let system = ActorSystem::new().unwrap();
        let group = system.async_group("reports-stuck", 2).unwrap();
        let mut builder = system.supervisor(config("sup-stuck", 5)).unwrap();
        let child = builder
            .add(
                ChildSpec::new("wedged", &group, |_| async { Ok::<_, String>(Worker) })
                    .shutdown_timeout(Duration::from_millis(50)),
            )
            .unwrap();
        let sup = builder.start().await.unwrap();
        let (entered_tx, entered) = std::sync::mpsc::channel();
        let (release_tx, release) = std::sync::mpsc::channel();
        child
            .wait_ready()
            .await
            .unwrap()
            .send(Wedge(entered_tx, release))
            .unwrap();
        entered.recv_timeout(LIMIT).unwrap();
        sup.stop();
        let until = Instant::now() + LIMIT;
        while event(&of("sup-stuck"), "shutdown_error").len() < 2 {
            assert!(Instant::now() < until, "no stuck shutdown_error");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        release_tx.send(()).unwrap();
        sup.join().await;
        let reports = of("sup-stuck");
        let actions: Vec<_> = event(&reports, "shutdown_error")
            .iter()
            .map(|(level, f)| (*level, f["child"].clone(), f["action"].clone()))
            .collect();
        assert_eq!(
            actions,
            [
                (tracing::Level::ERROR, "wedged".into(), "killed".into()),
                (tracing::Level::ERROR, "wedged".into(), "stuck".into()),
            ]
        );
        let _ = system.shutdown(LIMIT).await;
    }

    /// OneForAll: a collateral child that misses its shutdown time when the
    /// cohort is stopped for a sibling's crash is killed and reported, as a
    /// supervisor stop's child is (V3), and the round goes on.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_collateral_child_missing_its_shutdown_time_is_a_shutdown_error() {
        capture();
        let system = ActorSystem::new().unwrap();
        let group = system.async_group("reports-collateral", 2).unwrap();
        let mut builder = system
            .supervisor(
                config("sup-collateral", 5)
                    .strategy(spawned_concurrency::supervision::SupervisorStrategy::OneForAll),
            )
            .unwrap();
        let crashing = builder
            .add(ChildSpec::new("crashing", &group, |_| async {
                Ok::<_, String>(Worker)
            }))
            .unwrap();
        let slow = builder
            .add(
                ChildSpec::new("slow", &group, |_| async { Ok::<_, String>(Worker) })
                    // Wide enough that the release after the kill lands
                    // within the second interval (no `stuck`).
                    .shutdown_timeout(Duration::from_millis(250)),
            )
            .unwrap();
        let _sup = builder.start().await.unwrap();
        let (entered_tx, entered) = std::sync::mpsc::channel();
        let (release_tx, release) = std::sync::mpsc::channel();
        slow.wait_ready()
            .await
            .unwrap()
            .send(Wedge(entered_tx, release))
            .unwrap();
        entered.recv_timeout(LIMIT).unwrap();
        crashing.wait_ready().await.unwrap().send(Boom).unwrap();
        let until = Instant::now() + LIMIT;
        while event(&of("sup-collateral"), "shutdown_error").is_empty() {
            assert!(
                Instant::now() < until,
                "no shutdown_error: {:?}",
                of("sup-collateral")
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        release_tx.send(()).unwrap();
        // The kill ends it once its turn returns; the round restarts it.
        slow.wait_for_generation(1).await.unwrap();
        let reports = of("sup-collateral");
        let error = event(&reports, "shutdown_error");
        assert_eq!(error.len(), 1, "{reports:?}");
        assert_eq!(error[0].0, tracing::Level::ERROR);
        assert_eq!(error[0].1["child"], "slow");
        assert_eq!(error[0].1["action"], "killed");
        assert!(system.shutdown(LIMIT).await.complete);
    }
}

mod threads_mode {
    use super::*;
    use spawned_concurrency::threads::{Actor, ActorStart, Context, Handler};

    struct Crashy;
    impl Actor for Crashy {
        fn started(&mut self, ctx: &Context<Self>) {
            ctx.set_label("native");
        }
    }
    impl Handler<Boom> for Crashy {
        fn handle(&mut self, _: Boom, _: PendingReply<'_, ()>, _: &Context<Self>) {
            panic!("native boom");
        }
    }
    impl Handler<Quit> for Crashy {
        fn handle(&mut self, msg: Quit, reply: PendingReply<'_, ()>, ctx: &Context<Self>) {
            ctx.stop_with_reason(msg.0);
            reply.reply(());
        }
    }

    #[test]
    fn handler_panic_is_reported_and_clean_exits_are_not() {
        capture();
        let actor = Crashy.spawn();
        actor.send(Boom).unwrap();
        assert!(matches!(actor.wait_exit(), ExitReason::Panic(_)));
        let report = one_report(actor.id());
        assert!(report["actor_type"].ends_with("Crashy"));
        assert_eq!(report["label"], "native");
        assert!(report["reason"].contains("native boom"));
        let clean = Crashy.spawn();
        clean.request(Quit(ExitReason::Normal)).unwrap();
        clean.wait_exit();
        let killed = Crashy.spawn();
        killed.child_handle().kill();
        assert_eq!(killed.wait_exit(), ExitReason::Killed);
        let until = Instant::now() + Duration::from_millis(50);
        while Instant::now() < until {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(reports(clean.id()).is_empty());
        assert!(reports(killed.id()).is_empty());
    }
}
