#![cfg(spawned_scheduler_loom)]
#[path = "../benches/support/ready_gate.rs"]
mod ready_gate;
use loom::sync::{Arc, Mutex};
#[test]
fn publish_racing_finish_retains_exactly_one_ready_token() {
    let mut model = loom::model::Builder::new();
    model.preemption_bound = Some(3);
    model.max_permutations = Some(10000);
    model.check(|| {
        let gate = Arc::new(ready_gate::ReadyGate::new());
        let messages = Arc::new(Mutex::new(vec![0]));
        let ready = Arc::new(Mutex::new(0));
        gate.notify(|| *ready.lock().unwrap() += 1);
        let g = gate.clone();
        let m = messages.clone();
        let q = ready.clone();
        let worker = loom::thread::spawn(move || {
            {
                let mut n = q.lock().unwrap();
                assert_eq!(*n, 1);
                *n -= 1;
            }
            let mut mailbox = m.lock().unwrap();
            assert_eq!(mailbox.remove(0), 0);
            g.finish(|| !mailbox.is_empty(), || *q.lock().unwrap() += 1);
        });
        let mut producers = Vec::new();
        for id in [1, 2] {
            let g = gate.clone();
            let m = messages.clone();
            let q = ready.clone();
            producers.push(loom::thread::spawn(move || {
                m.lock().unwrap().push(id);
                g.notify(|| *q.lock().unwrap() += 1);
            }));
        }
        worker.join().unwrap();
        for p in producers {
            p.join().unwrap();
        }
        assert_eq!(messages.lock().unwrap().len(), 2);
        assert_eq!(*ready.lock().unwrap(), 1);
    });
}
