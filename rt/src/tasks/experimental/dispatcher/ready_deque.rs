//! Safe wrapper around Crossbeam's owner queues and external injection queue.
use super::Work;
use crossbeam_deque::{Injector, Steal, Stealer, Worker};
use std::{
    cell::{Cell, RefCell},
    sync::atomic::{AtomicUsize, Ordering},
};

struct Owner {
    pool: usize,
    worker: usize,
    queue: Worker<Work>,
    injection_first: Cell<bool>,
}
thread_local! {
    static OWNER: RefCell<Option<Owner>> = const {RefCell::new(None)};
}
pub(super) struct OwnerGuard;
impl Drop for OwnerGuard {
    fn drop(&mut self) {
        // Release TLS ownership before dropping the Worker handle. Its shared
        // buffer/tickets may remain alive in Stealers until the pool drops.
        let queue = OWNER.with(|slot| slot.borrow_mut().take());
        drop(queue);
    }
}
pub(super) fn install(pool: usize, worker: usize, queue: Worker<Work>) -> OwnerGuard {
    OWNER.with(|slot| {
        assert!(slot.borrow().is_none());
        *slot.borrow_mut() = Some(Owner {
            pool,
            worker,
            queue,
            injection_first: Cell::new(false),
        });
    });
    OwnerGuard
}
pub(super) struct ReadyDeque {
    injection: Injector<Work>,
    stealers: Vec<Stealer<Work>>,
    next: AtomicUsize,
}
pub(super) struct Enqueued {
    pub owner: usize,
    pub external: bool,
    pub share: bool,
}
fn attempt(mut steal: impl FnMut() -> Steal<Work>) -> Option<Work> {
    // Contention retries are hints, not empty-queue proofs. The root driver's
    // register/recheck and timer fallback remain required after a retry budget.
    for _ in 0..4 {
        match steal() {
            Steal::Success(work) => return Some(work),
            Steal::Empty => return None,
            Steal::Retry => {}
        }
    }
    None
}
impl ReadyDeque {
    pub fn new(count: usize) -> (Self, Vec<Worker<Work>>) {
        let queues: Vec<_> = (0..count).map(|_| Worker::new_lifo()).collect();
        let stealers = queues.iter().map(Worker::stealer).collect();
        (
            Self {
                injection: Injector::new(),
                stealers,
                next: AtomicUsize::new(0),
            },
            queues,
        )
    }
    pub fn enqueue(&self, pool: usize, work: Work, current: Option<usize>) -> Enqueued {
        let preferred = work.preferred() % self.stealers.len();
        let mut ticket = Some(work);
        let local = current.and_then(|worker| {
            OWNER.with(|slot| {
                let owner = slot.borrow();
                let owner = owner.as_ref()?;
                if owner.pool != pool {
                    return None;
                }
                assert_eq!(
                    owner.worker, worker,
                    "native deque worker identity mismatch"
                );
                owner.queue.push(ticket.take().unwrap());
                Some((owner.worker, owner.queue.len() > 1))
            })
        });
        if let Some((owner, share)) = local {
            Enqueued {
                owner,
                external: false,
                share,
            }
        } else {
            self.injection.push(ticket.take().unwrap());
            // Prefer an approximate rotating target rather than concentrating
            // every outside wake on its last actor worker. Other workers can
            // batch-steal this shared injection queue.
            let owner = self
                .next
                .fetch_add(1, Ordering::Relaxed)
                .wrapping_add(preferred)
                % self.stealers.len();
            Enqueued {
                owner,
                external: true,
                share: self.injection.len() > 1,
            }
        }
    }
    pub fn take(
        &self,
        pool: usize,
        worker: usize,
        oldest: bool,
        victim: &mut usize,
    ) -> Option<Work> {
        OWNER.with(|slot| {
            let owner = slot.borrow();
            let owner = owner.as_ref().expect("native deque owner not installed");
            assert_eq!(owner.pool, pool);
            assert_eq!(
                owner.worker, worker,
                "native deque worker identity mismatch"
            );
            let queue = &owner.queue;
            // Alternate oldest-local and outside-injection priority on periodic
            // turns. Injection-first on every turn can starve the local bottom
            // forever while new batches keep filling the LIFO top.
            if oldest {
                let injection_first = owner.injection_first.get();
                owner.injection_first.set(!injection_first);
                if injection_first {
                    if let Some(work) = attempt(|| self.injection.steal_batch_and_pop(queue)) {
                        return Some(work);
                    }
                }
                if let Some(work) = attempt(|| self.stealers[worker].steal()) {
                    return Some(work);
                }
                if !injection_first {
                    if let Some(work) = attempt(|| self.injection.steal_batch_and_pop(queue)) {
                        return Some(work);
                    }
                }
            }
            // A bounded self-steal Retry result is not an empty local queue.
            // Owner pop supplies a progress fallback without another steal CAS.
            if let Some(work) = queue.pop() {
                return Some(work);
            }
            if !oldest {
                if let Some(work) = attempt(|| self.injection.steal_batch_and_pop(queue)) {
                    return Some(work);
                }
            }
            for _ in 0..self.stealers.len() {
                let peer = *victim % self.stealers.len();
                *victim = victim.wrapping_add(1);
                if peer == worker {
                    continue;
                }
                if let Some(work) = attempt(|| self.stealers[peer].steal_batch_and_pop(queue)) {
                    return Some(work);
                }
            }
            None
        })
    }
    pub fn has_local(&self, pool: usize) -> bool {
        OWNER.with(|slot| {
            slot.borrow()
                .as_ref()
                .is_some_and(|owner| owner.pool == pool && !owner.queue.is_empty())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Gate, Job};
    use super::*;
    use std::sync::{Arc, Mutex, Weak};

    fn ticket(id: usize) -> Work {
        Work::Custom(Arc::new(Job {
            gate: Gate::new(),
            future: Mutex::new(None),
            pool: Weak::new(),
            preferred: AtomicUsize::new(id),
        }))
    }

    #[test]
    fn oldest_local_progress_under_sustained_injection() {
        let (ready, mut queues) = ReadyDeque::new(1);
        let _owner = install(101, 0, queues.pop().unwrap());
        ready.enqueue(101, ticket(1), Some(0));
        let mut victim = 0;
        let mut found = false;
        for turn in 0..64 {
            // More outside work than each local turn can consume. This must
            // not permanently hide the oldest already-runnable local ticket.
            for _ in 0..64 {
                ready.enqueue(101, ticket(2), None);
            }
            // Keep a hot local continuation above the old marker as well.
            ready.enqueue(101, ticket(2), Some(0));
            let work = ready.take(101, 0, turn % 4 == 3, &mut victim).unwrap();
            if work.preferred() == 1 {
                found = true;
                break;
            }
        }
        assert!(found, "old local ticket starved behind sustained injection");
    }

    #[test]
    fn injected_work_progresses_with_hot_local_continuations() {
        let (ready, mut queues) = ReadyDeque::new(1);
        let _owner = install(102, 0, queues.pop().unwrap());
        ready.enqueue(102, ticket(1), None);
        let mut victim = 0;
        let mut found = false;
        for turn in 0..64 {
            for _ in 0..64 {
                ready.enqueue(102, ticket(2), Some(0));
            }
            let work = ready.take(102, 0, turn % 4 == 3, &mut victim).unwrap();
            if work.preferred() == 1 {
                found = true;
                break;
            }
        }
        assert!(
            found,
            "outside ticket starved behind hot local continuations"
        );
    }

    #[test]
    fn cross_pool_arrival_uses_destination_injector() {
        let (_first, mut first_queues) = ReadyDeque::new(1);
        let _owner = install(103, 0, first_queues.pop().unwrap());
        let (second, _) = ReadyDeque::new(4);
        let result = second.enqueue(104, ticket(7), Some(0));
        assert!(result.external);
        assert_eq!(second.injection.steal().success().unwrap().preferred(), 7);
    }

    #[test]
    fn four_worker_peer_batch_steal_conserves_tickets() {
        let (ready, mut queues) = ReadyDeque::new(4);
        let thief_queue = queues.pop().unwrap();
        let owner_queue = queues.remove(0);
        let ready = Arc::new(ready);
        let _owner = install(105, 0, owner_queue);
        for id in 0..64 {
            ready.enqueue(105, ticket(id), Some(0));
        }
        let peer = ready.clone();
        let mut ids = std::thread::spawn(move || {
            let _owner = install(105, 3, thief_queue);
            let mut victim = 0;
            let first = peer.take(105, 3, false, &mut victim).unwrap();
            assert!(peer.has_local(105), "peer steal must transfer a batch");
            let mut ids = vec![first.preferred()];
            while let Some(work) = peer.take(105, 3, false, &mut victim) {
                ids.push(work.preferred());
            }
            ids
        })
        .join()
        .unwrap();
        let mut victim = 1;
        while let Some(work) = ready.take(105, 0, false, &mut victim) {
            ids.push(work.preferred());
        }
        ids.sort_unstable();
        assert_eq!(ids, (0..64).collect::<Vec<_>>());
    }
}
