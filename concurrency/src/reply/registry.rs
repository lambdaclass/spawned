//! Per-instance reply registry (docs/design/deferred-replies.md §4, §5).
//!
//! The registry holds the owner's credit pool, a list of weak references to
//! its deferred tokens' slots, and a phase that classifies reply drops:
//!
//! | phase | a dropped reply obligation |
//! |---|---|
//! | `InTurn` | recorded on the turn; committed at turn end: `Dropped` if the handler returned, `Stopped` if it panicked |
//! | `Running` | `Dropped` at once |
//! | `Exiting` | `Stopped` at once |
//! | `Dead` | no-op: the close already answered it |
//!
//! No rule uses `thread::panicking()`. The phase is one atomic so a turn costs
//! two uncontended atomic stores; the turn list is locked only when a drop
//! lands on it (`IN_TURN_PENDING`).

use super::deadline::{self, DeadlineHeap, Expirable};
use super::slot::{CreditPool, Slot, Terminal, NEVER};
use crate::ActorId;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

/// Default `Actor::MAX_OUTSTANDING`: deferred credits per actor instance.
pub const DEFAULT_MAX_OUTSTANDING: usize = 1024;

const RUNNING: u8 = 0;
const IN_TURN: u8 = 1;
const IN_TURN_PENDING: u8 = 2;
const EXITING: u8 = 3;
const DEAD: u8 = 4;

/// A slot seen without its reply type.
pub(crate) trait Closeable: Send + Sync {
    fn close(&self) -> bool;
    fn terminate(&self, to: Terminal) -> bool;
    fn is_terminal(&self) -> bool;
}

impl<R: Send> Expirable for Slot<R> {
    fn pending_deadline(&self) -> Option<u64> {
        let deadline = self.deadline();
        (!self.is_terminal() && deadline != NEVER).then_some(deadline)
    }
    fn expire(&self, now: u64) {
        self.check_deadline(&|| now);
    }
}

impl<R: Send> Closeable for Slot<R> {
    fn close(&self) -> bool {
        Slot::close(self)
    }
    fn terminate(&self, to: Terminal) -> bool {
        Slot::terminate(self, to)
    }
    fn is_terminal(&self) -> bool {
        Slot::is_terminal(self)
    }
}

#[derive(Default)]
struct Lists {
    tokens: Vec<Weak<dyn Closeable>>,
    prune_at: usize,
    turn_drops: Vec<Arc<dyn Closeable>>,
    deadlines: DeadlineHeap,
}

pub(crate) struct Registry {
    owner: ActorId,
    /// Service key, unique per registry.
    #[cfg_attr(all(spawned_sim, feature = "async"), allow(dead_code))]
    key: u64,
    pool: Arc<CreditPool>,
    phase: AtomicU8,
    lists: Mutex<Lists>,
    /// When exit began: the cleanup deadline is armed from here.
    exit_began: Mutex<Option<spawned_rt::time::Instant>>,
    #[cfg(all(spawned_sim, feature = "async"))]
    sim: deadline::SimService,
}

fn lock(lists: &Mutex<Lists>) -> std::sync::MutexGuard<'_, Lists> {
    lists.lock().unwrap_or_else(|p| p.into_inner())
}

impl Registry {
    pub(crate) fn new(owner: ActorId, max_outstanding: usize) -> Arc<Self> {
        let max = max_outstanding.min(super::slot::MAX_JOBS as usize);
        static NEXT_KEY: AtomicU64 = AtomicU64::new(1);
        Arc::new(Self {
            owner,
            key: NEXT_KEY.fetch_add(1, Ordering::Relaxed),
            pool: Arc::new(CreditPool::new(max)),
            phase: AtomicU8::new(RUNNING),
            lists: Mutex::new(Lists {
                prune_at: 16,
                ..Lists::default()
            }),
            exit_began: Mutex::new(None),
            #[cfg(all(spawned_sim, feature = "async"))]
            sim: deadline::SimService::default(),
        })
    }

