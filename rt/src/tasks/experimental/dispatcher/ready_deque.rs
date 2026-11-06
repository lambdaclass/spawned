//! Safe wrapper around Crossbeam's owner queues and external injection queue.
use super::Work;
use crossbeam_deque::{Injector, Steal, Stealer, Worker};
use std::{
    cell::RefCell,
    sync::atomic::{AtomicUsize, Ordering},
};

thread_local! {
    static OWNER: RefCell<Option<(usize, Worker<Work>)>> = const {RefCell::new(None)};
}
pub(super) struct OwnerGuard;
impl Drop for OwnerGuard {
    fn drop(&mut self) {
        // Release the RefCell borrow before dropping tickets/user state.
        let queue = OWNER.with(|slot| slot.borrow_mut().take());
        drop(queue);
    }
}
pub(super) fn install(pool: usize, queue: Worker<Work>) -> OwnerGuard {
    OWNER.with(|slot| {
        assert!(slot.borrow().is_none());
        *slot.borrow_mut() = Some((pool, queue));
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
                let (identity, queue) = owner.as_ref()?;
                if *identity != pool {
                    return None;
                }
                queue.push(ticket.take().unwrap());
                Some((worker, queue.len() > 1))
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
            let (identity, queue) = owner.as_ref().expect("native deque owner not installed");
            assert_eq!(*identity, pool);
            // Periodically inspect outside arrivals even with a hot local queue.
            if oldest {
                if let Some(work) = attempt(|| self.injection.steal_batch_and_pop(queue)) {
                    return Some(work);
                }
                if let Some(work) = attempt(|| self.stealers[worker].steal()) {
                    return Some(work);
                }
            } else if let Some(work) = queue.pop() {
                return Some(work);
            }
            if let Some(work) = attempt(|| self.injection.steal_batch_and_pop(queue)) {
                return Some(work);
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
                .is_some_and(|(identity, queue)| *identity == pool && !queue.is_empty())
        })
    }
}
