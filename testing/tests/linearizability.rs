//! The linearizability checker against known-good and known-bad histories,
//! a brute-force reference, generated sequential histories and large inputs.
use proptest::prelude::*;
use spawned_test::linearizability::{
    check, check_with, partition_by_key, CheckOptions, Model, Operation, Recorder, Refusal, Verdict,
};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------- models

/// A single register holding a `u64`, initially 0.
struct Register;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reg {
    Write(u64),
    Read,
}

impl Model for Register {
    type State = u64;
    type Input = Reg;
    type Output = u64;
    fn init(&self) -> u64 {
        0
    }
    fn step(&self, state: &u64, input: &Reg, output: Option<&u64>) -> Option<u64> {
        match input {
            Reg::Write(value) => Some(*value),
            Reg::Read => output.is_none_or(|seen| seen == state).then_some(*state),
        }
    }
}

/// A map from keys to optional values, checked per key.
struct Kv;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KvOp {
    Put(u8, u64),
    Get(u8),
}

impl KvOp {
    fn key(&self) -> u8 {
        match self {
            KvOp::Put(key, _) | KvOp::Get(key) => *key,
        }
    }
}

impl Model for Kv {
    type State = Option<u64>;
    type Input = KvOp;
    type Output = Option<u64>;
    fn init(&self) -> Option<u64> {
        None
    }
    fn step(
        &self,
        state: &Option<u64>,
        input: &KvOp,
        output: Option<&Option<u64>>,
    ) -> Option<Option<u64>> {
        match input {
            KvOp::Put(_, value) => Some(Some(*value)),
            KvOp::Get(_) => output.is_none_or(|seen| seen == state).then_some(*state),
        }
    }
    fn partition(&self, history: &[Operation<KvOp, Option<u64>>]) -> Vec<Vec<usize>> {
        partition_by_key(history, KvOp::key)
    }
}

/// FIFO queues, one per sender, checked per sender: a mailbox must hand each
/// sender's messages to the handler in send order.
struct Queues;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Q {
    Enqueue(u8, u64),
    Dequeue(u8),
}

impl Model for Queues {
    type State = VecDeque<u64>;
    type Input = Q;
    type Output = Option<u64>;
    fn init(&self) -> VecDeque<u64> {
        VecDeque::new()
    }
    fn step(
        &self,
        state: &VecDeque<u64>,
        input: &Q,
        output: Option<&Option<u64>>,
    ) -> Option<VecDeque<u64>> {
        let mut next = state.clone();
        match input {
            Q::Enqueue(_, value) => {
                next.push_back(*value);
                Some(next)
            }
            Q::Dequeue(_) => {
                let front = next.pop_front();
                output.is_none_or(|seen| *seen == front).then_some(next)
            }
        }
    }
    fn partition(&self, history: &[Operation<Q, Option<u64>>]) -> Vec<Vec<usize>> {
        partition_by_key(history, |q| match q {
            Q::Enqueue(sender, _) | Q::Dequeue(sender) => *sender,
        })
    }
}

fn op<I, O>(client: usize, input: I, call: u64, output: O, ret: u64) -> Operation<I, O> {
    Operation::complete(client, input, call, output, ret)
}

fn verdict<M: Model>(model: &M, history: &[Operation<M::Input, M::Output>]) -> Verdict {
    check(model, history).verdict()
}

// ------------------------------------------------- known histories

#[test]
fn sequential_and_overlapping_register_histories() {
    let history = vec![
        op(0, Reg::Write(1), 0, 0, 1),
        op(1, Reg::Read, 2, 1, 3),
        op(0, Reg::Write(2), 4, 0, 5),
        op(1, Reg::Read, 6, 2, 7),
    ];
    assert_eq!(verdict(&Register, &history), Verdict::Linearizable);
    // A read overlapping a write may see either value.
    for seen in [0, 1] {
        let history = vec![op(0, Reg::Write(1), 0, 0, 3), op(1, Reg::Read, 1, seen, 2)];
        assert_eq!(verdict(&Register, &history), Verdict::Linearizable);
    }
    assert_eq!(verdict(&Register, &[]), Verdict::Linearizable);
}