    /// The registry of replies not tied to an actor (`reply_pair`): always
    /// between turns, with the largest bound.
    pub(crate) fn detached() -> &'static Arc<Self> {
        static DETACHED: OnceLock<Arc<Registry>> = OnceLock::new();
        DETACHED.get_or_init(|| Self::new(ActorId::next(), usize::MAX))
    }

    pub(crate) fn pool(&self) -> &Arc<CreditPool> {
        &self.pool
    }

    pub(crate) fn owner(&self) -> ActorId {
        self.owner
    }

    /// Credits in use: live deferred tokens and running linked jobs.
    pub(crate) fn outstanding(&self) -> usize {
        self.pool.used()
    }

    /// List a newly deferred token. Called by `defer()` after its CAS, in the
    /// owner's turn, so it never races the close.
    pub(crate) fn register<R: Send + 'static>(&self, slot: &Arc<Slot<R>>) {
        let weak: Weak<dyn Closeable> = Arc::downgrade(slot) as Weak<dyn Closeable>;
        let mut lists = lock(&self.lists);
        if lists.tokens.len() >= lists.prune_at {
            lists
                .tokens
                .retain(|t| t.upgrade().is_some_and(|s| !s.is_terminal()));
            lists.prune_at = (lists.tokens.len() * 2).max(16);
        }
        lists.tokens.push(weak);
    }

    #[cfg_attr(all(spawned_sim, feature = "async"), allow(dead_code))]
    pub(crate) fn key(&self) -> u64 {
        self.key
    }

    #[cfg(all(spawned_sim, feature = "async"))]
    pub(crate) fn sim_service(&self) -> &deadline::SimService {
        &self.sim
    }

    pub(crate) fn is_dead(&self) -> bool {
        self.phase.load(Ordering::Acquire) == DEAD
    }

    /// Put a deferred token's deadline in the owner's heap, so it expires
    /// (and returns its credit) even when no caller polls.
    pub(crate) fn track_deadline<R: Send + 'static>(self: &Arc<Self>, slot: &Arc<Slot<R>>) {
        let at = slot.deadline();
        if at == NEVER || slot.is_terminal() {
            return;
        }
        let weak: Weak<dyn Expirable> = Arc::downgrade(slot) as Weak<dyn Expirable>;
        self.track(at, weak);
    }

    /// Add a heap entry; reschedule the service on a new earliest deadline.
    pub(crate) fn track(self: &Arc<Self>, at: u64, item: Weak<dyn Expirable>) {
        let earliest = {
            let mut lists = lock(&self.lists);
            if self.is_dead() {
                return;
            }
            lists.deadlines.push(at, item)
        };
        if earliest {
            deadline::schedule(self, at);
        }
    }

    /// Entries in the deadline heap, live and stale (bounded by compaction).
    #[cfg(test)]
    pub(crate) fn heap_entries(&self) -> usize {
        lock(&self.lists).deadlines.len()
    }

    /// The service visits: expire every due entry outside the lock, and
    /// return the next earliest deadline.
    pub(crate) fn service_deadlines(&self, now: u64) -> Option<u64> {
        let due = lock(&self.lists).deadlines.due(now);
        for item in due {
            if item.pending_deadline().is_some_and(|d| d <= now) {
                item.expire(now);
            }
        }
        lock(&self.lists).deadlines.earliest()
    }

    /// A reply obligation (`PendingReply` or `ReplyTo`) was dropped unanswered.
    pub(crate) fn dropped<R: Send + 'static>(&self, slot: Arc<Slot<R>>) {
        if slot.is_terminal() {
            return;
        }
        let slot: Arc<dyn Closeable> = slot;
        loop {
            match self.phase.load(Ordering::Acquire) {
                RUNNING => {
                    slot.terminate(Terminal::Dropped);
                    return;
                }
                EXITING => {
                    slot.terminate(Terminal::Stopped);
                    return;
                }
                DEAD => {
                    slot.terminate(Terminal::Stopped);
                    return;
                }
                _ => {
                    let mut lists = lock(&self.lists);
                    lists.turn_drops.push(slot.clone());
                    let mut current = self.phase.load(Ordering::Acquire);
                    let recorded = loop {
                        if current != IN_TURN && current != IN_TURN_PENDING {
                            break false;
                        }
                        match self.phase.compare_exchange(
                            current,
                            IN_TURN_PENDING,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        ) {
                            Ok(_) => break true,
                            Err(actual) => current = actual,
                        }
                    };
                    if recorded {
                        return;
                    }
                    // The turn ended first: classify by the new phase.
                    lists.turn_drops.pop();
                }
            }
        }
    }

    fn commit(&self, outcome: Terminal) {
        let drops = std::mem::take(&mut lock(&self.lists).turn_drops);
        for slot in drops {
            slot.terminate(outcome);
        }
    }

    /// The run loop is about to dispatch a handler.
    ///
    /// Only the run loop moves the phase out of `RUNNING`/`IN_TURN` (droppers
    /// only mark `IN_TURN_PENDING`), so a plain store suffices here.
    pub(crate) fn begin_turn(&self) {
        debug_assert_eq!(self.phase.load(Ordering::Relaxed), RUNNING);
        self.phase.store(IN_TURN, Ordering::Release);
    }

    /// The handler returned (`panicked == false`) or unwound.
    pub(crate) fn end_turn(&self, panicked: bool) {
        if self.phase.swap(RUNNING, Ordering::AcqRel) == IN_TURN_PENDING {
            self.commit(if panicked {
                Terminal::Stopped
            } else {
                Terminal::Dropped
            });
        }
    }

    /// Exit begins: later drops answer `Stopped`, and the current turn's
    /// recorded drops commit as `Stopped`.
    pub(crate) fn exiting(&self) {
        let previous = self.phase.load(Ordering::Acquire);
        if previous == DEAD {
            return;
        }
        self.exit_began
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get_or_insert_with(spawned_rt::time::Instant::now);
        if self.phase.swap(EXITING, Ordering::AcqRel) == IN_TURN_PENDING {
            self.commit(Terminal::Stopped);
        }
    }

    /// What remains of `timeout`, measured from the start of exit.
    pub(crate) fn cleanup_remaining(&self, timeout: std::time::Duration) -> std::time::Duration {
        let began = *self.exit_began.lock().unwrap_or_else(|p| p.into_inner());
        began.map_or(timeout, |began| timeout.saturating_sub(began.elapsed()))
    }

    pub(crate) fn is_exiting(&self) -> bool {
        self.phase.load(Ordering::Acquire) >= EXITING
    }

    /// Death: every open token listed becomes `STOPPED`, wherever it is.
    /// Returns how many were closed.
    pub(crate) fn close(&self) -> usize {
        if self.phase.swap(DEAD, Ordering::AcqRel) == IN_TURN_PENDING {
            self.commit(Terminal::Stopped);
        }
        let tokens = std::mem::take(&mut lock(&self.lists).tokens);
        let closed = tokens
            .into_iter()
            .filter_map(|t| t.upgrade())
            .filter(|s| s.close())
            .count();
        // The heap is dropped after the close (§5 step 3.4).
        lock(&self.lists).deadlines.clear();
        #[cfg(all(spawned_sim, feature = "async"))]
        self.sim.wake();
        closed
    }
}

