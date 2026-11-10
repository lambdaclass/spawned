//! Wing–Gong/Lowe search over one partition, as in Porcupine's `checkSingle`.
use super::{Failure, Model, Operation, Refusal, Verdict};
use std::collections::HashSet;
use std::time::Instant;

const NIL: usize = usize::MAX;
const HEAD: usize = 0;

/// A call or return entry in a doubly linked list ordered by time. A call
/// entry links to its return entry so both can be lifted together.
struct Entry {
    /// Index into the partition's operations.
    op: usize,
    /// For a call entry, the index of its return entry; `NIL` for a return.
    matched: usize,
    prev: usize,
    next: usize,
}

struct Entries(Vec<Entry>);

impl Entries {
    /// Build the list: calls and returns sorted by time, a call before a
    /// return at the same time (so equal times overlap). Pending operations
    /// return after everything else.
    fn new<I, O>(ops: &[&Operation<I, O>]) -> Self {
        let mut events: Vec<(u64, bool, usize)> = Vec::with_capacity(ops.len() * 2);
        for (index, op) in ops.iter().enumerate() {
            events.push((op.call, false, index));
            events.push((op.ret().unwrap_or(u64::MAX), true, index));
        }
        events.sort_by_key(|&(time, is_return, index)| (time, is_return, index));
        let mut entries = vec![Entry {
            op: NIL,
            matched: NIL,
            prev: NIL,
            next: NIL,
        }];
        let mut call_slot = vec![NIL; ops.len()];
        for (position, &(_, is_return, op)) in events.iter().enumerate() {
            let slot = position + 1;
            entries.push(Entry {
                op,
                matched: NIL,
                prev: slot - 1,
                next: if position + 1 < events.len() {
                    slot + 1
                } else {
                    NIL
                },
            });
            if is_return {
                entries[call_slot[op]].matched = slot;
            } else {
                call_slot[op] = slot;
            }
        }
        entries[HEAD].next = if events.is_empty() { NIL } else { 1 };
        Self(entries)
    }

    fn unlink(&mut self, slot: usize) {
        let (prev, next) = (self.0[slot].prev, self.0[slot].next);
        self.0[prev].next = next;
        if next != NIL {
            self.0[next].prev = prev;
        }
    }

    fn relink(&mut self, slot: usize) {
        let (prev, next) = (self.0[slot].prev, self.0[slot].next);
        self.0[prev].next = slot;
        if next != NIL {
            self.0[next].prev = slot;
        }
    }

    /// Remove a call entry and its return entry.
    fn lift(&mut self, call: usize) {
        self.unlink(call);
        self.unlink(self.0[call].matched);
    }

    /// Undo [`Self::lift`]; lifts are undone in reverse order.
    fn unlift(&mut self, call: usize) {
        self.relink(self.0[call].matched);
        self.relink(call);
    }

