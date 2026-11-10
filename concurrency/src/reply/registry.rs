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

use super::slot::{CreditPool, Slot, Terminal};
use crate::ActorId;
use std::sync::atomic::{AtomicU8, Ordering};
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
}

pub(crate) struct Registry {
    owner: ActorId,
    pool: Arc<CreditPool>,
    phase: AtomicU8,
    lists: Mutex<Lists>,
}

fn lock(lists: &Mutex<Lists>) -> std::sync::MutexGuard<'_, Lists> {
    lists.lock().unwrap_or_else(|p| p.into_inner())
}

impl Registry {
    pub(crate) fn new(owner: ActorId, max_outstanding: usize) -> Arc<Self> {
        let max = max_outstanding.min(super::slot::MAX_JOBS as usize);
        Arc::new(Self {
            owner,
            pool: Arc::new(CreditPool::new(max)),
            phase: AtomicU8::new(RUNNING),
            lists: Mutex::new(Lists {
                prune_at: 16,
                ..Lists::default()
            }),
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

    /// Placeholder for the deadline heap (step 5); deferred tokens are
    /// otherwise expired by their polling receiver or a reply.
    pub(crate) fn track_deadline<R: Send + 'static>(&self, _slot: &Arc<Slot<R>>) {}

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
    pub(crate) fn begin_turn(&self) {
        let _ = self
            .phase
            .compare_exchange(RUNNING, IN_TURN, Ordering::AcqRel, Ordering::Acquire);
    }

    /// The handler returned (`panicked == false`) or unwound.
    pub(crate) fn end_turn(&self, panicked: bool) {
        let previous = self
            .phase
            .compare_exchange(IN_TURN, RUNNING, Ordering::AcqRel, Ordering::Acquire)
            .or_else(|_| {
                self.phase.compare_exchange(
                    IN_TURN_PENDING,
                    RUNNING,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
            });
        if previous == Ok(IN_TURN_PENDING) {
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
        if self.phase.swap(EXITING, Ordering::AcqRel) == IN_TURN_PENDING {
            self.commit(Terminal::Stopped);
        }
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
        tokens
            .into_iter()
            .filter_map(|t| t.upgrade())
            .filter(|s| s.close())
            .count()
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
