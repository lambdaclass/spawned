//! Porcupine-style linearizability checking for recorded request histories.
//!
//! Record each request as an [`Operation`]: when it was invoked, and when and
//! with what it returned. A history is *linearizable* for a sequential
//! [`Model`] when every operation can be given a single point between its call
//! and its return such that, replayed in that order, the model accepts every
//! output. This is the contract an actor gives its concurrent callers: the
//! mailbox serialises requests, so any observed history must have such an
//! explanation.
//!
//! The search is the one [Porcupine](https://github.com/anishathalye/porcupine)
//! uses: Wing and Gong's backtracking over a linked list of call and return
//! entries, with Lowe's memoisation of `(linearized set, model state)` pairs,
//! and P-compositionality ([`Model::partition`]): a history over independent
//! keys is linearizable exactly when each per-key sub-history is, so each
//! partition is checked on its own. Thousands of operations split into
//! partitions of a few hundred check in milliseconds.
//!
//! **Pending operations.** A request whose reply never arrived (timed out,
//! cancelled, the caller crashed) is recorded with [`Operation::pending`]. It
//! may or may not have taken effect: the checker may linearize it anywhere
//! after its call (passing `None` as the output to [`Model::step`]) or leave it
//! out. A request that provably had no effect (the send was rejected) is a
//! completed operation whose output says so; model it as a no-op.
//!
//! **Budget.** The problem is NP-complete. [`CheckOptions`] bounds the time and
//! the number of memoised states; when the budget runs out the verdict is
//! [`Verdict::Unknown`], never a pass.
//!
//! ```
//! use spawned_test::linearizability::{check, Model, Operation, Verdict};
//!
//! /// A register: `Some(v)` writes `v`, `None` reads.
//! struct Register;
//! impl Model for Register {
//!     type State = u64;
//!     type Input = Option<u64>;
//!     type Output = u64;
//!     fn init(&self) -> u64 { 0 }
//!     fn step(&self, state: &u64, input: &Option<u64>, output: Option<&u64>) -> Option<u64> {
//!         match input {
//!             Some(value) => Some(*value),
//!             None => (output.is_none() || output == Some(state)).then_some(*state),
//!         }
//!     }
//! }
//!
//! // Client 0 writes 1 during [0, 3]; client 1 reads during [1, 2] and [4, 5].
//! let history = vec![
//!     Operation::complete(0, Some(1), 0, 0, 3),
//!     Operation::complete(1, None, 1, 1, 2), // may see the overlapping write...
//!     Operation::complete(1, None, 4, 0, 5), // ...but then cannot lose it.
//! ];
//! let report = check(&Register, &history);
//! assert_eq!(report.verdict(), Verdict::Illegal);
//! println!("{}", report.explain(&Register, &history));
//! ```
mod recorder;
mod render;
mod search;

pub use recorder::{Call, Recorder};

use std::collections::HashMap;
use std::fmt::Debug;
use std::hash::Hash;
use std::time::{Duration, Instant};

/// A sequential specification of the object under test.
pub trait Model {
    /// The model state. Hashed and compared to memoise visited states.
    type State: Clone + Eq + Hash + Debug;
    /// The request.
    type Input: Debug;
    /// The reply.
    type Output: Debug;

    /// The state before any operation.
    fn init(&self) -> Self::State;

    /// Apply `input` to `state` and return the new state if the model can
    /// produce `output`, or `None` if `output` is impossible from `state`.
    ///
    /// `output` is `None` for a pending operation whose reply was never
    /// observed: return the state after the operation takes effect.
    fn step(
        &self,
        state: &Self::State,
        input: &Self::Input,
        output: Option<&Self::Output>,
    ) -> Option<Self::State>;

    /// Split a history into independent sub-histories (indices into
    /// `history`), checked separately. Only split when operations in different
    /// parts never observe each other, as with per-key operations on a map;
    /// see [`partition_by_key`]. Operations left out of every part are not
    /// checked. The default is one partition.
    fn partition(&self, history: &[Operation<Self::Input, Self::Output>]) -> Vec<Vec<usize>> {
        vec![(0..history.len()).collect()]
    }
}

