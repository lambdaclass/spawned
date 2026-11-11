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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

/// Default `Actor::MAX_OUTSTANDING`: deferred credits per actor instance.
pub const DEFAULT_MAX_OUTSTANDING: usize = 1024;

use super::turn::{Outcome, TurnPhase, DEAD, EXITING};

fn terminal(outcome: Outcome) -> Terminal {
    match outcome {
        Outcome::Dropped => Terminal::Dropped,
        Outcome::Stopped => Terminal::Stopped,
    }
}

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
    deadlines: DeadlineHeap,
}

pub(crate) struct Registry {
    owner: ActorId,
    /// Service key, unique per registry.
    #[cfg_attr(all(spawned_sim, feature = "async"), allow(dead_code))]
    key: u64,
    pool: Arc<CreditPool>,
    /// Turn phase and in-turn drops (its phase sits on its own cache line).
    turn: TurnPhase<Arc<dyn Closeable>>,
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
            turn: TurnPhase::default(),
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
        if lists.tokens.len() < lists.prune_at {
            lists.tokens.push(weak);
            return;
        }
        // Prune outside the lock: a temporary upgrade can become a slot's
        // last strong reference, and dropping it drops an unread reply (user
        // code, which may itself drop a token of this owner).
        let listed = std::mem::take(&mut lists.tokens);
        drop(lists);
        let mut keep = Vec::with_capacity(listed.len());
        let mut upgraded = Vec::new();
        for token in listed {
            if let Some(slot) = token.upgrade() {
                if !slot.is_terminal() {
                    keep.push(token);
                }
                upgraded.push(slot);
            }
        }
        let mut lists = lock(&self.lists);
        keep.append(&mut lists.tokens);
        lists.prune_at = (keep.len() * 2).max(16);
        lists.tokens = keep;
        lists.tokens.push(weak);
        drop(lists);
        drop(upgraded);
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
        self.turn.load() == DEAD
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
        let (earliest, upgraded) = {
            let mut lists = lock(&self.lists);
            if self.is_dead() {
                return;
            }
            lists.deadlines.push(at, item)
        };
        // Compaction's temporary upgrades are dropped outside the lock.
        drop(upgraded);
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
        if let Some((slot, outcome)) = self.turn.dropped(slot) {
            slot.terminate(terminal(outcome));
        }
    }

    fn commit(drops: Vec<Arc<dyn Closeable>>, outcome: Outcome) {
        for slot in drops {
            slot.terminate(terminal(outcome));
        }
    }

    /// The run loop is about to dispatch a handler.
    pub(crate) fn begin_turn(&self) {
        self.turn.begin_turn();
    }

    /// The handler returned (`panicked == false`) or unwound.
    pub(crate) fn end_turn(&self, panicked: bool) {
        let (drops, outcome) = self.turn.end_turn(panicked);
        Self::commit(drops, outcome);
    }

    /// Exit begins: later drops answer `Stopped`, and the current turn's
    /// recorded drops commit as `Stopped`.
    pub(crate) fn exiting(&self) {
        if self.turn.load() == DEAD {
            return;
        }
        let first = {
            let mut began = self.exit_began.lock().unwrap_or_else(|p| p.into_inner());
            let first = began.is_none();
            began.get_or_insert_with(spawned_rt::time::Instant::now);
            first
        };
        Self::commit(self.turn.exiting(), Outcome::Stopped);
        if first {
            crate::lifecycle_trace::reply(self.owner.raw(), "Exit", 0, "");
        }
    }

    /// What remains of `timeout`, measured from the start of exit.
    pub(crate) fn cleanup_remaining(&self, timeout: std::time::Duration) -> std::time::Duration {
        let began = *self.exit_began.lock().unwrap_or_else(|p| p.into_inner());
        began.map_or(timeout, |began| timeout.saturating_sub(began.elapsed()))
    }

    pub(crate) fn is_exiting(&self) -> bool {
        self.turn.load() >= EXITING
    }

    /// Death: every open token listed becomes `STOPPED`, wherever it is.
    /// Returns how many were closed.
    pub(crate) fn close(&self) -> usize {
        crate::lifecycle_trace::reply(self.owner.raw(), "Death", 0, "");
        Self::commit(self.turn.close(), Outcome::Stopped);
        let tokens = std::mem::take(&mut lock(&self.lists).tokens);
        let closed = tokens
            .into_iter()
            .filter_map(|t| t.upgrade())
            .filter(|s| s.close())
            .count();
        // The heap is dropped after the close (§5 step 3.4), and the service
        // forgets this owner, so dead actors leave nothing behind.
        lock(&self.lists).deadlines.clear();
        deadline::forget(self);
        #[cfg(all(spawned_sim, feature = "async"))]
        self.sim.wake();
        closed
    }
}

impl Drop for Registry {
    fn drop(&mut self) {
        // Every handle to the instance is gone; nothing can still answer.
        if self.turn.load() != DEAD {
            self.close();
        }
    }
}

/// Closes the registry and cancels the jobs if the owner's lifecycle is
/// dropped without completing (for example, its runtime was destroyed).
/// Idempotent after a normal death.
#[cfg_attr(not(feature = "async"), allow(dead_code))]
pub(crate) struct CloseOnDrop {
    replies: Arc<Registry>,
    jobs: Arc<super::jobs::JobSet>,
}

#[cfg_attr(not(feature = "async"), allow(dead_code))]
impl CloseOnDrop {
    pub(crate) fn new(replies: Arc<Registry>, jobs: Arc<super::jobs::JobSet>) -> Self {
        Self { replies, jobs }
    }
}

impl Drop for CloseOnDrop {
    fn drop(&mut self) {
        if !self.replies.is_dead() {
            self.replies.exiting();
            self.jobs.cancel_all();
            self.replies.close();
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
