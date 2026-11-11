//! Per-instance reply registry (docs/design/deferred-replies.md §4, §5).
//!
//! The registry holds the owner's credit pool, a list of weak references to
//! its deferred tokens' slots, and a phase that classifies reply drops:
//!
//! | phase | a dropped reply obligation |
//! |---|---|
//! | `InTurn` | recorded on the turn; committed at turn end: `Dropped` if the handler returned, exit pending if it panicked |
//! | `Running` | `Dropped` at once |
//! | `Exiting` | exit pending |
//! | `Dead` | answered `Stopped` at once (the final reason is known) |
//!
//! **Exit pending** (docs/design/otp-exit-signals.md, "Required changes to
//! rank 11"): the slot stays `OPEN` and listed, and the death point answers
//! it with the owner's final reason, so callers, `Down` and `wait_exit` see
//! one reason. Only the caller's own deadline (or abandonment) completes it
//! earlier; nobody holds a token that could still reply.
//!
//! The final reason is stored once, at the death point ([`Registry::seal`]),
//! before any slot of this owner becomes `STOPPED`; a caller that reads a
//! `STOPPED` slot reads it from here ([`Registry::final_reason`]).
//!
//! No rule uses `thread::panicking()`. The phase is one atomic so a turn costs
//! two uncontended atomic stores; the turn list is locked only when a drop
//! lands on it (`IN_TURN_PENDING`).

use super::deadline::{self, DeadlineHeap, Expirable};
use super::slot::{CreditPool, Slot, Terminal, NEVER};
use crate::{ActorId, ExitReason};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

/// Default `Actor::MAX_OUTSTANDING`: deferred credits per actor instance.
pub const DEFAULT_MAX_OUTSTANDING: usize = 1024;

