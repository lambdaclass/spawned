//! Bounded unique-payload pressure, including shutdown with an admitted backlog.
#[path = "support/failure_actor.rs"]
mod actor;
use actor::*;
use spawned_concurrency::ExitReason;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
#[test]
#[ignore = "bounded high-producer stress; run in the extended campaign"]
fn many_producers_preserve_fifo_or_discard_every_owned_payload_before_completion() {
    let producers: usize = std::env::var("SPAWNED_STRESS_PRODUCERS")
        .unwrap_or_else(|_| "128".into())
        .parse()
        .unwrap();
    let each: usize = std::env::var("SPAWNED_STRESS_MESSAGES")
        .unwrap_or_else(|_| "256".into())
        .parse()
        .unwrap();
    let bytes: usize = std::env::var("SPAWNED_STRESS_PAYLOAD")
        .unwrap_or_else(|_| "1024".into())
        .parse()
        .unwrap();
    assert!(
        (1..=256).contains(&producers)
            && (1..=4096).contains(&each)
            && (1..=16_384).contains(&bytes)
    );
    let count = producers.checked_mul(each).unwrap();
    assert!(count <= 262_144 && count.checked_mul(bytes).unwrap() <= 128 * 1024 * 1024);
    let rt = runtime();
    let _entered = rt.enter();
    for backend in BACKENDS {
        for discard in [false, true] {
            let mut cleanup = Cleanup::default();
            let (started, mut control) = gate();
            let actor = Handle::start(
                backend,
                Worker {
                    started: Some(started),
                    ..Worker::default()
                },
            );
            cleanup.track(&actor);
            control.entered();
            let dropped = Arc::new(AtomicUsize::new(0));
            std::thread::scope(|scope| {
                for producer in 0..producers {
                    let (actor, dropped) = (actor.clone(), dropped.clone());
                    scope.spawn(move || {
                        for sequence in 0..each {
                            let id = (producer * each + sequence) as u64;
                            actor
                                .send(Command::Payload {
                                    id,
                                    payload: Payload {
                                        bytes: vec![(id % 251) as u8; bytes],
                                        dropped: dropped.clone(),
                                    },
                                })
                                .unwrap();
                        }
                    });
                }
            });
            assert_eq!(
                dropped.load(Ordering::SeqCst),
                0,
                "payloads must still be queued behind startup"
            );
            if discard {
                actor.stop();
            }
            control.release();
            if !discard {
                let Reply::Values(values) = actor.request(&rt, Command::Read).unwrap() else {
                    panic!("wrong reply")
                };
                assert_eq!(values.len(), count);
                let mut next = vec![0; producers];
                for id in values {
                    let producer = id as usize / each;
                    let sequence = id as usize % each;
                    assert_eq!(
                        sequence, next[producer],
                        "duplicate, missing or reordered message"
                    );
                    next[producer] += 1;
                }
                assert!(next.iter().all(|&n| n == each));
                actor.stop();
            }
            assert_eq!(actor.wait(&rt), ExitReason::Normal);
            assert_eq!(
                dropped.load(Ordering::SeqCst),
                count,
                "completion retained an admitted payload"
            );
            println!("backend={backend:?} discard={discard} producers={producers} payloads={count} owned_bytes={}", count * bytes);
        }
    }
}
