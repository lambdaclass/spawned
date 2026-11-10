//! Concurrent request histories checked for linearizability with
//! `spawned_test::linearizability` (a Porcupine-style checker). Timed-out work
//! is pending (may or may not have taken effect), never assumed absent; only a
//! proven enqueue rejection establishes no effect.
// Tests that need OS threads are compiled out under spawned_sim.
#![cfg_attr(spawned_sim, allow(unused_imports, dead_code))]
#[path = "support/failure_actor.rs"]
mod actor;
#[path = "support/bounded_history.rs"]
mod bounded;
#[path = "support/store_actor.rs"]
mod store;
use actor::*;
use bounded::{Action, Operation as Bounded, Outcome};
use proptest::prelude::*;
use proptest::strategy::ValueTree;
use proptest::test_runner::{Config, TestRunner};
use spawned_concurrency::{ActorError, ExitReason};
use spawned_test::linearizability::{check, Model, Operation, Recorder, Verdict};
use std::{
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use store::{Bug, KeyState, NativeStore, Store, StoreModel, StoreOp};

// ------------------------------------------------- the append-log worker

/// What a request to the append-log `Worker` produced.
#[derive(Clone, Debug, PartialEq, Eq)]
enum LogReply {
    Reply(Reply),
    /// The enqueue was rejected: proven to have had no effect.
    Rejected,
}

/// The `Worker` specification: an append-only log; `Read` returns all of it.
struct AppendLog;

impl Model for AppendLog {
    type State = Vec<u64>;
    type Input = Action;
    type Output = LogReply;
    fn init(&self) -> Vec<u64> {
        Vec::new()
    }
    fn step(
        &self,
        state: &Vec<u64>,
        input: &Action,
        output: Option<&LogReply>,
    ) -> Option<Vec<u64>> {
        if output == Some(&LogReply::Rejected) {
            return Some(state.clone());
        }
        let mut next = state.clone();
        let expected = match input {
            Action::Append(id) => {
                next.push(*id);
                Reply::Appended {
                    id: *id,
                    length: next.len(),
                }
            }
            Action::Read => Reply::Values(state.clone()),
        };
        output
            .is_none_or(|seen| *seen == LogReply::Reply(expected))
            .then_some(next)
    }
}

/// Translate a recorded `Worker` history; each operation is its own client.
fn translate(history: &[Bounded]) -> Vec<Operation<Action, LogReply>> {
    history
        .iter()
        .enumerate()
        .map(|(client, op)| match &op.outcome {
            Outcome::Known { finished, reply } => Operation::complete(
                client,
                op.action.clone(),
                op.invoked,
                LogReply::Reply(reply.clone()),
                *finished,
            ),
            Outcome::Rejected { finished } => Operation::complete(
                client,
                op.action.clone(),
                op.invoked,
                LogReply::Rejected,
                *finished,
            ),
            Outcome::Unknown => Operation::pending(client, op.action.clone(), op.invoked),
        })
        .collect()
}

fn verdict(history: &[Bounded]) -> Verdict {
    check(&AppendLog, &translate(history)).verdict()
}

fn tick(clock: &AtomicU64) -> u64 {
    clock.fetch_add(1, Ordering::SeqCst)
}
fn invoke(
    actor: &Handle,
    rt: &tokio::runtime::Runtime,
    clock: &AtomicU64,
    action: Action,
) -> Bounded {
    let invoked = tick(clock);
    let command = match action {
        Action::Append(id) => Command::append(id),
        Action::Read => Command::Read,
    };
    let outcome = match actor.raw(command) {
        Err(_) => Outcome::Rejected {
            finished: tick(clock),
        },
        Ok(receiver) => match receiver.wait(rt, LIMIT) {
            Ok(reply) => Outcome::Known {
                finished: tick(clock),
                reply,
            },
            Err(_) => Outcome::Unknown,
        },
    };
    Bounded {
        invoked,
        action,
        outcome,
    }
}

#[test]
fn checker_rejects_wrong_replies_duplicate_dispatch_and_lost_acknowledged_effects() {
    let valid = vec![
        Bounded {
            invoked: 0,
            action: Action::Append(7),
            outcome: Outcome::Known {
                finished: 1,
                reply: Reply::Appended { id: 7, length: 1 },
            },
        },
        Bounded {
            invoked: 2,
            action: Action::Read,
            outcome: Outcome::Known {
                finished: 3,
                reply: Reply::Values(vec![7]),
            },
        },
    ];
    let both = |history: &[Bounded], expected: bool| {
        assert_eq!(bounded::check(history), Ok(expected));
        let expected = if expected {
            Verdict::Linearizable
        } else {
            Verdict::Illegal
        };
        assert_eq!(verdict(history), expected);
    };
    both(&valid, true);
    for values in [vec![], vec![7, 7], vec![8]] {
        let mut broken = valid.clone();
        broken[1].outcome = Outcome::Known {
            finished: 3,
            reply: Reply::Values(values),
        };
        both(&broken, false);
    }
    let mut wrong = valid.clone();
    wrong[0].outcome = Outcome::Known {
        finished: 1,
        reply: Reply::Appended { id: 8, length: 1 },
    };
    both(&wrong, false);
    let mut unknown = valid.clone();
    unknown[0].outcome = Outcome::Unknown;
    both(&unknown, true);
    unknown[0].outcome = Outcome::Rejected { finished: 1 };
    both(&unknown, false);
}

// Not simulated: mixes Thread and Blocking backends on OS threads.
#[cfg(not(spawned_sim))]
#[test]
fn concurrent_histories_have_a_legal_sequential_explanation() {
    let cases: usize = std::env::var("SPAWNED_HISTORY_CASES")
        .unwrap_or_else(|_| "8".into())
        .parse()
        .unwrap();
    assert!((1..=1024).contains(&cases));
    let seed: u64 = std::env::var("SPAWNED_HISTORY_SEED")
        .unwrap_or_else(|_| "20261106".into())
        .parse()
        .unwrap();
    const CLIENTS: u64 = 6;
    const ROUNDS: u64 = 4;
    let rt = Arc::new(runtime());
    let _entered = rt.enter();
    let mut longest = 0;
    for &backend in BACKENDS {
        for case in 0..cases {
            let mut cleanup = Cleanup::default();
            let actor = Handle::start(backend, Worker::default());
            cleanup.track(&actor);
            let clock = Arc::new(AtomicU64::new(0));
            let operations = Arc::new(Mutex::new(Vec::new()));
            // Demonstrate an effect survives a timed-out reply wait.
            let (after, mut control) = gate();
            let invoked = tick(&clock);
            let reply = actor
                .raw(Command::Append {
                    id: 0,
                    before: None,
                    after: Some(after),
                    panic_after: false,
                })
                .unwrap();
            control.entered();
            assert!(matches!(
                reply.wait(&rt, Duration::ZERO),
                Err(ActorError::RequestTimeout)
            ));
            operations.lock().unwrap().push(Bounded {
                invoked,
                action: Action::Append(0),
                outcome: Outcome::Unknown,
            });
            control.release();
            operations
                .lock()
                .unwrap()
                .push(invoke(&actor, &rt, &clock, Action::Read));
            std::thread::scope(|scope| {
                for client in 0..CLIENTS {
                    let (actor, rt, clock, operations) =
                        (actor.clone(), rt.clone(), clock.clone(), operations.clone());
                    scope.spawn(move || {
                        for round in 0..ROUNDS {
                            let id = 1 + client * ROUNDS + round;
                            if (seed ^ case as u64 ^ id).is_multiple_of(2) {
                                std::thread::yield_now();
                            }
                            let append = invoke(&actor, &rt, &clock, Action::Append(id));
                            operations.lock().unwrap().push(append);
                            let read = invoke(&actor, &rt, &clock, Action::Read);
                            operations.lock().unwrap().push(read);
                        }
                    });
                }
            });
            operations
                .lock()
                .unwrap()
                .push(invoke(&actor, &rt, &clock, Action::Read));
            actor.stop();
            assert_eq!(actor.wait(&rt), ExitReason::Normal);
            operations
                .lock()
                .unwrap()
                .push(invoke(&actor, &rt, &clock, Action::Append(99)));
            let mut history = operations.lock().unwrap().clone();
            history.sort_by_key(|op| op.invoked);
            longest = longest.max(history.len());
            let translated = translate(&history);
            let report = check(&AppendLog, &translated);
            assert_eq!(
                report.verdict(),
                Verdict::Linearizable,
                "seed={seed} case={case} backend={backend:?}\n{}",
                report.explain(&AppendLog, &translated)
            );
            // The oracle must also reject corruption of an actual observed history.
            let read = history
                .iter_mut()
                .rev()
                .find(|op| matches!(op.action, Action::Read))
                .unwrap();
            if let Outcome::Known {
                reply: Reply::Values(values),
                ..
            } = &mut read.outcome
            {
                values.push(999);
            }
            assert_eq!(verdict(&history), Verdict::Illegal);
        }
    }
    println!(
        "checked {} histories of up to {longest} operations across {} backends; input seed={seed}; OS schedules are not replayed",
        cases * BACKENDS.len(),
        BACKENDS.len()
    );
}

/// Worker histories of up to 16 operations: sequential executions stretched so
/// neighbours overlap, with lost replies, rejections and sometimes one
/// corrupted reply.
fn bounded_history() -> impl Strategy<Value = Vec<Bounded>> {
    let step = (any::<bool>(), 0..5u64, 0..5u64, 0..12u8, any::<bool>());
    (
        prop::collection::vec(step, 0..=16),
        prop::option::of((any::<prop::sample::Index>(), 0..3u8)),
    )
        .prop_map(|(steps, corrupt)| {
            let mut log = Vec::new();
            let mut history: Vec<Bounded> = steps
                .into_iter()
                .enumerate()
                .map(|(i, (append, before, after, kind, effect))| {
                    let point = 10 + 4 * i as u64;
                    let action = if append {
                        Action::Append(i as u64)
                    } else {
                        Action::Read
                    };
                    let (invoked, finished) = (point - before, point + after);
                    let outcome = match kind {
                        0 => Outcome::Rejected { finished },
                        1 => {
                            if effect && append {
                                log.push(i as u64);
                            }
                            Outcome::Unknown
                        }
                        _ => {
                            let reply = if append {
                                log.push(i as u64);
                                Reply::Appended {
                                    id: i as u64,
                                    length: log.len(),
                                }
                            } else {
                                Reply::Values(log.clone())
                            };
                            Outcome::Known { finished, reply }
                        }
                    };
                    Bounded {
                        invoked,
                        action,
                        outcome,
                    }
                })
                .collect();
            if let (Some((index, how)), false) = (corrupt, history.is_empty()) {
                let len = history.len();
                match &mut history[index.index(len)].outcome {
                    Outcome::Known {
                        reply: Reply::Values(values),
                        ..
                    } => match how {
                        0 => values.push(1_000),
                        1 => {
                            values.pop();
                        }
                        _ => values.reverse(),
                    },
                    Outcome::Known {
                        reply: Reply::Appended { length, .. },
                        ..
                    } => *length += 1,
                    outcome @ Outcome::Unknown => {
                        *outcome = Outcome::Rejected {
                            finished: u64::MAX / 2,
                        }
                    }
                    Outcome::Rejected { .. } => {}
                }
            }
            history
        })
}

#[test]
fn new_checker_agrees_with_the_bounded_oracle_wherever_it_decides() {
    let mut runner = TestRunner::deterministic();
    let strategy = bounded_history();
    let (mut legal, mut illegal, mut undecided) = (0, 0, 0);
    for _ in 0..4_000 {
        let history = strategy.new_tree(&mut runner).unwrap().current();
        match bounded::check(&history) {
            Ok(expected) => {
                let wanted = if expected {
                    legal += 1;
                    Verdict::Linearizable
                } else {
                    illegal += 1;
                    Verdict::Illegal
                };
                assert_eq!(verdict(&history), wanted, "{history:?}");
            }
            Err(_) => undecided += 1,
        }
    }
    println!(
        "agreement: {legal} linearizable, {illegal} illegal, {undecided} beyond the bounded oracle"
    );
    assert!(
        legal > 500 && illegal > 500,
        "{legal} legal / {illegal} illegal"
    );
}

// ------------------------------------------------- the key-value store actor

/// One generated request; `short` waits zero time for the reply, so the
/// request is recorded as pending although the actor still runs it.
#[derive(Clone, Copy, Debug)]
struct Cmd {
    op: StoreOp,
    short: bool,
}

fn cmd() -> impl Strategy<Value = Cmd> {
    let op = prop_oneof![
        3 => (0..4u8).prop_map(|key| StoreOp::Put { key, value: 0 }),
        3 => (0..4u8).prop_map(|key| StoreOp::Get { key }),
        2 => (0..2u8).prop_map(|key| StoreOp::Push { key, value: 0 }),
        2 => (0..2u8).prop_map(|key| StoreOp::Pop { key }),
    ];
    (op, prop::bool::weighted(0.03)).prop_map(|(op, short)| Cmd { op, short })
}

/// PropEr's `parallel_commands`: a sequential prefix, then 2 to 4 branches
/// run concurrently. Written values are unique so every read is informative.
type Program = (Vec<Cmd>, Vec<Vec<Cmd>>);

fn parallel_commands() -> impl Strategy<Value = Program> {
    (
        prop::collection::vec(cmd(), 0..20),
        prop::collection::vec(prop::collection::vec(cmd(), 25..250), 2..=4),
    )
        .prop_map(|(mut prefix, mut branches)| {
            let mut next = 1u64;
            for cmd in prefix.iter_mut().chain(branches.iter_mut().flatten()) {
                if let StoreOp::Put { value, .. } | StoreOp::Push { value, .. } = &mut cmd.op {
                    *value = next;
                    next += 1;
                }
            }
            (prefix, branches)
        })
}

#[derive(Clone, Copy, Debug)]
enum Mode {
    #[cfg(feature = "async")]
    Tasks(spawned_concurrency::tasks::Backend),
    Threads,
}

const MODES: &[Mode] = &[
    #[cfg(feature = "async")]
    Mode::Tasks(spawned_concurrency::tasks::Backend::Async),
    #[cfg(feature = "async")]
    Mode::Tasks(spawned_concurrency::tasks::Backend::Thread),
    Mode::Threads,
];

type StoreHistory = Vec<Operation<StoreOp, Option<u64>>>;

fn wait_for(cmd: Cmd) -> Duration {
    if cmd.short {
        Duration::ZERO
    } else {
        LIMIT
    }
}

/// Run `program` against a fresh store and return the recorded history.
fn run(rt: &tokio::runtime::Runtime, mode: Mode, bug: Bug, program: &Program) -> StoreHistory {
    let (prefix, branches) = program;
    let recorder = Recorder::new();
    match mode {
        #[cfg(feature = "async")]
        Mode::Tasks(backend) => rt.block_on(async {
            use spawned_concurrency::tasks::ActorStart as _;
            async fn issue(
                actor: &spawned_concurrency::tasks::ActorRef<store::AsyncStore>,
                recorder: &Recorder<StoreOp, Option<u64>>,
                client: usize,
                cmd: Cmd,
            ) {
                let call = recorder.call(client, cmd.op);
                if let Ok(reply) = actor.request_with_timeout(cmd.op, wait_for(cmd)).await {
                    call.complete(reply);
                }
            }
            let actor = store::AsyncStore(Store::new(bug)).start_with_backend(backend);
            for &cmd in prefix {
                issue(&actor, &recorder, 0, cmd).await;
            }
            let tasks: Vec<_> = branches
                .iter()
                .cloned()
                .enumerate()
                .map(|(branch, cmds)| {
                    let (actor, recorder) = (actor.clone(), recorder.clone());
                    tokio::spawn(async move {
                        for cmd in cmds {
                            issue(&actor, &recorder, branch + 1, cmd).await;
                        }
                    })
                })
                .collect();
            for task in tasks {
                task.await.unwrap();
            }
            actor.child_handle().stop();
            actor.join().await;
        }),
        Mode::Threads => {
            let _ = rt;
            use spawned_concurrency::threads::ActorStart as _;
            fn issue(
                actor: &spawned_concurrency::threads::ActorRef<NativeStore>,
                recorder: &Recorder<StoreOp, Option<u64>>,
                client: usize,
                cmd: Cmd,
            ) {
                let call = recorder.call(client, cmd.op);
                if let Ok(reply) = actor.request_with_timeout(cmd.op, wait_for(cmd)) {
                    call.complete(reply);
                }
            }
            let actor = NativeStore(Store::new(bug)).start();
            for &cmd in prefix {
                issue(&actor, &recorder, 0, cmd);
            }
            std::thread::scope(|scope| {
                for (branch, cmds) in branches.iter().enumerate() {
                    let (actor, recorder) = (&actor, &recorder);
                    scope.spawn(move || {
                        for &cmd in cmds {
                            issue(actor, recorder, branch + 1, cmd);
                        }
                    });
                }
            });
            actor.child_handle().stop();
            actor.join();
        }
    }
    recorder.take()
}

fn store_cases() -> u32 {
    std::env::var("SPAWNED_LINEARIZABLE_CASES")
        .unwrap_or_else(|_| "16".into())
        .parse()
        .unwrap()
}

// Not simulated: drives Task, Thread and native stores from a multi-thread Tokio runtime.
#[cfg(not(spawned_sim))]
#[test]
fn parallel_command_histories_against_a_real_actor_are_linearizable() {
    let rt = runtime();
    let longest = AtomicUsize::new(0);
    let pending = AtomicUsize::new(0);
    for &mode in MODES {
        let mut runner = TestRunner::new(Config {
            cases: store_cases(),
            failure_persistence: None,
            ..Config::default()
        });
        runner
            .run(&parallel_commands(), |program| {
                let history = run(&rt, mode, Bug::None, &program);
                longest.fetch_max(history.len(), Ordering::Relaxed);
                pending.fetch_add(
                    history.iter().filter(|op| op.returned.is_none()).count(),
                    Ordering::Relaxed,
                );
                let report = check(&StoreModel, &history);
                prop_assert!(
                    report.is_linearizable(),
                    "{mode:?}\n{}",
                    report.explain(&StoreModel, &history)
                );
                Ok(())
            })
            .unwrap();
    }
    println!(
        "{} cases x {} modes; longest history {} operations; {} pending requests",
        store_cases(),
        MODES.len(),
        longest.into_inner(),
        pending.into_inner()
    );
}

/// A sequential prefix that trips every injected bug, followed by generated
/// parallel branches.
fn tripwire_program(seed: u64) -> Program {
    let mut prefix = Vec::new();
    let mut value = 1_000_000;
    let cmd = |op| Cmd { op, short: false };
    for _ in 0..6 {
        value += 1;
        prefix.push(cmd(StoreOp::Put { key: 0, value }));
        prefix.push(cmd(StoreOp::Get { key: 0 }));
    }
    for _ in 0..4 {
        value += 1;
        prefix.push(cmd(StoreOp::Push { key: 0, value }));
    }
    for _ in 0..4 {
        prefix.push(cmd(StoreOp::Pop { key: 0 }));
    }
    let mut runner = TestRunner::new_with_rng(
        Config::default(),
        proptest::test_runner::TestRng::from_seed(
            proptest::test_runner::RngAlgorithm::ChaCha,
            &[seed as u8; 32],
        ),
    );
    let (_, branches) = parallel_commands().new_tree(&mut runner).unwrap().current();
    (prefix, branches)
}

// Not simulated: drives Task, Thread and native stores from a multi-thread Tokio runtime.
#[cfg(not(spawned_sim))]
#[test]
fn checker_rejects_buggy_store_actors() {
    let rt = runtime();
    for &mode in MODES {
        for seed in 0..2 {
            let program = tripwire_program(seed);
            let history = run(&rt, mode, Bug::None, &program);
            assert!(check(&StoreModel, &history).is_linearizable());
            for bug in [Bug::LostWrite, Bug::StaleRead, Bug::Reorder] {
                let history = run(&rt, mode, bug, &program);
                let report = check(&StoreModel, &history);
                assert_eq!(
                    report.verdict(),
                    Verdict::Illegal,
                    "{mode:?} {bug:?} not caught"
                );
                if seed == 0 && matches!(mode, Mode::Threads) {
                    println!("{bug:?}:\n{}", report.explain(&StoreModel, &history));
                    if let Some(dir) = std::env::var_os("SPAWNED_LINEARIZABILITY_DIR") {
                        let path = std::path::Path::new(&dir).join(format!("{bug:?}.html"));
                        std::fs::create_dir_all(&dir).unwrap();
                        std::fs::write(&path, report.to_html(&StoreModel, &history)).unwrap();
                        println!("timeline: {}", path.display());
                    }
                }
            }
        }
    }
}

// Not simulated: drives Task, Thread and native stores from a multi-thread Tokio runtime.
#[cfg(not(spawned_sim))]
#[test]
fn bugs_are_caught_by_parallel_branches_alone() {
    // No tripwire prefix: the generated concurrent traffic must expose each
    // bug on its own in at least one of a few runs.
    let rt = runtime();
    let mode = *MODES.last().unwrap();
    for bug in [Bug::LostWrite, Bug::StaleRead, Bug::Reorder] {
        let caught = (0..4)
            .filter(|&seed| {
                let (_, branches) = tripwire_program(seed);
                let history = run(&rt, mode, bug, &(Vec::new(), branches));
                check(&StoreModel, &history).verdict() == Verdict::Illegal
            })
            .count();
        assert!(caught > 0, "{bug:?} escaped every parallel-only run");
        println!("{bug:?}: caught in {caught}/4 parallel-only runs");
    }
    let _ = KeyState::default();
}
