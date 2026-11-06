//! Independent bounded sequential oracle. No runtime counters or implementation state.
#![allow(dead_code)]
use super::actor::Reply;
use std::collections::HashSet;
#[derive(Clone, Debug)]
pub enum Action {
    Append(u64),
    Read,
}
#[derive(Clone, Debug)]
pub enum Outcome {
    Known {
        finished: u64,
        reply: Reply,
    },
    /// Timeout/closed reply is indeterminate: the actor-side operation may still finish.
    Unknown,
    /// Only a proven enqueue rejection establishes no effect.
    Rejected {
        finished: u64,
    },
}
#[derive(Clone, Debug)]
pub struct Operation {
    pub invoked: u64,
    pub action: Action,
    pub outcome: Outcome,
}
impl Operation {
    fn finished(&self) -> Option<u64> {
        match self.outcome {
            Outcome::Known { finished, .. } | Outcome::Rejected { finished } => Some(finished),
            Outcome::Unknown => None,
        }
    }
}
pub fn check(operations: &[Operation]) -> Result<bool, &'static str> {
    if operations.len() > 16 {
        return Err("history exceeds the independent checker's bound");
    }
    fn search(
        ops: &[Operation],
        state: Vec<u64>,
        done: u64,
        visited: &mut HashSet<(u64, Vec<u64>)>,
    ) -> Result<bool, &'static str> {
        if done == (1u64 << ops.len()) - 1 {
            return Ok(true);
        }
        if !visited.insert((done, state.clone())) {
            return Ok(false);
        }
        if visited.len() > 100_000 {
            return Err("history search inconclusive: state limit reached");
        }
        for (i, op) in ops.iter().enumerate() {
            if done & (1 << i) != 0 {
                continue;
            }
            if ops.iter().enumerate().any(|(j, previous)| {
                done & (1 << j) == 0 && previous.finished().is_some_and(|end| end < op.invoked)
            }) {
                continue;
            }
            let next = done | (1 << i);
            if matches!(op.outcome, Outcome::Rejected { .. } | Outcome::Unknown)
                && search(ops, state.clone(), next, visited)?
            {
                return Ok(true);
            }
            if matches!(op.outcome, Outcome::Rejected { .. }) {
                continue;
            }
            let (new_state, expected) = match op.action {
                Action::Append(id) => {
                    let mut values = state.clone();
                    values.push(id);
                    let reply = Reply::Appended {
                        id,
                        length: values.len(),
                    };
                    (values, reply)
                }
                Action::Read => (state.clone(), Reply::Values(state.clone())),
            };
            let matches = match &op.outcome {
                Outcome::Known { reply, .. } => reply == &expected,
                Outcome::Unknown => true,
                Outcome::Rejected { .. } => false,
            };
            if matches && search(ops, new_state, next, visited)? {
                return Ok(true);
            }
        }
        Ok(false)
    }
    search(operations, Vec::new(), 0, &mut HashSet::new())
}