use super::turn::{Outcome, TurnPhase, DEAD, EXITING};

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
    /// The owner's final exit reason, set once at its death point. Set means
    /// the mailbox refuses admission (`NoProc`).
    final_reason: OnceLock<ExitReason>,
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
            final_reason: OnceLock::new(),
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
    ///
    /// Entries run framework code that can reach user code (a waker, a
    /// payload destructor the entry did not route to its failure sink, the
    /// last reference to a job's `done`). Each runs contained, so one
    /// failing entry neither skips the others nor ends the shared service.
    pub(crate) fn service_deadlines(&self, now: u64) -> Option<u64> {
        let due = lock(&self.lists).deadlines.due(now);
        for item in due {
            contain_entry(|| {
                if item.pending_deadline().is_some_and(|d| d <= now) {
                    item.expire(now);
                }
                // Possibly the last reference: dropped inside the guard.
                drop(item);
            });
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
            self.commit(vec![slot], outcome);
        }
    }

    /// Answer `drops`: `Dropped` at once, or (`Stopped`) exit pending until
    /// the death point.
    fn commit(&self, drops: Vec<Arc<dyn Closeable>>, outcome: Outcome) {
        for slot in drops {
            match outcome {
                Outcome::Dropped => {
                    slot.terminate(Terminal::Dropped);
                }
                Outcome::Stopped => self.hold(slot),
            }
        }
    }

    /// Exit pending: keep `slot` open and listed so the death point answers
    /// it with the final reason. Past the death point, answer it now.
    fn hold(&self, slot: Arc<dyn Closeable>) {
        let mut lists = lock(&self.lists);
        // `close` sets `DEAD` before it takes the list under this lock, so a
        // slot listed here is either taken by that close or closed below.
        if self.is_dead() {
            drop(lists);
            slot.close();
        } else {
            lists.tokens.push(Arc::downgrade(&slot));
            drop(lists);
        }
        // The caller's receiver may have been the other strong reference:
        // dropping the last one runs no user code (the slot holds no value).
        drop(slot);
    }

    /// An undispatched envelope dropped its request (mailbox disposal, a
    /// rejected enqueue): answered at the death point with the final reason.
    pub(crate) fn dispose<R: Send + 'static>(&self, slot: Arc<Slot<R>>) {
        if !slot.is_terminal() {
            self.hold(slot);
        }
    }

    /// The death point D (called inside the owner's exit-table critical
    /// section): fix the final reason callers will see and refuse admission.
    /// Idempotent: the first reason stays.
    pub(crate) fn seal(&self, reason: &ExitReason) {
        let _ = self.final_reason.set(reason.clone());
    }

    /// The owner's final reason, once it reached its death point.
    pub(crate) fn final_reason(&self) -> Option<&ExitReason> {
        self.final_reason.get()
    }

    /// Whether the owner is past its death point: sends and requests report
    /// `NoProc`.
    pub(crate) fn refuses_admission(&self) -> bool {
        self.final_reason.get().is_some()
    }

    /// The run loop is about to dispatch a handler.
    pub(crate) fn begin_turn(&self) {
        self.turn.begin_turn();
    }

    /// The handler returned (`panicked == false`) or unwound.
    pub(crate) fn end_turn(&self, panicked: bool) {
        let (drops, outcome) = self.turn.end_turn(panicked);
        self.commit(drops, outcome);
    }

    /// Exit begins: later drops, and the current turn's recorded drops, are
    /// exit pending until the death point.
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
        self.commit(self.turn.exiting(), Outcome::Stopped);
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

    /// Death: every open token listed, and every exit-pending slot, becomes
    /// `STOPPED` with the final reason, wherever it is. Returns how many were
    /// closed.
    ///
    /// The run loops [`seal`](Self::seal) the reason at the death point
    /// first. A registry closed without it (its lifecycle was dropped by the
    /// runtime before the death point, G1) answers `Killed`.
    pub(crate) fn close(&self) -> usize {
        self.seal(&ExitReason::Killed);
        crate::lifecycle_trace::reply(self.owner.raw(), "Death", 0, "");
        for slot in self.turn.close() {
            slot.close();
        }
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

/// Run one deadline-service step; a panic is logged and disposed of.
pub(crate) fn contain_entry(step: impl FnOnce()) {
    if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(step)) {
        let message = crate::error::panic_message(&*payload);
        crate::mailbox::dispose_panic(payload);
        crate::actor_cleanup::log_safely(|| {
            tracing::error!(event = "deadline_entry_failed", %message,
                "a deadline entry panicked on the deadline service; servicing continues");
        });
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

    fn terminal_of<R>(slot: &Slot<R>) -> Option<Terminal> {
        Terminal::of(crate::reply::slot::state(slot.word()))
    }

    #[test]
    fn turn_drops_commit_dropped_or_wait_for_death() {
        let registry = Registry::new(ActorId::next(), 8);
        // A turn that returns answers its drops `Dropped`.
        let slot = deferred(&registry, NEVER);
        registry.begin_turn();
        registry.dropped(slot.clone());
        assert!(
            !slot.is_terminal(),
            "an in-turn drop waits for the turn end"
        );
        registry.end_turn(false);
        assert_eq!(terminal_of(&slot), Some(Terminal::Dropped));
        // Between turns a drop answers at once.
        let slot = deferred(&registry, NEVER);
        registry.dropped(slot.clone());
        assert_eq!(terminal_of(&slot), Some(Terminal::Dropped));
        // A panicking turn's drops, and drops while exiting, are exit
        // pending: open until the death point answers them with the final
        // reason.
        let unwound = deferred(&registry, NEVER);
        registry.begin_turn();
        registry.dropped(unwound.clone());
        registry.end_turn(true);
        registry.exiting();
        let exiting = deferred(&registry, NEVER);
        registry.dropped(exiting.clone());
        assert!(!unwound.is_terminal() && !exiting.is_terminal());
        registry.seal(&ExitReason::Shutdown);
        assert_eq!(registry.close(), 2);
        assert_eq!(terminal_of(&unwound), Some(Terminal::Stopped));
        assert_eq!(terminal_of(&exiting), Some(Terminal::Stopped));
        assert_eq!(registry.final_reason(), Some(&ExitReason::Shutdown));
        // Past the death point a drop answers at once.
        let late = Arc::new(Slot::<u32>::new(NEVER));
        registry.dropped(late.clone());
        assert_eq!(terminal_of(&late), Some(Terminal::Stopped));
        assert_eq!(registry.outstanding(), 0);
    }

    #[test]
    fn unsealed_close_answers_killed() {
        let registry = Registry::new(ActorId::next(), 8);
        let open = deferred(&registry, NEVER);
        assert_eq!(registry.close(), 1);
        assert_eq!(terminal_of(&open), Some(Terminal::Stopped));
        assert_eq!(registry.final_reason(), Some(&ExitReason::Killed));
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