#[test]
fn stale_read_is_rejected_with_an_explanation() {
    let history = vec![op(0, Reg::Write(1), 0, 0, 1), op(1, Reg::Read, 2, 0, 3)];
    let report = check(&Register, &history);
    assert_eq!(report.verdict(), Verdict::Illegal);
    let failure = report.partitions[0].failure.as_ref().unwrap();
    assert_eq!(failure.prefix, vec![0]);
    assert_eq!(failure.blocked, 1);
    assert_eq!(failure.candidates, vec![(1, Refusal::Rejected)]);
    let text = report.explain(&Register, &history);
    assert!(
        text.contains("longest linearizable prefix: 1 of 2"),
        "{text}"
    );
    assert!(
        text.contains("cannot linearize #1 client 1 Read -> 0"),
        "{text}"
    );
    let html = report.to_html(&Register, &history);
    assert!(html.contains("class=\"op blocked\"") && html.contains("class=\"op lin\""));
}

#[test]
fn new_old_inversion_is_rejected() {
    // Once a read has observed the overlapping write, a later read cannot
    // observe the old value.
    let history = vec![
        op(0, Reg::Write(1), 0, 0, 10),
        op(1, Reg::Read, 1, 1, 2),
        op(2, Reg::Read, 3, 0, 4),
    ];
    assert_eq!(verdict(&Register, &history), Verdict::Illegal);
    // In the other order, both reads fit around the write.
    let history = vec![
        op(0, Reg::Write(1), 0, 0, 10),
        op(1, Reg::Read, 1, 0, 2),
        op(2, Reg::Read, 3, 1, 4),
    ];
    assert_eq!(verdict(&Register, &history), Verdict::Linearizable);
}

#[test]
fn lost_write_and_read_your_write_violations_are_rejected() {
    // Lost write: the second acknowledged put disappears.
    let lost = vec![
        op(0, KvOp::Put(1, 10), 0, None, 1),
        op(0, KvOp::Put(1, 20), 2, None, 3),
        op(1, KvOp::Get(1), 4, Some(10), 5),
    ];
    assert_eq!(verdict(&Kv, &lost), Verdict::Illegal);
    // Read-your-write: a client does not see its own completed put, even with
    // unrelated traffic on other keys.
    let ryw = vec![
        op(0, KvOp::Put(1, 10), 0, None, 1),
        op(1, KvOp::Put(2, 5), 0, None, 3),
        op(0, KvOp::Get(1), 2, None, 3),
    ];
    assert_eq!(verdict(&Kv, &ryw), Verdict::Illegal);
    // A concurrent overwrite makes an "old" read legal.
    let raced = vec![
        op(0, KvOp::Put(1, 10), 0, None, 1),
        op(1, KvOp::Put(1, 20), 2, None, 6),
        op(0, KvOp::Get(1), 3, Some(10), 4),
        op(0, KvOp::Get(1), 7, Some(20), 8),
    ];
    assert_eq!(verdict(&Kv, &raced), Verdict::Linearizable);
}

#[test]
fn pending_operations_may_or_may_not_take_effect() {
    let pending = || Operation::pending(0, Reg::Write(1), 0);
    // May have taken effect: seen.
    let seen = vec![pending(), op(1, Reg::Read, 2, 1, 3)];
    assert_eq!(verdict(&Register, &seen), Verdict::Linearizable);
    // May not have: never seen.
    let unseen = vec![pending(), op(1, Reg::Read, 2, 0, 3)];
    assert_eq!(verdict(&Register, &unseen), Verdict::Linearizable);
    // Taking effect late is allowed...
    let late = vec![
        pending(),
        op(1, Reg::Read, 2, 0, 3),
        op(1, Reg::Read, 4, 1, 5),
    ];
    assert_eq!(verdict(&Register, &late), Verdict::Linearizable);
    // ...but once seen, it cannot be undone.
    let undone = vec![
        pending(),
        op(1, Reg::Read, 2, 1, 3),
        op(1, Reg::Read, 4, 0, 5),
    ];
    assert_eq!(verdict(&Register, &undone), Verdict::Illegal);
    // And it cannot take effect before it was called.
    let early = vec![
        op(1, Reg::Read, 0, 1, 1),
        Operation::pending(0, Reg::Write(1), 2),
    ];
    assert_eq!(verdict(&Register, &early), Verdict::Illegal);
    // A value that must come from a pending write forces it to take effect,
    // in order with other pending writes.
    let forced = vec![
        Operation::pending(0, Reg::Write(1), 0),
        Operation::pending(1, Reg::Write(2), 1),
        op(2, Reg::Read, 2, 1, 3),
        op(2, Reg::Read, 4, 2, 5),
        op(2, Reg::Read, 6, 1, 7),
    ];
    assert_eq!(verdict(&Register, &forced), Verdict::Illegal);
}