/// Group operations by a key derived from their input, in first-seen order.
pub fn partition_by_key<I, O, K: Hash + Eq>(
    history: &[Operation<I, O>],
    key: impl Fn(&I) -> K,
) -> Vec<Vec<usize>> {
    let mut slots: HashMap<K, usize> = HashMap::new();
    let mut parts: Vec<Vec<usize>> = Vec::new();
    for (index, op) in history.iter().enumerate() {
        let slot = *slots.entry(key(&op.input)).or_insert_with(|| {
            parts.push(Vec::new());
            parts.len() - 1
        });
        parts[slot].push(index);
    }
    parts
}

/// One request in a history. Times are any monotonic clock shared by all
/// clients (a logical counter or nanoseconds); only their order matters. An
/// operation precedes another when it returned strictly before the other was
/// called; equal times count as concurrent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Operation<I, O> {
    /// The caller, used only to lay out reports.
    pub client: usize,
    /// The request.
    pub input: I,
    /// When the request was issued.
    pub call: u64,
    /// When the reply arrived and what it was; `None` while pending.
    pub returned: Option<(u64, O)>,
}

impl<I, O> Operation<I, O> {
    /// A request that returned `output` at `ret`.
    pub fn complete(client: usize, input: I, call: u64, output: O, ret: u64) -> Self {
        assert!(
            ret >= call,
            "operation returned at {ret} before its call at {call}"
        );
        Self {
            client,
            input,
            call,
            returned: Some((ret, output)),
        }
    }

    /// A request without an observed reply: it may or may not take effect.
    pub fn pending(client: usize, input: I, call: u64) -> Self {
        Self {
            client,
            input,
            call,
            returned: None,
        }
    }

    /// The reply, if one was observed.
    pub fn output(&self) -> Option<&O> {
        self.returned.as_ref().map(|(_, output)| output)
    }

    /// When the reply arrived, if it did.
    pub fn ret(&self) -> Option<u64> {
        self.returned.as_ref().map(|(at, _)| *at)
    }
}

/// Limits on the search. When one is reached the verdict is
/// [`Verdict::Unknown`].
#[derive(Clone, Debug)]
pub struct CheckOptions {
    /// Wall-clock limit for the whole history (all partitions).
    pub timeout: Option<Duration>,
    /// Maximum memoised `(linearized set, state)` pairs per partition.
    pub max_states: Option<usize>,
}

impl Default for CheckOptions {
    /// 60 seconds and 10 million states per partition.
    fn default() -> Self {
        Self {
            timeout: Some(Duration::from_secs(60)),
            max_states: Some(10_000_000),
        }
    }
}

/// The outcome of a check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Every partition has a legal linearization.
    Linearizable,
    /// Some partition has none; [`Report::explain`] says where it fails.
    Illegal,
    /// The budget ran out before a decision. Not a pass.
    Unknown,
}

/// Why a candidate operation could not be linearized next at the failure
/// point.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The model rejects its output in the state reached by the prefix.
    Rejected,
    /// The model accepts it, but every continuation from there was already
    /// explored and failed.
    DeadEnd,
}

/// The deepest point the search reached in a partition that is not
/// linearizable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    /// The longest linearizable prefix found, as indices into the history, in
    /// linearization order.
    pub prefix: Vec<usize>,
    /// The completed operation whose return was reached without it being
    /// linearizable after `prefix`.
    pub blocked: usize,
    /// Every operation that could have been linearized next, and why it was
    /// not.
    pub candidates: Vec<(usize, Refusal)>,
}

/// The result for one partition.
#[derive(Clone, Debug)]
pub struct PartitionReport {
    /// The partition's operations, as indices into the history.
    pub operations: Vec<usize>,
    /// The partition's verdict.
    pub verdict: Verdict,
    /// Memoised states visited.
    pub states: usize,
    /// Set when `verdict` is [`Verdict::Illegal`].
    pub failure: Option<Failure>,
}

/// The result of [`check`] or [`check_with`].
#[derive(Clone, Debug)]
pub struct Report {
    /// One entry per partition checked, in [`Model::partition`] order.
    /// Checking stops at the first illegal partition, so later ones are
    /// absent.
    pub partitions: Vec<PartitionReport>,
    /// Why the verdict is [`Verdict::Unknown`], if it is.
    pub unknown: Option<String>,
    /// Wall-clock time spent.
    pub elapsed: Duration,
}

