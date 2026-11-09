#![cfg(spawned_scheduler_loom)]
use loom::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Condvar, Mutex,
};
#[path = "../src/mailbox_admission.rs"]
mod admission;

#[test]
fn close_observes_every_accepted_publication_and_rejects_late_sends() {
    let mut model = loom::model::Builder::new();
    model.preemption_bound = Some(3);
    model.max_permutations = Some(10_000);
    model.check(|| {
        let gate = Arc::new(admission::SendGate::default());
        let published = Arc::new(AtomicUsize::new(0));
        let mut producers = Vec::new();
        for _ in 0..2 {
            let gate = gate.clone();
            let published = published.clone();
            producers.push(loom::thread::spawn(move || {
                if let Some(permit) = gate.enter() {
                    loom::thread::yield_now();
                    published.fetch_add(1, Ordering::Relaxed);
                    drop(permit);
                }
            }));
        }
        gate.close();
        let observed = published.load(Ordering::Relaxed);
        assert!(gate.is_closed());
        assert!(gate.enter().is_none());
        for producer in producers {
            producer.join().unwrap();
        }
        assert_eq!(
            observed,
            published.load(Ordering::Relaxed),
            "publication escaped receiver cleanup"
        );
    });
}