    fn is_call(&self, slot: usize) -> bool {
        self.0[slot].matched != NIL
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct Bits(Box<[u64]>);

impl Bits {
    fn new(len: usize) -> Self {
        Self(vec![0; len.div_ceil(64)].into_boxed_slice())
    }
    fn set(&mut self, bit: usize) {
        self.0[bit / 64] |= 1 << (bit % 64);
    }
    fn clear(&mut self, bit: usize) {
        self.0[bit / 64] &= !(1 << (bit % 64));
    }
}

pub(super) struct Outcome {
    pub verdict: Verdict,
    pub states: usize,
    pub failure: Option<Failure>,
    pub unknown: Option<String>,
}

pub(super) fn check_partition<M: Model>(
    model: &M,
    history: &[Operation<M::Input, M::Output>],
    indices: &[usize],
    deadline: Option<Instant>,
    max_states: Option<usize>,
) -> Outcome {
    let ops: Vec<&Operation<M::Input, M::Output>> = indices.iter().map(|&i| &history[i]).collect();
    let mut entries = Entries::new(&ops);
    let mut state = model.init();
    let mut linearized = Bits::new(ops.len());
    let mut cache: HashSet<(Bits, M::State)> = HashSet::new();
    // The linearized prefix: each call entry with the state before it.
    let mut calls: Vec<(usize, M::State)> = Vec::new();
    let mut failure: Option<Failure> = None;
    let mut entry = entries.0[HEAD].next;
    let mut steps: u64 = 0;
    let finish = |verdict, cache: &HashSet<_>, failure, unknown| Outcome {
        verdict,
        states: cache.len(),
        failure,
        unknown,
    };
    loop {
        if entries.0[HEAD].next == NIL {
            return finish(Verdict::Linearizable, &cache, None, None);
        }
        steps += 1;
        if steps.is_multiple_of(1024) {
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                let reason = format!(
                    "timeout after {} states in a partition of {} operations",
                    cache.len(),
                    ops.len()
                );
                return finish(Verdict::Unknown, &cache, None, Some(reason));
            }
            if max_states.is_some_and(|max| cache.len() > max) {
                let reason = format!(
                    "state limit {} reached in a partition of {} operations",
                    cache.len(),
                    ops.len()
                );
                return finish(Verdict::Unknown, &cache, None, Some(reason));
            }
        }
        let op = entries.0[entry].op;
        if entries.is_call(entry) {
            if let Some(next_state) = model.step(&state, &ops[op].input, ops[op].output()) {
                linearized.set(op);
                if cache.insert((linearized.clone(), next_state.clone())) {
                    calls.push((entry, std::mem::replace(&mut state, next_state)));
                    entries.lift(entry);
                    entry = entries.0[HEAD].next;
                    continue;
                }
                linearized.clear(op);
            }
            entry = entries.0[entry].next;
            continue;
        }
        // A return entry: its operation was not linearized before its return.
        if ops[op].returned.is_none() {
            // Pending returns sort after every completed return, so reaching
            // one means every completed operation is linearized; pending ones
            // left over never took effect.
            return finish(Verdict::Linearizable, &cache, None, None);
        }
        if failure
            .as_ref()
            .is_none_or(|f| calls.len() > f.prefix.len())
        {
            failure = Some(describe(
                model,
                &ops,
                indices,
                &entries,
                entry,
                &state,
                &linearized,
                &calls,
                &cache,
            ));
        }
        let Some((call, previous)) = calls.pop() else {
            return finish(Verdict::Illegal, &cache, failure, None);
        };
        state = previous;
        linearized.clear(entries.0[call].op);
        entries.unlift(call);
        entry = entries.0[call].next;
    }
}

/// Snapshot the failure point: the prefix, the blocked operation and why each
/// call entry ahead of its return was refused.
#[allow(clippy::too_many_arguments)]
fn describe<M: Model>(
    model: &M,
    ops: &[&Operation<M::Input, M::Output>],
    indices: &[usize],
    entries: &Entries,
    blocked: usize,
    state: &M::State,
    linearized: &Bits,
    calls: &[(usize, M::State)],
    cache: &HashSet<(Bits, M::State)>,
) -> Failure {
    let mut candidates = Vec::new();
    let mut slot = entries.0[HEAD].next;
    while slot != blocked {
        if entries.is_call(slot) {
            let op = entries.0[slot].op;
            let refusal = match model.step(state, &ops[op].input, ops[op].output()) {
                None => Refusal::Rejected,
                Some(next) => {
                    let mut bits = linearized.clone();
                    bits.set(op);
                    debug_assert!(cache.contains(&(bits, next)));
                    Refusal::DeadEnd
                }
            };
            candidates.push((indices[op], refusal));
        }
        slot = entries.0[slot].next;
    }
    Failure {
        prefix: calls
            .iter()
            .map(|(call, _)| indices[entries.0[*call].op])
            .collect(),
        blocked: indices[entries.0[blocked].op],
        candidates,
    }
}
