//! Per-actor deadline heaps and the deadline service
//! (docs/design/deferred-replies.md §2, "Deadline storage").
//!
//! Each actor's registry owns a min-heap of `(deadline, entry)`, where an entry
//! is a deferred token's slot (and, with jobs, a job state). Entries hold
//! `Weak` references, so the heap owns nothing. An entry is stale when its
//! item is gone or terminal, or when its deadline is later than the item's
//! current one (superseded by `with_deadline`); the heap is rebuilt when stale
//! entries exceed live ones.
//!
//! The service holds one entry per actor, its earliest deadline, and expires
//! due entries even when no caller polls, so credits return on time:
//!
//! - real time: one process-wide service thread with its own timer, so
//!   per-test Tokio runtimes cannot break it;
//! - under `spawned_sim`: a task of the simulated runtime per actor heap, so
//!   clock domains never mix and virtual time drives it.

use super::registry::Registry;
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::sync::{Arc, Weak};

/// Something with a deadline the owner's heap services.
pub(crate) trait Expirable: Send + Sync {
    /// The current deadline while the item is pending, else `None`.
    fn pending_deadline(&self) -> Option<u64>;
    /// The deadline passed (`now >= deadline`): expire the item.
    fn expire(&self, now: u64);
}

struct Entry {
    at: u64,
    seq: u64,
    item: Weak<dyn Expirable>,
}
impl PartialEq for Entry {
    fn eq(&self, other: &Self) -> bool {
        (self.at, self.seq) == (other.at, other.seq)
    }
}
impl Eq for Entry {}
impl PartialOrd for Entry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Entry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.at, self.seq).cmp(&(other.at, other.seq))
    }
}

impl Entry {
    /// Live: the item exists, is pending, and this is its current deadline.
    /// The upgraded item is handed to `upgraded`, to be dropped by the caller
    /// outside every lock (it may be the last strong reference).
    fn is_live(&self, upgraded: &mut Vec<Arc<dyn Expirable>>) -> bool {
        let Some(item) = self.item.upgrade() else {
            return false;
        };
        let live = item.pending_deadline() == Some(self.at);
        upgraded.push(item);
        live
    }
}

/// One actor's deadline heap.
#[derive(Default)]
pub(crate) struct DeadlineHeap {
    heap: BinaryHeap<Reverse<Entry>>,
    seq: u64,
    /// Live entries at the last rebuild; the heap is rebuilt when it holds
    /// more than twice that many (plus slack).
    live_at_rebuild: usize,
}

impl DeadlineHeap {
    /// Add an entry. Returns whether it is the new earliest deadline, and
    /// the items compaction upgraded (drop them outside the lock).
    #[must_use]
    pub(crate) fn push(
        &mut self,
        at: u64,
        item: Weak<dyn Expirable>,
    ) -> (bool, Vec<Arc<dyn Expirable>>) {
        let mut upgraded = Vec::new();
        if self.heap.len() >= 2 * self.live_at_rebuild + 16 {
            self.compact(&mut upgraded);
        }
        self.seq += 1;
        let earliest = self.earliest().is_none_or(|e| at < e);
        self.heap.push(Reverse(Entry {
            at,
            seq: self.seq,
            item,
        }));
        (earliest, upgraded)
    }

    pub(crate) fn earliest(&self) -> Option<u64> {
        self.heap.peek().map(|Reverse(e)| e.at)
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.heap.len()
    }

    /// Rebuild without stale entries.
    fn compact(&mut self, upgraded: &mut Vec<Arc<dyn Expirable>>) {
        let entries = std::mem::take(&mut self.heap).into_vec();
        self.heap = entries
            .into_iter()
            .filter(|Reverse(e)| e.is_live(upgraded))
            .collect();
        self.live_at_rebuild = self.heap.len();
    }

    /// Pop every entry due at `now`; the caller expires them outside the lock.
    pub(crate) fn due(&mut self, now: u64) -> Vec<Arc<dyn Expirable>> {
        let mut due = Vec::new();
        while let Some(Reverse(top)) = self.heap.peek() {
            if top.at > now {
                break;
            }
            let Reverse(entry) = self.heap.pop().expect("peeked");
            if let Some(item) = entry.item.upgrade() {
                due.push(item);
            }
        }
        due
    }

    /// Drop every entry (the owner died).
    pub(crate) fn clear(&mut self) {
        self.heap.clear();
    }
}

// ---------------------------------------------------------------------------
// Service
// ---------------------------------------------------------------------------

/// The owner died: the service forgets it (its heap is gone).
pub(crate) fn forget(_registry: &Registry) {
    #[cfg(not(all(spawned_sim, feature = "async")))]
    thread_service::forget(_registry.key());
}

/// Ask the service to visit `registry` at `at`.
pub(crate) fn schedule(registry: &Arc<Registry>, at: u64) {
    #[cfg(all(spawned_sim, feature = "async"))]
    sim::schedule(registry, at);
    #[cfg(not(all(spawned_sim, feature = "async")))]
    thread_service::schedule(registry, at);
}

