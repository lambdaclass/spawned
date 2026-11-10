//! Handshake-controlled failures with semantic coverage, not random hit counts.
// Tests that need OS threads are compiled out under spawned_sim.
#![cfg_attr(spawned_sim, allow(unused_imports, dead_code))]
#[path = "support/failure_actor.rs"]
mod actor;
use actor::*;
use spawned_concurrency::{ActorError, ExitReason};
use std::{
    collections::BTreeSet,
    sync::{mpsc, Arc, Mutex},
    time::Duration,
};

const REQUIRED: [&str; 9] = [
    "stop_started_with_queued_request",
    "timeout_after_effect_before_reply",
    "caller_drop_after_effect",
    "panic_after_effect_before_reply",
    "cleanup_panic_after_normal_stop",
    "linked_failure_after_normal_stop",
    "linked_failure_before_normal_stop",
    "demonitor_before_target_exit",
    "demonitor_after_down_dispatch",
];

// Not simulated: runs on every backend, including OS-thread ones.
#[cfg(not(spawned_sim))]
#[test]
fn controlled_failures_reach_every_required_window_on_every_backend() {
    let rt = runtime();
    let _entered = rt.enter();
    let mut summaries = Vec::new();
    for &backend in BACKENDS {
        let mut cleanup = Cleanup::default();
        let mut reached = BTreeSet::new();
        {
            let (started, mut control) = gate();
            let effects = Arc::new(Mutex::new(Vec::new()));
            let actor = Handle::start(
                backend,
                Worker {
                    started: Some(started),
                    effects: Some(effects.clone()),
                    ..Worker::default()
                },
            );
            cleanup.track(&actor);
            control.entered();
            let queued = actor.raw(Command::append(1)).unwrap();
            actor.stop();
            control.release();
            assert_eq!(actor.wait(&rt), ExitReason::Normal);
            assert!(matches!(
                queued.wait(&rt, LIMIT),
                Err(ActorError::ActorStopped)
            ));
            assert!(effects.lock().unwrap().is_empty());
            reached.insert(REQUIRED[0]);
        }
        for abandon in [false, true] {
            let actor = Handle::start(backend, Worker::default());
            cleanup.track(&actor);
            let (after, mut control) = gate();
            let response = actor
                .raw(Command::Append {
                    id: 7,
                    before: None,
                    after: Some(after),
                    panic_after: false,
                })
                .unwrap();
            control.entered(); // Effect already happened; reply cannot have been sent.
            if abandon {
                drop(response);
            } else {
                assert!(matches!(
                    response.wait(&rt, Duration::ZERO),
                    Err(ActorError::RequestTimeout)
                ));
            }
            control.release();
            assert_eq!(
                actor.request(&rt, Command::Read).unwrap(),
                Reply::Values(vec![7])
            );
            actor.stop();
            assert_eq!(actor.wait(&rt), ExitReason::Normal);
            reached.insert(REQUIRED[if abandon { 2 } else { 1 }]);
        }
        {
            let effects = Arc::new(Mutex::new(Vec::new()));
            let actor = Handle::start(
                backend,
                Worker {
                    effects: Some(effects.clone()),
                    ..Worker::default()
                },
            );
            cleanup.track(&actor);
            let (after, mut control) = gate();
            let response = actor
                .raw(Command::Append {
                    id: 9,
                    before: None,
                    after: Some(after),
                    panic_after: true,
                })
                .unwrap();
            control.entered();
            assert_eq!(*effects.lock().unwrap(), vec![9]);
            control.release();
            assert!(matches!(
                response.wait(&rt, LIMIT),
                Err(ActorError::ActorStopped)
            ));
            assert!(matches!(actor.wait(&rt), ExitReason::Panic(_)));
            assert_eq!(*effects.lock().unwrap(), vec![9]);
            reached.insert(REQUIRED[3]);
        }
        {
            let (started, mut control) = gate();
            let actor = Handle::start(
                backend,
                Worker {
                    started: Some(started),
                    panic_stopped: true,
                    ..Worker::default()
                },
            );
            cleanup.track(&actor);
            control.entered();
            actor.stop();
            control.release();
            assert!(matches!(actor.wait(&rt), ExitReason::Panic(_)));
            reached.insert(REQUIRED[4]);
        }
        {
            let (stopped, mut control) = gate();
            let peer = Handle::start(
                backend,
                Worker {
                    stopped: Some(stopped),
                    ..Worker::default()
                },
            );
            let target = Handle::start(backend, Worker::default());
            cleanup.track(&peer);
            cleanup.track(&target);
            peer.link(&target.child());
            peer.stop();
            control.entered(); // Normal stop is committed while cleanup is pending.
            assert!(target.request(&rt, Command::Panic).is_err());
            assert!(matches!(target.wait(&rt), ExitReason::Panic(_)));
            control.release();
            assert_eq!(peer.wait(&rt), ExitReason::Normal);
            reached.insert(REQUIRED[5]);
        }
        {
            let peer = Handle::start(backend, Worker::default());
            let target = Handle::start(backend, Worker::default());
            cleanup.track(&peer);
            cleanup.track(&target);
            peer.link(&target.child());
            let (after, mut control) = gate();
            let response = peer
                .raw(Command::Append {
                    id: 11,
                    before: None,
                    after: Some(after),
                    panic_after: false,
                })
                .unwrap();
            control.entered();
            assert!(target.request(&rt, Command::Panic).is_err());
            assert!(matches!(target.wait(&rt), ExitReason::Panic(_)));
            peer.stop();
            control.release();
            assert_eq!(
                response.wait(&rt, LIMIT).unwrap(),
                Reply::Appended { id: 11, length: 1 }
            );
            assert!(matches!(peer.wait(&rt), ExitReason::Panic(_)));
            reached.insert(REQUIRED[6]);
        }
        for delivered in [false, true] {
            let (events, observed) = mpsc::channel();
            let observer = Handle::start(
                backend,
                Worker {
                    down: Some(events),
                    ..Worker::default()
                },
            );
            let target = Handle::start(backend, Worker::default());
            cleanup.track(&observer);
            cleanup.track(&target);
            let id = observer.monitor(&target.child());
            if !delivered {
                observer.demonitor(id);
            }
            target.stop();
            assert_eq!(target.wait(&rt), ExitReason::Normal);
            if delivered {
                let down = observed.recv_timeout(LIMIT).unwrap();
                assert_eq!(down.monitor_ref, id);
                assert_eq!(down.reason, ExitReason::Normal);
                observer.demonitor(id); // Cannot retract an already-dispatched Down.
            } else {
                // A bounded negative observation is not an exhaustive scheduling proof.
                assert!(matches!(
                    observed.recv_timeout(Duration::from_millis(20)),
                    Err(mpsc::RecvTimeoutError::Timeout)
                ));
            }
            observer.stop();
            observer.wait(&rt);
            reached.insert(REQUIRED[if delivered { 8 } else { 7 }]);
        }
        assert_eq!(reached, REQUIRED.into_iter().collect());
        summaries.push(format!("{backend:?}:{}", reached.len()));
    }
    let report = format!(
        "semantic failure coverage: {}; windows={REQUIRED:?}\n",
        summaries.join(", ")
    );
    println!("{report}");
    if let Ok(path) = std::env::var("SPAWNED_FAILURE_REPORT") {
        std::fs::write(path, report).unwrap();
    }
}