impl Report {
    /// Illegal if any partition is, else Unknown if any is, else
    /// Linearizable.
    pub fn verdict(&self) -> Verdict {
        let verdicts = self.partitions.iter().map(|p| p.verdict);
        if verdicts.clone().any(|v| v == Verdict::Illegal) {
            Verdict::Illegal
        } else if self.unknown.is_some() || verdicts.clone().any(|v| v == Verdict::Unknown) {
            Verdict::Unknown
        } else {
            Verdict::Linearizable
        }
    }

    /// Whether the verdict is [`Verdict::Linearizable`].
    pub fn is_linearizable(&self) -> bool {
        self.verdict() == Verdict::Linearizable
    }

    /// Memoised states visited across all partitions.
    pub fn states(&self) -> usize {
        self.partitions.iter().map(|p| p.states).sum()
    }

    /// A readable account: per illegal partition, the longest linearizable
    /// prefix with the model state after each step, the operation that cannot
    /// be placed and why each candidate was refused.
    pub fn explain<M: Model>(
        &self,
        model: &M,
        history: &[Operation<M::Input, M::Output>],
    ) -> String {
        render::explain(self, model, history)
    }

    /// A self-contained HTML page with a Porcupine-style timeline of each
    /// illegal (or, if none, each) partition: one row per client, the
    /// linearized prefix numbered in order and the blocked operation marked.
    pub fn to_html<M: Model>(
        &self,
        model: &M,
        history: &[Operation<M::Input, M::Output>],
    ) -> String {
        render::html(self, model, history)
    }
}

/// Check `history` against `model` with [`CheckOptions::default`].
pub fn check<M: Model>(model: &M, history: &[Operation<M::Input, M::Output>]) -> Report {
    check_with(model, history, &CheckOptions::default())
}

/// Check `history` against `model` within `options`.
pub fn check_with<M: Model>(
    model: &M,
    history: &[Operation<M::Input, M::Output>],
    options: &CheckOptions,
) -> Report {
    let started = Instant::now();
    let deadline = options.timeout.map(|timeout| started + timeout);
    let mut partitions = Vec::new();
    let mut unknown = None;
    for operations in model.partition(history) {
        let outcome =
            search::check_partition(model, history, &operations, deadline, options.max_states);
        let verdict = outcome.verdict;
        if let Some(reason) = outcome.unknown {
            unknown.get_or_insert(reason);
        }
        partitions.push(PartitionReport {
            operations,
            verdict,
            states: outcome.states,
            failure: outcome.failure,
        });
        if verdict == Verdict::Illegal {
            break;
        }
    }
    Report {
        partitions,
        unknown,
        elapsed: started.elapsed(),
    }
}

/// Panic unless `history` is linearizable, with [`Report::explain`] as the
/// message. When the `SPAWNED_LINEARIZABILITY_DIR` environment variable names
/// a directory, an illegal history's [`Report::to_html`] timeline is also
/// written there and its path included in the message.
#[track_caller]
pub fn assert_linearizable<M: Model>(
    model: &M,
    history: &[Operation<M::Input, M::Output>],
) -> Report {
    let report = check(model, history);
    match report.verdict() {
        Verdict::Linearizable => report,
        Verdict::Unknown => panic!(
            "linearizability undecided for {} operations: {}",
            history.len(),
            report.unknown.as_deref().unwrap_or("budget exhausted")
        ),
        Verdict::Illegal => {
            let mut message = report.explain(model, history);
            if let Some(dir) = std::env::var_os("SPAWNED_LINEARIZABILITY_DIR") {
                let dir = std::path::PathBuf::from(dir);
                let path = dir.join(format!(
                    "history-{}-{}.html",
                    std::process::id(),
                    started_nanos()
                ));
                let written = std::fs::create_dir_all(&dir)
                    .and_then(|_| std::fs::write(&path, report.to_html(model, history)));
                match written {
                    Ok(()) => message.push_str(&format!("\ntimeline: {}\n", path.display())),
                    Err(error) => message.push_str(&format!("\ntimeline not written: {error}\n")),
                }
            }
            panic!("{message}")
        }
    }
}

fn started_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default()
}
