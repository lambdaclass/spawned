//! Short concurrent histories; timed-out work is indeterminate, never assumed absent.
#[path = "support/failure_actor.rs"]
mod actor;
#[path = "support/history.rs"]
mod history;
use actor::*;
use history::*;
use spawned_concurrency::{ActorError, ExitReason};
use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
fn tick(clock: &AtomicU64) -> u64 {
    clock.fetch_add(1, Ordering::SeqCst)
}
fn invoke(
    actor: &Handle,
    rt: &tokio::runtime::Runtime,
    clock: &AtomicU64,
    action: Action,
) -> Operation {
    let invoked = tick(clock);
    let command = match action {
        Action::Append(id) => Command::append(id),
        Action::Read => Command::Read,
    };
    let outcome = match actor.raw(command) {
        Err(_) => Outcome::Rejected {
            finished: tick(clock),
        },
        Ok(receiver) => match receiver.wait(rt, LIMIT) {
            Ok(reply) => Outcome::Known {
                finished: tick(clock),
                reply,
            },
            Err(_) => Outcome::Unknown,
        },
    };
    Operation {
        invoked,
        action,
        outcome,
    }
}

#[test]
fn checker_rejects_wrong_replies_duplicate_dispatch_and_lost_acknowledged_effects() {
    let valid = vec![
        Operation {
            invoked: 0,
            action: Action::Append(7),
            outcome: Outcome::Known {
                finished: 1,
                reply: Reply::Appended { id: 7, length: 1 },
            },
        },
        Operation {
            invoked: 2,
            action: Action::Read,
            outcome: Outcome::Known {
                finished: 3,
                reply: Reply::Values(vec![7]),
            },
        },
    ];
    assert_eq!(check(&valid), Ok(true));
    for values in [vec![], vec![7, 7], vec![8]] {
        let mut broken = valid.clone();
        broken[1].outcome = Outcome::Known {
            finished: 3,
            reply: Reply::Values(values),
        };
        assert_eq!(check(&broken), Ok(false));
    }
    let mut wrong = valid.clone();
    wrong[0].outcome = Outcome::Known {
        finished: 1,
        reply: Reply::Appended { id: 8, length: 1 },
    };
    assert_eq!(check(&wrong), Ok(false));
    let mut unknown = valid.clone();
    unknown[0].outcome = Outcome::Unknown;
    assert_eq!(check(&unknown), Ok(true));
    unknown[0].outcome = Outcome::Rejected { finished: 1 };
    assert_eq!(check(&unknown), Ok(false));
}

#[test]
fn concurrent_histories_have_a_legal_sequential_explanation() {
    let cases: usize = std::env::var("SPAWNED_HISTORY_CASES")
        .unwrap_or_else(|_| "8".into())
        .parse()
        .unwrap();
    assert!((1..=1024).contains(&cases));
    let seed: u64 = std::env::var("SPAWNED_HISTORY_SEED")
        .unwrap_or_else(|_| "20261106".into())
        .parse()
        .unwrap();
    let rt = Arc::new(runtime());
    let _entered = rt.enter();
    for &backend in BACKENDS {
        for case in 0..cases {
            let mut cleanup = Cleanup::default();
            let actor = Handle::start(backend, Worker::default());
            cleanup.track(&actor);
            let clock = Arc::new(AtomicU64::new(0));
            let operations = Arc::new(Mutex::new(Vec::new()));
            // Demonstrate an effect survives a timed-out reply wait.
            let (after, mut control) = gate();
            let invoked = tick(&clock);
            let reply = actor
                .raw(Command::Append {
                    id: 0,
                    before: None,
                    after: Some(after),
                    panic_after: false,
                })
                .unwrap();
            control.entered();
            assert!(matches!(
                reply.wait(&rt, Duration::ZERO),
                Err(ActorError::RequestTimeout)
            ));
            operations.lock().unwrap().push(Operation {
                invoked,
                action: Action::Append(0),
                outcome: Outcome::Unknown,
            });
            control.release();
            operations
                .lock()
                .unwrap()
                .push(invoke(&actor, &rt, &clock, Action::Read));
            std::thread::scope(|scope| {
                for client in 0..3 {
                    let (actor, rt, clock, operations) =
                        (actor.clone(), rt.clone(), clock.clone(), operations.clone());
                    scope.spawn(move || {
                        let id = client + 1;
                        if (seed ^ case as u64 ^ id).is_multiple_of(2) {
                            std::thread::yield_now();
                        }
                        let append = invoke(&actor, &rt, &clock, Action::Append(id));
                        operations.lock().unwrap().push(append);
                        let read = invoke(&actor, &rt, &clock, Action::Read);
                        operations.lock().unwrap().push(read);
                    });
                }
            });
            operations
                .lock()
                .unwrap()
                .push(invoke(&actor, &rt, &clock, Action::Read));
            actor.stop();
            assert_eq!(actor.wait(&rt), ExitReason::Normal);
            operations
                .lock()
                .unwrap()
                .push(invoke(&actor, &rt, &clock, Action::Append(99)));
            let mut history = operations.lock().unwrap().clone();
            history.sort_by_key(|op| op.invoked);
            assert_eq!(
                check(&history),
                Ok(true),
                "seed={seed} case={case} backend={backend:?} history={history:?}"
            );
            // The oracle must also reject corruption of an actual observed history.
            let read = history
                .iter_mut()
                .rev()
                .find(|op| matches!(op.action, Action::Read))
                .unwrap();
            if let Outcome::Known {
                reply: Reply::Values(values),
                ..
            } = &mut read.outcome
            {
                values.push(999);
            }
            assert_eq!(check(&history), Ok(false));
        }
    }
    println!("checked {} histories across four backends; input seed={seed}; OS schedules are not replayed", cases * 4);
}
