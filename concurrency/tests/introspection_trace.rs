//! Records one actor's activities and status turn from real actors, for TLA+
//! trace validation against `verification/tla/Introspection.tla`
//! (`TraceIntrospection.tla`, schema `introspection-v1`). Each scenario
//! writes `introspection_<name>.json` to `$LIFECYCLE_TRACE_DIR` (default
//! `target/lifecycle-traces`); `trace_check.py` checks them and
//! `negative_checks.py` requires edited copies to be rejected.
//!
//! The model has one actor and at most one status request, and no exit
//! signals but a kill; every scenario stays inside that.
#![cfg(all(feature = "lifecycle-trace", feature = "async", not(spawned_sim)))]

use spawned_concurrency::{
    lifecycle_trace,
    message::Message,
    tasks::{Actor, ActorRef, ActorStart, Context, Handler},
    ActorError, ExitReason, PendingReply,
};
use std::{path::PathBuf, sync::mpsc as std_mpsc, time::Duration};

const LIMIT: Duration = Duration::from_secs(5);

/// One recording at a time (held across awaits).
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
static SERIAL_SYNC: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn trace_dir() -> PathBuf {
    std::env::var_os("LIFECYCLE_TRACE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/lifecycle-traces")
        })
}

fn raw(id: spawned_concurrency::ActorId) -> u64 {
    id.to_string().parse().unwrap()
}

/// Stop recording, write the trace, return its events as `Action:x`.
fn write(name: &str, id: spawned_concurrency::ActorId) -> Vec<String> {
    let events = lifecycle_trace::take_introspection(raw(id));
    let json = format!(
        "{{\"name\":\"introspection_{name}\",\"schema\":\"introspection-v1\",\"events\":[\n{}\n]}}\n",
        events.iter().map(|e| e.to_json()).collect::<Vec<_>>().join(",\n")
    );
    let dir = trace_dir();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("introspection_{name}.json")), json).unwrap();
    events
        .iter()
        .map(|e| {
            if e.x.is_empty() {
                e.action.to_string()
            } else {
                format!("{}:{}", e.action, e.x)
            }
        })
        .collect()
}

fn has(events: &[String], event: &str) -> bool {
    events.iter().any(|e| e == event)
}

struct Wedge(std_mpsc::Sender<()>, std_mpsc::Receiver<()>);
impl Message for Wedge {
    type Result = ();
}
struct Ping;
impl Message for Ping {
    type Result = ();
}
struct Boom;
impl Message for Boom {
    type Result = ();
}

fn wedge() -> (Wedge, std_mpsc::Receiver<()>, std_mpsc::Sender<()>) {
    let (entered_tx, entered) = std_mpsc::channel();
    let (release_tx, release) = std_mpsc::channel();
    (Wedge(entered_tx, release), entered, release_tx)
}

mod tasks_mode {
    use super::*;

    #[derive(Default)]
    struct Worker {
        panic_in_started: bool,
        panic_in_stopped: bool,
        stop_gate: Option<tokio::sync::oneshot::Receiver<()>>,
        stop_entered: Option<std_mpsc::Sender<()>>,
    }
    impl Actor for Worker {
        async fn started(&mut self, _: &Context<Self>) {
            if self.panic_in_started {
                panic!("started boom");
            }
        }
        async fn stopped(&mut self, _: &Context<Self>) {
            if let Some(entered) = self.stop_entered.take() {
                let _ = entered.send(());
            }
            if let Some(gate) = self.stop_gate.take() {
                let _ = gate.await;
            }
            if self.panic_in_stopped {
                panic!("stopped boom");
            }
        }
    }
    impl Handler<Wedge> for Worker {
        async fn handle(&mut self, msg: Wedge, reply: PendingReply<'_, ()>, _: &Context<Self>) {
            msg.0.send(()).unwrap();
            let _ = msg.1.recv_timeout(LIMIT);
            reply.reply(());
        }
    }
    impl Handler<Ping> for Worker {
        async fn handle(&mut self, _: Ping, reply: PendingReply<'_, ()>, _: &Context<Self>) {
            reply.reply(());
        }
    }
    impl Handler<Boom> for Worker {
        async fn handle(&mut self, _: Boom, _: PendingReply<'_, ()>, _: &Context<Self>) {
            panic!("handler boom");
        }
    }

    async fn start(worker: Worker) -> ActorRef<Worker> {
        lifecycle_trace::record_introspection();
        worker.spawn()
    }

