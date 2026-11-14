//! The owner's turn phase and the commit of reply obligations dropped in a
//! turn (docs/design/deferred-replies.md §4, "Drop classification").
//!
//! | phase | a dropped obligation |
//! |---|---|
//! | `InTurn` | recorded; committed at turn end: `Dropped` if the handler returned, `Stopped` if it panicked |
//! | `Running` | `Dropped` at once |
//! | `Exiting` / `Dead` | `Stopped` |
//!
//! `Stopped` means "answered with the owner's final reason": the registry
//! holds such a slot open (exit pending) until the death point, or closes it
//! at once when the owner is already dead.
//!
//! Shared with the Loom model (`tests/registry_turn_loom.rs`), which supplies
//! the synchronization types through `super::sync`. The protocol never runs
//! user code: it returns the items and their outcome, and the caller applies
//! them after every lock is released.

use super::sync::{AtomicU8, AtomicUsize, Mutex, Ordering};

pub(crate) const RUNNING: u8 = 0;
pub(crate) const IN_TURN: u8 = 1;
pub(crate) const IN_TURN_PENDING: u8 = 2;
pub(crate) const EXITING: u8 = 3;
pub(crate) const DEAD: u8 = 4;

/// How a dropped obligation is answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    Dropped,
    Stopped,
}

/// On its own cache line: the run loop writes it every turn.
#[repr(align(64))]
struct Phase(AtomicU8);

pub(crate) struct TurnPhase<T> {
    phase: Phase,
    // A deferred token stays counted until its entire drop has completed.
    // Only the actor can create tokens, before they leave its current turn.
    external: AtomicUsize,
    drops: Mutex<Vec<T>>,
}

impl<T> Default for TurnPhase<T> {
    fn default() -> Self {
        Self {
            phase: Phase(AtomicU8::new(RUNNING)),
            external: AtomicUsize::new(0),
            drops: Mutex::new(Vec::new()),
        }
    }
}

fn take<T>(drops: &Mutex<Vec<T>>) -> Vec<T> {
    std::mem::take(&mut *drops.lock().unwrap_or_else(|p| p.into_inner()))
}

impl<T> TurnPhase<T> {
    #[inline]
    pub(crate) fn load(&self) -> u8 {
        self.phase.0.load(Ordering::Acquire)
    }

    /// An obligation was dropped: `None` when it was recorded on the current
    /// turn, otherwise the item and how to answer it now.
    pub(crate) fn dropped(&self, item: T) -> Option<(T, Outcome)> {
        loop {
            match self.load() {
                RUNNING => return Some((item, Outcome::Dropped)),
                EXITING | DEAD => return Some((item, Outcome::Stopped)),
                _ => {
                    let mut drops = self.drops.lock().unwrap_or_else(|p| p.into_inner());
                    let mut current = self.load();
                    let recorded = loop {
                        if current != IN_TURN && current != IN_TURN_PENDING {
                            break false;
                        }
                        match self.phase.0.compare_exchange(
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
                        drops.push(item);
                        return None;
                    }
                    // The turn ended first: classify by the new phase.
                }
            }
        }
    }

    /// Called by the owner before publishing a new deferred token. A borrowed
    /// PendingReply cannot outlive the handler, so it needs no external hold.
    pub(crate) fn external_added(&self) {
        self.external.fetch_add(1, Ordering::Relaxed);
    }

    /// Retire a deferred token only after its drop/commit work. The owner
    /// acquiring zero then sees every earlier drop's phase and queue writes.
    pub(crate) fn external_finished(&self) {
        let previous = self.external.fetch_sub(1, Ordering::Release);
        debug_assert!(previous > 0, "deferred token ownership underflow");
    }

    #[cfg(all(test, feature = "async"))]
    pub(crate) fn external_count(&self) -> usize {
        self.external.load(Ordering::Acquire)
    }

    /// The run loop is about to dispatch a handler. Only the run loop moves
    /// the phase out of `RUNNING`, so a plain store suffices.
    #[inline]
    pub(crate) fn begin_turn(&self) {
        self.phase.0.store(IN_TURN, Ordering::Release);
    }

    /// The handler returned (`panicked == false`) or unwound: the recorded
    /// drops and their outcome.
    #[inline]
    pub(crate) fn end_turn(&self, panicked: bool) -> (Vec<T>, Outcome) {
        let outcome = if panicked {
            Outcome::Stopped
        } else {
            Outcome::Dropped
        };
        // The completed handler owns every borrowed obligation; none can
        // still be dropped concurrently. If no deferred token exists either,
        // no foreign writer can change this phase: an acquire of the last
        // token's release also sees the phase/queue writes of its whole drop.
        // Creating another token requires this owner to enter its next handler.
        // Exiting/dead transitions belong to the same run-loop/cleanup owner;
        // runtime destruction cannot drop a future while it is being polled.
        // Jobs can retain slots after a token is consumed, but only borrowed
        // PendingReply drops and counted ReplyTo drops call record_drop.
        // Otherwise retain the atomic exchange against a racing token drop.
        let previous = if self.external.load(Ordering::Acquire) == 0 {
            let previous = self.load();
            self.phase.0.store(RUNNING, Ordering::Release);
            previous
        } else {
            self.phase.0.swap(RUNNING, Ordering::AcqRel)
        };
        if previous == IN_TURN_PENDING {
            (take(&self.drops), outcome)
        } else {
            (Vec::new(), outcome)
        }
    }

    /// Exit begins (idempotent; not after death): recorded drops commit as
    /// `Stopped`.
    pub(crate) fn exiting(&self) -> Vec<T> {
        let mut current = self.load();
        loop {
            if current >= EXITING {
                return Vec::new();
            }
            match self.phase.0.compare_exchange(
                current,
                EXITING,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(previous) if previous == IN_TURN_PENDING => return take(&self.drops),
                Ok(_) => return Vec::new(),
                Err(actual) => current = actual,
            }
        }
    }

    /// Death: recorded drops commit as `Stopped`.
    pub(crate) fn close(&self) -> Vec<T> {
        if self.phase.0.swap(DEAD, Ordering::AcqRel) == IN_TURN_PENDING {
            take(&self.drops)
        } else {
            Vec::new()
        }
    }
}