#[cfg(not(all(spawned_sim, feature = "async")))]
mod thread_service {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Condvar, Mutex, OnceLock};
    use std::time::Duration;

    #[derive(Default)]
    struct State {
        /// `(at, key)`; an entry is current only if `actors[key].0 == at`.
        queue: BinaryHeap<Reverse<(u64, u64)>>,
        actors: HashMap<u64, (u64, Weak<Registry>)>,
    }

    struct Service {
        state: Mutex<State>,
        wake: Condvar,
    }

    fn lock(service: &Service) -> std::sync::MutexGuard<'_, State> {
        service.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn service() -> &'static Service {
        static SERVICE: OnceLock<&'static Service> = OnceLock::new();
        SERVICE.get_or_init(|| {
            let service: &'static Service = Box::leak(Box::new(Service {
                state: Mutex::new(State::default()),
                wake: Condvar::new(),
            }));
            std::thread::Builder::new()
                .name("spawned-deadlines".into())
                .spawn(move || run(service))
                .expect("spawn the deadline service thread");
            service
        })
    }

    pub(super) fn forget(key: u64) {
        let Some(service) = SERVICE_REF.get() else {
            return;
        };
        let forgotten = lock(service).actors.remove(&key);
        drop(forgotten);
    }

    static SERVICE_REF: OnceLock<&'static Service> = OnceLock::new();

    pub(super) fn schedule(registry: &Arc<Registry>, at: u64) {
        let service = service();
        let _ = SERVICE_REF.set(service);
        let key = registry.key();
        let mut state = lock(service);
        match state.actors.get(&key) {
            Some((current, _)) if *current <= at => return,
            _ => {}
        }
        state.actors.insert(key, (at, Arc::downgrade(registry)));
        state.queue.push(Reverse((at, key)));
        if state.queue.len() > 2 * state.actors.len() + 64 {
            let actors = &state.actors;
            let live: BinaryHeap<_> = actors
                .iter()
                .map(|(key, (at, _))| Reverse((*at, *key)))
                .collect();
            state.queue = live;
        }
        drop(state);
        service.wake.notify_one();
    }

    fn run(service: &'static Service) {
        let mut state = lock(service);
        loop {
            let now = super::super::now_ticks();
            match state.queue.peek().copied() {
                None => {
                    state = service.wake.wait(state).unwrap_or_else(|p| p.into_inner());
                }
                Some(Reverse((at, key))) if at <= now => {
                    state.queue.pop();
                    let current = state.actors.get(&key).map(|(a, w)| (*a, w.clone()));
                    let Some((current_at, weak)) = current else {
                        continue;
                    };
                    if current_at != at {
                        continue; // superseded by an earlier schedule
                    }
                    state.actors.remove(&key);
                    drop(state);
                    if let Some(registry) = weak.upgrade() {
                        if let Some(next) = registry.service_deadlines(now) {
                            schedule(&registry, next);
                        }
                    }
                    state = lock(service);
                }
                Some(Reverse((at, _))) => {
                    let wait = Duration::from_nanos(at - now);
                    state = service
                        .wake
                        .wait_timeout(state, wait)
                        .unwrap_or_else(|p| p.into_inner())
                        .0;
                }
            }
        }
    }
}

#[cfg(all(spawned_sim, feature = "async"))]
mod sim {
    use super::*;
    use futures::task::AtomicWaker;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::task::Poll;
    use std::time::Duration;

    /// Per-heap service task state, kept by the registry.
    #[derive(Default)]
    pub(crate) struct SimService {
        started: AtomicBool,
        earliest: AtomicU64,
        waker: AtomicWaker,
    }

    pub(super) fn schedule(registry: &Arc<Registry>, at: u64) {
        let service = registry.sim_service();
        service.earliest.fetch_min(at, Ordering::AcqRel);
        if !service.started.swap(true, Ordering::AcqRel) {
            service.earliest.store(at, Ordering::Release);
            let weak = Arc::downgrade(registry);
            spawned_rt::tasks::spawn(run(weak));
        }
        service.waker.wake();
    }

    async fn run(weak: Weak<Registry>) {
        loop {
            let Some(registry) = weak.upgrade() else {
                return;
            };
            if registry.is_dead() {
                return;
            }
            let at = registry.sim_service().earliest.load(Ordering::Acquire);
            drop(registry);
            let now = super::super::now_ticks();
            if at <= now {
                let Some(registry) = weak.upgrade() else {
                    return;
                };
                registry
                    .sim_service()
                    .earliest
                    .store(u64::MAX, Ordering::Release);
                if let Some(next) = registry.service_deadlines(now) {
                    registry
                        .sim_service()
                        .earliest
                        .fetch_min(next, Ordering::AcqRel);
                }
                continue;
            }
            // Sleep until `at`, or until an earlier deadline or the death
            // wakes us.
            let mut sleep = (at != u64::MAX)
                .then(|| Box::pin(spawned_rt::tasks::sleep(Duration::from_nanos(at - now))));
            let weak_poll = weak.clone();
            std::future::poll_fn(|cx| {
                let Some(registry) = weak_poll.upgrade() else {
                    return Poll::Ready(());
                };
                let service = registry.sim_service();
                service.waker.register(cx.waker());
                if registry.is_dead() || service.earliest.load(Ordering::Acquire) < at {
                    return Poll::Ready(());
                }
                match &mut sleep {
                    Some(sleep) => sleep.as_mut().poll(cx),
                    None => Poll::Pending,
                }
            })
            .await;
        }
    }

    impl SimService {
        pub(crate) fn wake(&self) {
            self.waker.wake();
        }
    }
    use std::future::Future;
}

#[cfg(all(spawned_sim, feature = "async"))]
pub(crate) use sim::SimService;
