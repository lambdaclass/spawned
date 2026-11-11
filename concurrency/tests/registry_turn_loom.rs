//! Loom model of the owner's turn phase and in-turn drop commit
//! (`src/reply/turn.rs`, docs/design/deferred-replies.md §4): drops from
//! other threads racing turn end, exit and death. Every dropped obligation
//! gets exactly one outcome, `Stopped` exactly when the turn it landed on
//! panicked or the owner was exiting or dead, and none is lost.
#![cfg(spawned_scheduler_loom)]

mod sync {
    pub(crate) use loom::sync::atomic::{AtomicU8, Ordering};
    pub(crate) use loom::sync::Mutex;
}

#[allow(dead_code)]
#[path = "../src/reply/turn.rs"]
mod turn;

use loom::sync::Arc;
use std::sync::Mutex as StdMutex;
use turn::{Outcome, TurnPhase};

fn model(f: impl Fn() + Sync + Send + 'static) {
    let mut builder = loom::model::Builder::new();
    builder.preemption_bound = Some(3);
    builder.check(f);
}

/// Records each item's outcome; panics on a second one.
#[derive(Default)]
struct Outcomes(StdMutex<Vec<(u32, Outcome)>>);
impl Outcomes {
    fn apply(&self, items: Vec<u32>, outcome: Outcome) {
        for item in items {
            self.one(item, outcome);
        }
    }
    fn one(&self, item: u32, outcome: Outcome) {
        let mut seen = self.0.lock().unwrap();
        assert!(
            seen.iter().all(|(i, _)| *i != item),
            "item {item} answered twice"
        );
        seen.push((item, outcome));
    }
    fn of(&self, item: u32) -> Option<Outcome> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .find(|(i, _)| *i == item)
            .map(|(_, o)| *o)
    }
}

fn drop_on(turn: &TurnPhase<u32>, outcomes: &Outcomes, item: u32) {
    if let Some((item, outcome)) = turn.dropped(item) {
        outcomes.one(item, outcome);
    }
}

/// A drop from another thread racing the end of a turn that returned or
/// panicked: it commits with that turn, or (if after it) answers `Dropped`.
#[test]
fn drop_vs_turn_end() {
    for panicked in [false, true] {
        model(move || {
            let turn = Arc::new(TurnPhase::<u32>::default());
            let outcomes = Arc::new(Outcomes::default());
            turn.begin_turn();
            let dropper = {
                let (turn, outcomes) = (turn.clone(), outcomes.clone());
                loom::thread::spawn(move || drop_on(&turn, &outcomes, 1))
            };
            let (drops, outcome) = turn.end_turn(panicked);
            let committed = !drops.is_empty();
            outcomes.apply(drops, outcome);
            dropper.join().unwrap();
            let got = outcomes.of(1).expect("a dropped obligation was lost");
            if committed {
                let expected = if panicked {
                    Outcome::Stopped
                } else {
                    Outcome::Dropped
                };
                assert_eq!(got, expected);
            } else {
                assert_eq!(
                    got,
                    Outcome::Dropped,
                    "a drop after the turn answers at once"
                );
            }
        });
    }
}

/// Drops racing exit and death: nothing is lost, and anything recorded
/// on the interrupted turn commits as `Stopped`.
#[test]
fn drops_vs_exit_vs_death() {
    model(|| {
        let turn = Arc::new(TurnPhase::<u32>::default());
        let outcomes = Arc::new(Outcomes::default());
        turn.begin_turn();
        let droppers: Vec<_> = (1..=2)
            .map(|item| {
                let (turn, outcomes) = (turn.clone(), outcomes.clone());
                loom::thread::spawn(move || drop_on(&turn, &outcomes, item))
            })
            .collect();
        outcomes.apply(turn.exiting(), Outcome::Stopped);
        outcomes.apply(turn.close(), Outcome::Stopped);
        for dropper in droppers {
            dropper.join().unwrap();
        }
        for item in 1..=2 {
            assert_eq!(outcomes.of(item), Some(Outcome::Stopped), "item {item}");
        }
        assert_eq!(turn.load(), turn::DEAD);
    });
}

/// Exit is idempotent and never revives a dead owner.
#[test]
fn exiting_after_death_is_a_no_op() {
    model(|| {
        let turn = Arc::new(TurnPhase::<u32>::default());
        let late = {
            let turn = turn.clone();
            loom::thread::spawn(move || turn.exiting())
        };
        let closed = turn.close();
        assert!(closed.is_empty());
        assert!(late.join().unwrap().is_empty());
        assert_eq!(turn.load(), turn::DEAD);
    });
}