    /// A status request between two messages, then a stop.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn status_between_turns() {
        let _serial = SERIAL.lock().await;
        let actor = start(Worker::default()).await;
        actor.request(Ping).await.unwrap();
        assert!(actor.get_status().await.is_ok());
        actor.request(Ping).await.unwrap();
        actor.child_handle().stop();
        actor.join().await;
        let events = write("status_between_turns", actor.id());
        assert!(
            has(&events, "Admit:run") && has(&events, "Answer:ok") && has(&events, "End:status")
        );
    }

    /// The caller gives up while the actor is wedged: the status turn
    /// skips its callback.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn status_skipped_after_timeout() {
        let _serial = SERIAL.lock().await;
        let actor = start(Worker::default()).await;
        let (msg, entered, release) = wedge();
        actor.send(msg).unwrap();
        entered.recv_timeout(LIMIT).unwrap();
        let late = actor
            .with_request_timeout(Duration::from_millis(30))
            .get_state(|_| ())
            .await;
        assert!(matches!(late, Err(ActorError::RequestTimeout)));
        release.send(()).unwrap();
        actor.request(Ping).await.unwrap();
        actor.child_handle().stop();
        actor.join().await;
        let events = write("status_skipped_after_timeout", actor.id());
        assert!(has(&events, "Admit:skip") && !has(&events, "Answer:ok"));
    }

    /// Queued behind a wedged turn when a kill arrives: answered at D.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn status_queued_at_death_after_kill() {
        let _serial = SERIAL.lock().await;
        let actor = start(Worker::default()).await;
        let (msg, entered, release) = wedge();
        actor.send(msg).unwrap();
        entered.recv_timeout(LIMIT).unwrap();
        let queued = actor.get_status();
        actor.child_handle().kill();
        release.send(()).unwrap();
        assert!(matches!(
            queued.await,
            Err(ActorError::ActorStopped(ExitReason::Killed))
        ));
        actor.join().await;
        let events = write("status_queued_at_death_after_kill", actor.id());
        assert!(has(&events, "Exit:skip") && has(&events, "Die:Killed"));
    }

    /// Sent after D: refused.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn status_after_death() {
        let _serial = SERIAL.lock().await;
        let actor = start(Worker::default()).await;
        actor.child_handle().stop();
        actor.join().await;
        assert!(matches!(actor.get_status().await, Err(ActorError::NoProc)));
        let _ = actor.send(Ping);
        let events = write("status_after_death", actor.id());
        assert!(has(&events, "Status:noproc") && has(&events, "Send:noproc"));
    }

    /// A callback panic is answered `CallbackFailed`; the actor continues.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn callback_panic_contained() {
        let _serial = SERIAL.lock().await;
        let actor = start(Worker::default()).await;
        let failed = actor.get_state(|_| panic!("callback boom")).await;
        assert!(matches!(failed, Err(ActorError::CallbackFailed)));
        actor.request(Ping).await.unwrap();
        actor.child_handle().stop();
        assert_eq!(actor.wait_exit().await, ExitReason::Normal);
        actor.join().await;
        let events = write("callback_panic_contained", actor.id());
        assert!(has(&events, "Answer:failed") && has(&events, "Die:Shutdown"));
    }

    /// A handler panic with a status request queued behind it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn handler_panic_with_status_queued() {
        let _serial = SERIAL.lock().await;
        let actor = start(Worker::default()).await;
        let (msg, entered, release) = wedge();
        actor.send(msg).unwrap();
        entered.recv_timeout(LIMIT).unwrap();
        actor.send(Boom).unwrap();
        let queued = actor.get_status();
        release.send(()).unwrap();
        assert!(matches!(
            queued.await,
            Err(ActorError::ActorStopped(ExitReason::Panic(_)))
        ));
        actor.join().await;
        let events = write("handler_panic_with_status_queued", actor.id());
        assert!(has(&events, "Panic:turn") && has(&events, "Die:Panic"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn panics_in_started_and_stopped() {
        let _serial = SERIAL.lock().await;
        let actor = start(Worker {
            panic_in_started: true,
            ..Worker::default()
        })
        .await;
        actor.join().await;
        let events = write("panic_in_started", actor.id());
        assert!(has(&events, "StartPanic") && has(&events, "Die:Panic"));

        let actor = start(Worker {
            panic_in_stopped: true,
            ..Worker::default()
        })
        .await;
        actor.request(Ping).await.unwrap();
        actor.child_handle().stop();
        actor.join().await;
        let events = write("panic_in_stopped", actor.id());
        assert!(has(&events, "HookPanic") && has(&events, "Die:Panic"));
    }

    /// A kill while stopped() runs: it finishes, the reason is Killed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn kill_during_stopped() {
        let _serial = SERIAL.lock().await;
        let (open, gate) = tokio::sync::oneshot::channel();
        let (entered_tx, entered) = std_mpsc::channel();
        let actor = start(Worker {
            stop_gate: Some(gate),
            stop_entered: Some(entered_tx),
            ..Worker::default()
        })
        .await;
        actor.request(Ping).await.unwrap();
        actor.child_handle().stop();
        entered.recv_timeout(LIMIT).unwrap();
        actor.child_handle().kill();
        open.send(()).unwrap();
        assert_eq!(actor.wait_exit().await, ExitReason::Killed);
        actor.join().await;
        let events = write("kill_during_stopped", actor.id());
        assert!(has(&events, "HookBegin") && has(&events, "Kill") && has(&events, "Die:Killed"));
    }

    /// The runtime drops the actor with a status request queued: D answers
    /// it (Killed) before anything is destroyed.
    #[test]
    fn runtime_drop_with_status_queued() {
        let _serial = SERIAL_SYNC.lock().unwrap();
        let _async = SERIAL.blocking_lock();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (actor, queued) = runtime.block_on(async {
            lifecycle_trace::record_introspection();
            let actor = Worker::default().spawn();
            actor.request(Ping).await.unwrap();
            // Queued; the runtime never polls the actor again.
            let queued = actor.with_request_timeout(LIMIT).get_state(|_| ());
            (actor, queued)
        });
        drop(runtime);
        assert!(matches!(
            queued.into_result(),
            Err(ActorError::ActorStopped(ExitReason::Killed))
        ));
        let events = write("runtime_drop_with_status_queued", actor.id());
        assert!(has(&events, "Drop") && has(&events, "Die:Killed"));
    }
}