impl Drop for Registry {
    fn drop(&mut self) {
        // Every handle to the instance is gone; nothing can still answer.
        if self.phase.load(Ordering::Acquire) != DEAD {
            self.close();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reply::slot::Slot;

    fn deferred(registry: &Arc<Registry>, deadline: u64) -> Arc<Slot<u32>> {
        let slot = Arc::new(Slot::new(deadline));
        slot.defer(registry.pool(), &|| 0).unwrap();
        registry.register(&slot);
        registry.track_deadline(&slot);
        slot
    }

    #[test]
    fn turn_drops_commit_dropped_or_stopped() {
        let registry = Registry::new(ActorId::next(), 8);
        for (panicked, expected) in [(false, Terminal::Dropped), (true, Terminal::Stopped)] {
            let slot = deferred(&registry, NEVER);
            registry.begin_turn();
            registry.dropped(slot.clone());
            assert!(
                !slot.is_terminal(),
                "an in-turn drop waits for the turn end"
            );
            registry.end_turn(panicked);
            assert_eq!(
                Terminal::of(crate::reply::slot::state(slot.word())),
                Some(expected)
            );
        }
        // Between turns a drop answers at once; while exiting, Stopped.
        let slot = deferred(&registry, NEVER);
        registry.dropped(slot.clone());
        assert_eq!(
            Terminal::of(crate::reply::slot::state(slot.word())),
            Some(Terminal::Dropped)
        );
        registry.exiting();
        let slot = deferred(&registry, NEVER);
        registry.dropped(slot.clone());
        assert_eq!(
            Terminal::of(crate::reply::slot::state(slot.word())),
            Some(Terminal::Stopped)
        );
        assert_eq!(registry.outstanding(), 0);
    }

    #[test]
    fn close_stops_every_listed_open_token_once() {
        let registry = Registry::new(ActorId::next(), 8);
        let open = deferred(&registry, NEVER);
        let answered = deferred(&registry, NEVER);
        answered.reply(1, &|| 0);
        assert_eq!(registry.close(), 1);
        assert_eq!(
            Terminal::of(crate::reply::slot::state(open.word())),
            Some(Terminal::Stopped)
        );
        assert_eq!(registry.outstanding(), 0);
        assert_eq!(registry.heap_entries(), 0, "the heap is dropped at death");
    }

    #[test]
    fn heap_stays_bounded_under_shortening_churn() {
        let registry = Registry::new(ActorId::next(), 64);
        let far = u64::MAX / 2;
        let slots: Vec<_> = (0..32).map(|i| deferred(&registry, far + i)).collect();
        for round in 0..200u64 {
            for slot in &slots {
                // Each shortening supersedes the previous entry (stale).
                slot.shorten(far - round - 1, &|| 0);
                registry.track_deadline(slot);
            }
            assert!(
                registry.heap_entries() <= 2 * slots.len() + 16 + slots.len(),
                "heap grew to {} entries for {} live tokens",
                registry.heap_entries(),
                slots.len()
            );
        }
        // Expire them all: the service visit empties the heap of due entries.
        assert_eq!(registry.service_deadlines(u64::MAX - 2), None);
        assert_eq!(registry.outstanding(), 0);
    }
}