#[test]
fn queue_order_violations_are_rejected_per_sender() {
    let fifo = vec![
        op(0, Q::Enqueue(0, 1), 0, None, 1),
        op(0, Q::Enqueue(0, 2), 2, None, 3),
        op(1, Q::Dequeue(0), 4, Some(1), 5),
        op(1, Q::Dequeue(0), 6, Some(2), 7),
    ];
    assert_eq!(verdict(&Queues, &fifo), Verdict::Linearizable);
    let mut reordered = fifo.clone();
    reordered[2].returned = Some((5, Some(2)));
    reordered[3].returned = Some((7, Some(1)));
    assert_eq!(verdict(&Queues, &reordered), Verdict::Illegal);
    let mut duplicated = fifo.clone();
    duplicated[3].returned = Some((7, Some(1)));
    assert_eq!(verdict(&Queues, &duplicated), Verdict::Illegal);
    let mut dropped = fifo.clone();
    dropped[3].returned = Some((7, None));
    assert_eq!(verdict(&Queues, &dropped), Verdict::Illegal);
    // Concurrent enqueues may land in either order.
    let concurrent = vec![
        op(0, Q::Enqueue(0, 1), 0, None, 3),
        op(1, Q::Enqueue(0, 2), 1, None, 2),
        op(2, Q::Dequeue(0), 4, Some(2), 5),
        op(2, Q::Dequeue(0), 6, Some(1), 7),
    ];
    assert_eq!(verdict(&Queues, &concurrent), Verdict::Linearizable);
    // Two senders interleave freely; only each sender's own order counts.
    let interleaved = vec![
        op(0, Q::Enqueue(0, 1), 0, None, 1),
        op(1, Q::Enqueue(1, 9), 0, None, 1),
        op(0, Q::Enqueue(0, 2), 2, None, 3),
        op(2, Q::Dequeue(1), 4, Some(9), 5),
        op(2, Q::Dequeue(0), 6, Some(1), 7),
    ];
    assert_eq!(verdict(&Queues, &interleaved), Verdict::Linearizable);
}

#[test]
fn partitions_are_checked_independently() {
    let mut history = Vec::new();
    for key in 0..4u8 {
        let base = key as u64 * 10;
        history.push(op(0, KvOp::Put(key, base), base, None, base + 1));
        history.push(op(1, KvOp::Get(key), base + 2, Some(base), base + 3));
    }
    let report = check(&Kv, &history);
    assert_eq!(report.verdict(), Verdict::Linearizable);
    assert_eq!(report.partitions.len(), 4);
    history[5].returned = Some((23, Some(99)));
    let report = check(&Kv, &history);
    assert_eq!(report.verdict(), Verdict::Illegal);
    let failure = report.partitions[2].failure.as_ref().unwrap();
    assert_eq!(failure.blocked, 5);
    assert!(report.explain(&Kv, &history).contains("partition 2"));
}

#[test]
fn exhausted_budget_is_unknown_not_a_pass() {
    // Twenty overlapping writes, then a read of a value nobody wrote: refuting
    // it means exploring every subset of writes.
    let mut history: Vec<_> = (0..20)
        .map(|i| op(i, Reg::Write(i as u64 + 1), i as u64, 0, 100))
        .collect();
    history.push(op(99, Reg::Read, 101, 999, 102));
    let tight = CheckOptions {
        timeout: None,
        max_states: Some(1_000),
    };
    let report = check_with(&Register, &history, &tight);
    assert_eq!(report.verdict(), Verdict::Unknown);
    assert!(report.unknown.as_deref().unwrap().contains("state limit"));
    let timed = CheckOptions {
        timeout: Some(Duration::ZERO),
        max_states: None,
    };
    let report = check_with(&Register, &history, &timed);
    assert_eq!(report.verdict(), Verdict::Unknown);
    assert!(report.unknown.as_deref().unwrap().contains("timeout"));
}