mod threads_mode {
    use super::*;
    use spawned_concurrency::threads::{
        Actor as NActor, ActorStart as NStart, Context as NContext, Handler as NHandler,
    };

    struct Worker;
    impl NActor for Worker {}
    impl NHandler<Wedge> for Worker {
        fn handle(&mut self, msg: Wedge, reply: PendingReply<'_, ()>, _: &NContext<Self>) {
            msg.0.send(()).unwrap();
            let _ = msg.1.recv_timeout(LIMIT);
            reply.reply(());
        }
    }
    impl NHandler<Ping> for Worker {
        fn handle(&mut self, _: Ping, reply: PendingReply<'_, ()>, _: &NContext<Self>) {
            reply.reply(());
        }
    }

    #[test]
    fn status_between_turns_and_callback_panic() {
        let _serial = SERIAL_SYNC.lock().unwrap();
        let _async = SERIAL.blocking_lock();
        lifecycle_trace::record_introspection();
        let actor = Worker.spawn();
        actor.request(Ping).unwrap();
        assert!(matches!(
            actor
                .get_state(|_: &Worker| panic!("native callback boom"))
                .into_result(),
            Err(ActorError::CallbackFailed)
        ));
        actor.request(Ping).unwrap();
        actor.child_handle().stop();
        actor.join();
        let events = write("native_callback_panic", actor.id());
        assert!(has(&events, "Answer:failed") && has(&events, "Die:Shutdown"));
    }

    #[test]
    fn status_queued_at_death_after_kill() {
        let _serial = SERIAL_SYNC.lock().unwrap();
        let _async = SERIAL.blocking_lock();
        lifecycle_trace::record_introspection();
        let actor = Worker.spawn();
        let (msg, entered, release) = wedge();
        actor.send(msg).unwrap();
        entered.recv_timeout(LIMIT).unwrap();
        let a = actor.clone();
        let queued = std::thread::spawn(move || a.get_status().into_result());
        std::thread::sleep(Duration::from_millis(30));
        actor.child_handle().kill();
        release.send(()).unwrap();
        assert!(matches!(
            queued.join().unwrap(),
            Err(ActorError::ActorStopped(ExitReason::Killed))
        ));
        actor.join();
        let events = write("native_status_queued_at_death", actor.id());
        assert!(has(&events, "Die:Killed"));
    }
}
