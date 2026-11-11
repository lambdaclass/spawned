//! The owner's turn phase and the commit of reply obligations dropped in a
//! turn (docs/design/deferred-replies.md §4, "Drop classification").
//!
//! | phase | a dropped obligation |
//! |---|---|
//! | `InTurn` | recorded; committed at turn end: `Dropped` if the handler returned, `Stopped` if it panicked |
//! | `Running` | `Dropped` at once |
//! | `Exiting` / `Dead` | `Stopped` at once |
//!
//! Shared with the Loom model (`tests/registry_turn_loom.rs`), which supplies
//! the synchronization types through `super::sync`. The protocol never runs
//! user code: it returns the items and their outcome, and the caller applies
//! them after every lock is released.

use super::sync::{AtomicU8, Mutex, Ordering};

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
    drops: Mutex<Vec<T>>,
}

impl<T> Default for TurnPhase<T> {
    fn default() -> Self {
        Self {
            phase: Phase(AtomicU8::new(RUNNING)),
            drops: Mutex::new(Vec::new()),
        }
    }
}

fn take<T>(drops: &Mutex<Vec<T>>) -> Vec<T> {
    std::mem::take(&mut *drops.lock().unwrap_or_else(|p| p.into_inner()))
}

impl<T> TurnPhase<T> {
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

    /// The run loop is about to dispatch a handler. Only the run loop moves
    /// the phase out of `RUNNING`, so a plain store suffices.
    pub(crate) fn begin_turn(&self) {
        self.phase.0.store(IN_TURN, Ordering::Release);
    }

    /// The handler returned (`panicked == false`) or unwound: the recorded
    /// drops and their outcome.
    pub(crate) fn end_turn(&self, panicked: bool) -> (Vec<T>, Outcome) {
        let outcome = if panicked {
            Outcome::Stopped
        } else {
            Outcome::Dropped
        };
        if self.phase.0.swap(RUNNING, Ordering::AcqRel) == IN_TURN_PENDING {
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