#[test]
fn recorder_stamps_calls_and_records_dropped_calls_as_pending() {
    let recorder: Recorder<Reg, u64> = Recorder::new();
    let write = recorder.call(0, Reg::Write(5));
    let read = recorder.call(1, Reg::Read);
    write.complete(0);
    drop(read);
    let late = recorder.call(1, Reg::Read);
    assert_eq!(late.input(), &Reg::Read);
    late.complete(5);
    let history = recorder.take();
    assert_eq!(history.len(), 3);
    assert_eq!(history[0].ret(), Some(2));
    assert!(history[1].returned.is_none());
    assert_eq!((history[2].call, history[2].ret()), (3, Some(4)));
    assert_eq!(verdict(&Register, &history), Verdict::Linearizable);
    assert!(recorder.take().is_empty());
}

// ------------------------------------------------- brute-force reference

/// Exhaustive search: try every order that respects real time, allowing
/// pending operations to be skipped.
fn brute_force<M: Model>(model: &M, history: &[Operation<M::Input, M::Output>]) -> bool {
    fn go<M: Model>(
        model: &M,
        history: &[Operation<M::Input, M::Output>],
        remaining: &mut Vec<usize>,
        state: &M::State,
    ) -> bool {
        if remaining.iter().all(|&i| history[i].returned.is_none()) {
            return true;
        }
        for position in 0..remaining.len() {
            let candidate = remaining[position];
            let call = history[candidate].call;
            let blocked = remaining
                .iter()
                .any(|&other| history[other].ret().is_some_and(|ret| ret < call));
            if blocked {
                continue;
            }
            let op = &history[candidate];
            if let Some(next) = model.step(state, &op.input, op.output()) {
                remaining.remove(position);
                let found = go(model, history, remaining, &next);
                remaining.insert(position, candidate);
                if found {
                    return true;
                }
            }
        }
        false
    }
    go(
        model,
        history,
        &mut (0..history.len()).collect(),
        &model.init(),
    )
}

/// Small register histories with arbitrary times, outputs and pending ops.
fn arbitrary_register_history() -> impl Strategy<Value = Vec<Operation<Reg, u64>>> {
    let operation = (
        0..4usize,
        prop_oneof![(1..4u64).prop_map(Reg::Write), Just(Reg::Read)],
        0..12u64,
        0..6u64,
        0..4u64,
        prop::bool::weighted(0.15),
    )
        .prop_map(|(client, input, call, length, output, pending)| {
            if pending {
                Operation::pending(client, input, call)
            } else {
                Operation::complete(client, input, call, output, call + length)
            }
        });
    prop::collection::vec(operation, 0..8)
}

/// Sequential executions of a key-value map, each operation stretched around
/// its linearization point so neighbours overlap; some replies are lost.
fn sequential_kv_history() -> impl Strategy<Value = Vec<Operation<KvOp, Option<u64>>>> {
    let step = (
        prop_oneof![
            (0..4u8, 0..5u64).prop_map(|(k, v)| KvOp::Put(k, v)),
            (0..4u8).prop_map(KvOp::Get)
        ],
        0..6u64,
        0..6u64,
        prop::bool::weighted(0.05),
    );
    prop::collection::vec(step, 0..300).prop_map(|steps| {
        let mut map = std::collections::HashMap::new();
        steps
            .into_iter()
            .enumerate()
            .map(|(i, (input, before, after, pending))| {
                let point = 10 + 4 * i as u64;
                let output = match input {
                    KvOp::Put(key, value) => {
                        map.insert(key, value);
                        None
                    }
                    KvOp::Get(key) => map.get(&key).copied(),
                };
                let call = point - before;
                if pending {
                    Operation::pending(i % 5, input, call)
                } else {
                    Operation::complete(i % 5, input, call, output, point + after)
                }
            })
            .collect()
    })
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 512,
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    #[test]
    fn agrees_with_brute_force(history in arbitrary_register_history()) {
        let expected = brute_force(&Register, &history);
        let report = check(&Register, &history);
        prop_assert_eq!(report.verdict(), if expected { Verdict::Linearizable } else { Verdict::Illegal });
        if let Some(failure) = report.partitions[0].failure.as_ref() {
            // The reported prefix replays and the blocked op is completed.
            let mut state = Register.init();
            for &i in &failure.prefix {
                state = Register.step(&state, &history[i].input, history[i].output()).unwrap();
            }
            prop_assert!(history[failure.blocked].returned.is_some());
        }
    }

    #[test]
    fn sequential_histories_are_always_accepted(history in sequential_kv_history()) {
        prop_assert_eq!(verdict(&Kv, &history), Verdict::Linearizable);
    }
}

// ------------------------------------------------- scale

/// A linearizable history of `ops` operations from `clients` concurrent
/// clients over `keys` keys: each op's interval spans up to `overlap`
/// neighbouring linearization points.
fn large_history(
    ops: usize,
    keys: u8,
    clients: usize,
    overlap: u64,
    values: u64,
    seed: u64,
) -> Vec<Operation<KvOp, Option<u64>>> {
    let mut rng = seed;
    let mut next = move || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    let mut map = std::collections::HashMap::new();
    (0..ops)
        .map(|i| {
            let key = (next() % keys as u64) as u8;
            let point = 1_000 + 10 * i as u64;
            let spread = 10 * overlap;
            let (call, ret) = (point - next() % spread, point + next() % spread);
            let client = i % clients;
            if next().is_multiple_of(2) {
                let value = i as u64 % values;
                map.insert(key, value);
                Operation::complete(client, KvOp::Put(key, value), call, None, ret)
            } else {
                let seen = map.get(&key).copied();
                Operation::complete(client, KvOp::Get(key), call, seen, ret)
            }
        })
        .collect()
}

#[test]
fn checks_thousands_of_operations_within_seconds() {
    // 5,000 operations over 20 keys (about 250 per key), each overlapping
    // about 16 neighbours.
    let history = large_history(5_000, 20, 16, 8, u64::MAX, 0x5eed);
    let report = check(&Kv, &history);
    assert_eq!(report.verdict(), Verdict::Linearizable);
    assert!(
        report.elapsed < Duration::from_secs(20),
        "{:?}",
        report.elapsed
    );
    // A single corrupted read is found and explained.
    let mut broken = history.clone();
    let victim = broken
        .iter()
        .rposition(|op| matches!(op.input, KvOp::Get(_)) && op.output() != Some(&None))
        .unwrap();
    broken[victim].returned.as_mut().unwrap().1 = Some(u64::MAX);
    let report = check(&Kv, &broken);
    assert_eq!(report.verdict(), Verdict::Illegal);
    assert!(
        report
            .partitions
            .last()
            .unwrap()
            .failure
            .as_ref()
            .unwrap()
            .blocked
            == victim
    );
    println!(
        "5000 ops / 20 keys: legal checked in {:?}, corruption found in {:?}",
        check(&Kv, &history).elapsed,
        report.elapsed
    );
}

/// `cargo test --release -p spawned-test --test linearizability -- --ignored --nocapture`
#[test]
#[ignore = "benchmark; run with --release --ignored --nocapture"]
fn scale_table() {
    println!("ops\tkeys\tper key\toverlap\tvalues\tverdict\tstates\ttime");
    for &(ops, keys, overlap, values) in &[
        (1_000, 1, 4, u64::MAX),
        (5_000, 1, 4, u64::MAX),
        (1_000, 1, 8, u64::MAX),
        (1_000, 1, 8, 3),
        (10_000, 20, 8, u64::MAX),
        (10_000, 20, 8, 3),
        (100_000, 100, 8, u64::MAX),
        (5_000, 10, 16, u64::MAX),
        (5_000, 10, 16, 3),
    ] {
        let history = large_history(ops, keys, 16, overlap, values, 7);
        let mut broken = history.clone();
        let victim = broken
            .iter()
            .rposition(|op| matches!(op.input, KvOp::Get(_)))
            .unwrap();
        broken[victim].returned.as_mut().unwrap().1 = Some(u64::MAX);
        for (name, history) in [("legal", &history), ("corrupt", &broken)] {
            let started = Instant::now();
            let report = check(&Kv, history);
            println!(
                "{ops}\t{keys}\t{}\t{overlap}\t{}\t{name}:{:?}\t{}\t{:?}",
                ops / keys as usize,
                if values == u64::MAX {
                    "unique".to_string()
                } else {
                    values.to_string()
                },
                report.verdict(),
                report.states(),
                started.elapsed(),
            );
        }
    }
}
