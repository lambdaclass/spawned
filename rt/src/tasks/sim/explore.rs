//! Systematic interleaving exploration for the simulator.
//!
//! The simulator makes one choice per poll: which ready task runs next. The
//! default answers it with a seeded random walk. This module adds two
//! systematic strategies:
//!
//! - **PCT** (probabilistic concurrency testing, Burckhardt et al., ASPLOS
//!   2010), with a poll as the step. Every task gets a random priority when
//!   it first becomes ready (drawn in task-id order) and the highest-priority
//!   ready task runs each poll. `d - 1` change points `k_1..k_{d-1}` are
//!   drawn uniformly (and distinct) from `0..k`; the task that runs poll
//!   `k_i` then drops to priority `i`, below every initial priority. As in
//!   the paper, `k_i` is associated with priority `i` independently of where
//!   it falls in time. A unit test checks the scheduler against an
//!   independent implementation of the paper's algorithm, including tasks
//!   that block and are re-enabled. **No probability bound is claimed:** the
//!   paper's `1 / (n * k^(d-1))` needs `k` to bound the steps of every
//!   execution and a bug of depth `d` in the paper's sense. Here `k` is an
//!   estimate (the given `k`, or the longest run seen so far, which early
//!   runs can underestimate), polls after the fairness cutoff are not PCT
//!   steps, and a poll is coarser than the paper's instruction step. Treat
//!   PCT as a well-founded heuristic and measure it. That a strategy with
//!   `d = 2` or one DFS delay finds a bug does not establish the bug's depth.
//! - **Bounded DFS over delays.** The same priority scheduler made
//!   deterministic: priorities start in task-id order, so with no delays it
//!   is the canonical schedule (always the smallest ready id). A *delay* at a
//!   poll demotes the task about to run below every other task, for good
//!   (delay bounding, Emmi, Qadeer and Rakamarić, POPL 2011, with a
//!   persistent demotion as in PCT). The search enumerates, depth first and
//!   statelessly (each schedule is a replayed prefix of delay counts), every
//!   schedule with at most `bound` delays, and reports whether it exhausted
//!   that space. The space is truncated: polls past `depth` and past the
//!   fairness cutoff are not choice points, so "exhausted" means every
//!   schedule within the bound whose alternatives all fall before those
//!   cutoffs; [`Report::truncated`] says whether any run had alternatives
//!   past them, and [`Report::complete`] is exhaustion without truncation.
//!   There is no partial-order reduction: the executor sees wakers, not which
//!   shared state a poll touches.
//!
//! Both strategies are deterministic per run and record every pick into the
//! run's [`Tape`](super::Tape), so a failure they find replays with
//! `SPAWNED_SIM_TAPE` and shrinks with [`shrink`](super::shrink) like a
//! random-walk failure.
//!
//! **Fairness.** A priority scheduler starves a task that spins
//! (`while !flag { yield_now().await }`) waiting on a lower-priority one. As
//! in Coyote, after [`DEFAULT_FAIR_AFTER`] polls (`SPAWNED_SIM_FAIR_AFTER`)
//! a run switches to a fair rule, the least recently polled ready task, and
//! those later polls are not choice points. The cutoff can also be set per
//! strategy (`pct:fair=N`, `dfs:fair=N`), and is part of the strategy a
//! failure reports.
//!
//! **Messages.** Delivery order needs no separate choice point: a `send`
//! enqueues into the mailbox inside the sender's poll, so the order in which
//! messages from different senders reach a mailbox is exactly the order in
//! which the senders' polls run, which these strategies already choose.
//! Messages from one sender stay FIFO. A choice at the mailbox would only add
//! reorderings that cannot happen in process (a message overtaking one sent
//! causally before it).

use super::{quietly, run_planned, splitmix, Outcome};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::future::Future;

/// Polls after which a PCT or DFS run switches to a fair scheduler.
pub const DEFAULT_FAIR_AFTER: u64 = 20_000;
/// PCT's run-length estimate `k` before any run has been measured.
pub const DEFAULT_PCT_STEPS: u64 = 100;

/// How the next ready task is chosen across the runs of one exploration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Strategy {
    /// Seeded uniform random walk, one seed per run (the default scheduler).
    Random,
    /// PCT with `depth - 1` change points over a run length estimate `steps`
    /// (polls); `None` estimates it as the longest run so far. `fair_after`
    /// overrides `SPAWNED_SIM_FAIR_AFTER`.
    Pct {
        depth: u32,
        steps: Option<u64>,
        fair_after: Option<u64>,
    },
    /// Depth-first enumeration of every schedule with at most `bound` delays;
    /// with `depth`, only the first `depth` polls of a run are choice points.
    /// `fair_after` overrides `SPAWNED_SIM_FAIR_AFTER`.
    Dfs {
        bound: u32,
        depth: Option<u64>,
        fair_after: Option<u64>,
    },
}

impl Strategy {
    /// Parse `random`, `pct[:d=3][,k=200][,fair=N]` or
    /// `dfs[:bound=2][,depth=500][,fair=N]`.
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        let (name, args) = text.split_once(':').unwrap_or((text, ""));
        let mut options = BTreeMap::new();
        for arg in args.split(',').map(str::trim).filter(|a| !a.is_empty()) {
            let (key, value) = arg
                .split_once('=')
                .ok_or_else(|| format!("strategy {text:?}: expected key=value, got {arg:?}"))?;
            let value: u64 = value
                .trim()
                .parse()
                .map_err(|e| format!("strategy {text:?}: {key}: {e}"))?;
            options.insert(key.trim().to_string(), value);
        }
        let mut take = |keys: &[&str]| keys.iter().find_map(|k| options.remove(*k));
        let strategy = match name {
            "random" => Strategy::Random,
            "pct" => Strategy::Pct {
                depth: take(&["d", "depth"]).unwrap_or(3).max(1) as u32,
                steps: take(&["k", "steps"]),
                fair_after: take(&["fair", "fair_after"]),
            },
            "dfs" => Strategy::Dfs {
                bound: take(&["bound", "b", "delays"]).unwrap_or(2) as u32,
                depth: take(&["depth"]),
                fair_after: take(&["fair", "fair_after"]),
            },
            _ => return Err(format!("unknown strategy {name:?} (random, pct, dfs)")),
        };
        match options.keys().next() {
            Some(key) => Err(format!("strategy {text:?}: unknown option {key:?}")),
            None => Ok(strategy),
        }
    }
}

impl fmt::Display for Strategy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Strategy::Random => f.write_str("random"),
            Strategy::Pct {
                depth,
                steps,
                fair_after,
            } => {
                write!(f, "pct:d={depth}")?;
                if let Some(k) = steps {
                    write!(f, ",k={k}")?;
                }
                if let Some(fair) = fair_after {
                    write!(f, ",fair={fair}")?;
                }
                Ok(())
            }
            Strategy::Dfs {
                bound,
                depth,
                fair_after,
            } => {
                write!(f, "dfs:bound={bound}")?;
                if let Some(depth) = depth {
                    write!(f, ",depth={depth}")?;
                }
                if let Some(fair) = fair_after {
                    write!(f, ",fair={fair}")?;
                }
                Ok(())
            }
        }
    }
}

impl Strategy {
    /// The fairness cutoff this strategy runs with: its own, else
    /// `SPAWNED_SIM_FAIR_AFTER`, else [`DEFAULT_FAIR_AFTER`].
    pub fn fair_after(&self) -> u64 {
        match self {
            Strategy::Pct {
                fair_after: Some(fair),
                ..
            }
            | Strategy::Dfs {
                fair_after: Some(fair),
                ..
            } => *fair,
            _ => default_fair_after(),
        }
    }

    /// This strategy with every setting that a run depends on filled in, so
    /// its `Display` form reproduces the run.
    fn resolved(&self, estimate: u64) -> Strategy {
        let fair_after = Some(self.fair_after());
        match *self {
            Strategy::Random => Strategy::Random,
            Strategy::Pct { depth, .. } => Strategy::Pct {
                depth,
                steps: Some(estimate),
                fair_after,
            },
            Strategy::Dfs { bound, depth, .. } => Strategy::Dfs {
                bound,
                depth,
                fair_after,
            },
        }
    }
}

/// One poll of a PCT or DFS run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Choice {
    /// Ready tasks at this poll.
    pub ready: u32,
    /// Delays (DFS: demotions before picking) or change points (PCT: the
    /// picked task's demotion after this step) applied at this poll.
    pub delays: u32,
    /// Whether this poll was a choice point (false past the strategy's depth
    /// or fairness cutoff, where alternatives are not explored).
    pub choice: bool,
    /// Hash of the sorted ready task ids, to check that a replayed prefix
    /// met the same ready sets.
    pub set: u64,
}

fn set_hash(ready: &[u64]) -> u64 {
    let mut ids = ready.to_vec();
    ids.sort_unstable();
    ids.iter().fold(0xcbf2_9ce4_8422_2325, |h, id| {
        (h ^ id).wrapping_mul(0x0100_0000_01b3)
    })
}

#[derive(Debug)]
enum Kind {
    /// `change_points[i]` is the step after which the task that ran it drops
    /// to priority `i + 1` (kept in sampling order, not sorted by time).
    Pct {
        change_points: Vec<u64>,
    },
    Delays {
        prefix: Vec<u32>,
        depth: u64,
    },
}

/// The per-run priority scheduler behind PCT and DFS.
#[derive(Debug)]
pub(super) struct Plan {
    kind: Kind,
    rng: u64,
    /// Higher runs first; ties go to the smaller task id.
    priority: BTreeMap<u64, i64>,
    last_polled: BTreeMap<u64, u64>,
    /// Polls so far (PCT's steps; every poll before the fairness cutoff).
    step: u64,
    fair_after: u64,
    demotions: i64,
    choices: Vec<Choice>,
}

fn default_fair_after() -> u64 {
    std::env::var("SPAWNED_SIM_FAIR_AFTER")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(DEFAULT_FAIR_AFTER)
}

impl Plan {
    fn new(kind: Kind, rng: u64, fair_after: u64) -> Self {
        Self {
            kind,
            rng,
            priority: BTreeMap::new(),
            last_polled: BTreeMap::new(),
            step: 0,
            fair_after,
            demotions: 0,
            choices: Vec::new(),
        }
    }

    /// PCT for one run: `depth - 1` distinct change points in `0..steps`,
    /// kept in the order drawn: the `i`-th drawn gets priority `i + 1`.
    fn pct(seed: u64, depth: u32, steps: u64, fair_after: u64) -> Self {
        // Separate from the seeded random walk's stream.
        let mut rng = seed ^ 0x5043_545f_5043_545f;
        let mut points = Vec::new();
        let wanted = (depth.saturating_sub(1) as u64).min(steps.max(1));
        while (points.len() as u64) < wanted {
            let point = splitmix(&mut rng) % steps.max(1);
            if !points.contains(&point) {
                points.push(point);
            }
        }
        Self::new(
            Kind::Pct {
                change_points: points,
            },
            rng,
            fair_after,
        )
    }

    /// Delay-bounded run: `prefix[i]` delays at poll `i`, none afterwards.
    fn delays(prefix: Vec<u32>, depth: u64, fair_after: u64) -> Self {
        Self::new(Kind::Delays { prefix, depth }, 0, fair_after)
    }

    /// Ready-list indices, highest priority first.
    fn order(&self, ready: &[u64]) -> Vec<usize> {
        let mut order: Vec<usize> = (0..ready.len()).collect();
        order.sort_by_key(|&i| (std::cmp::Reverse(self.priority[&ready[i]]), ready[i]));
        order
    }

    pub(super) fn pick(&mut self, ready: &[u64]) -> usize {
        let step = self.step;
        self.step += 1;
        // Tasks seen for the first time get their priorities in id order, so
        // the draws do not depend on the order their wakers fired.
        let mut fresh: Vec<u64> = ready
            .iter()
            .copied()
            .filter(|id| !self.priority.contains_key(id))
            .collect();
        fresh.sort_unstable();
        for id in fresh {
            let priority = match &self.kind {
                // Initial PCT priorities are all above the d - 1 change
                // priorities 1..d.
                Kind::Pct { change_points } => {
                    change_points.len() as i64 + 1 + (splitmix(&mut self.rng) >> 33) as i64
                }
                Kind::Delays { .. } => 0,
            };
            self.priority.insert(id, priority);
        }
        let set = set_hash(ready);
        let (index, delays, choice) = if step >= self.fair_after {
            let index = (0..ready.len())
                .min_by_key(|&i| (self.last_polled.get(&ready[i]).copied(), ready[i]))
                .unwrap_or(0);
            (index, 0, false)
        } else {
            match &self.kind {
                Kind::Pct { change_points } => {
                    // The highest-priority ready task runs this step. If the
                    // step is change point k_i (the i-th drawn, 1-based), that
                    // task then drops to priority i, below every initial
                    // priority; whether it ranks above another demoted task
                    // depends on i, not on which change came first
                    // (Burckhardt et al. 2010, section 2.5).
                    let index = self.order(ready)[0];
                    let mut delays = 0;
                    if let Some(i) = change_points.iter().position(|p| *p == step) {
                        self.priority.insert(ready[index], i as i64 + 1);
                        delays = 1;
                    }
                    (index, delays, true)
                }
                Kind::Delays { prefix, depth } => {
                    let choice = step < *depth;
                    let wanted = if choice {
                        prefix.get(step as usize).copied().unwrap_or(0)
                    } else {
                        0
                    };
                    let wanted = wanted.min(ready.len() as u32 - 1);
                    let mut order = self.order(ready);
                    for _ in 0..wanted {
                        // Demote below everything, including earlier delays.
                        self.demotions += 1;
                        self.priority.insert(ready[order[0]], -self.demotions);
                        order = self.order(ready);
                    }
                    (order[0], wanted, choice)
                }
            }
        };
        self.choices.push(Choice {
            ready: ready.len() as u32,
            delays,
            choice,
            set,
        });
        self.last_polled.insert(ready[index], step);
        index
    }
}

/// A failing run found by [`explore`].
#[derive(Debug, Clone)]
pub struct Found {
    /// 1-based index of the failing run.
    pub run: usize,
    /// The seed passed to the body (and, for random and PCT, the run's seed).
    pub seed: u64,
    /// The strategy that reproduces the run from `seed` alone (PCT with its
    /// `k` and fairness cutoff filled in); for DFS, replay the tape.
    pub strategy: Strategy,
    /// DFS only: delays per poll that reproduce the run.
    pub delays: Option<Vec<u32>>,
    pub outcome: Outcome,
}

/// Result of [`explore`] or [`sweep`].
#[derive(Debug, Clone)]
pub struct Report {
    pub strategy: Strategy,
    /// Runs made, including failing ones.
    pub runs: usize,
    /// Failing runs ([`explore`] stops at the first).
    pub failures: usize,
    /// Distinct schedules (by schedule hash) among those runs.
    pub schedules: usize,
    /// Longest run, in polls (PCT's run-length estimate `k` when not given).
    pub max_polls: u64,
    /// DFS only: every schedule within the bound, with alternatives only
    /// before the depth and fairness cutoffs, was run.
    pub exhausted: bool,
    /// PCT and DFS: some run had more than one ready task at a poll past the
    /// depth or fairness cutoff, so alternatives there were not explored.
    pub truncated: bool,
    /// DFS only: runs whose replayed prefix did not apply (different ready
    /// sets or delays than the run that produced it); nonzero means the body
    /// is not deterministic under the simulator, and the search stopped there.
    pub divergences: usize,
    /// The first failing run.
    pub found: Option<Found>,
}

impl Report {
    /// DFS only: the bounded space was exhausted, nothing was cut off and no
    /// run diverged, so every schedule with at most `bound` delays ran.
    pub fn complete(&self) -> bool {
        self.exhausted && !self.truncated && self.divergences == 0
    }
}

/// Run `body` up to `runs` times under `strategy`, stopping at the first
/// failure. Random and PCT use seeds `first_seed..`; DFS passes `first_seed`
/// to every run (schedules vary, the body's input does not). Panic output is
/// suppressed; the failure is in the report.
pub fn explore<F, Fut>(strategy: &Strategy, first_seed: u64, runs: usize, body: &F) -> Report
where
    F: Fn(u64) -> Fut + Sync,
    Fut: Future<Output = ()>,
{
    run_strategy(strategy, first_seed, runs, true, body)
}

/// [`explore`] without stopping at the first failure, to measure how often a
/// strategy hits a bug ([`Report::failures`] out of [`Report::runs`]).
pub fn sweep<F, Fut>(strategy: &Strategy, first_seed: u64, runs: usize, body: &F) -> Report
where
    F: Fn(u64) -> Fut + Sync,
    Fut: Future<Output = ()>,
{
    run_strategy(strategy, first_seed, runs, false, body)
}

fn run_strategy<F, Fut>(
    strategy: &Strategy,
    first_seed: u64,
    runs: usize,
    stop: bool,
    body: &F,
) -> Report
where
    F: Fn(u64) -> Fut + Sync,
    Fut: Future<Output = ()>,
{
    let mut report = Report {
        strategy: strategy.clone(),
        runs: 0,
        failures: 0,
        schedules: 0,
        max_polls: 0,
        exhausted: false,
        truncated: false,
        divergences: 0,
        found: None,
    };
    let mut hashes = BTreeSet::new();
    let mut estimate = match strategy {
        Strategy::Pct { steps, .. } => steps.unwrap_or(DEFAULT_PCT_STEPS),
        _ => 0,
    };
    let mut prefix: Vec<u32> = Vec::new();
    // DFS: the choices of the run that produced `prefix`.
    let mut parent: Vec<Choice> = Vec::new();
    while report.runs < runs {
        let seed = match strategy {
            Strategy::Dfs { .. } => first_seed,
            _ => first_seed.wrapping_add(report.runs as u64),
        };
        let fair_after = strategy.fair_after();
        let replayed = strategy.resolved(estimate);
        let plan = match strategy {
            Strategy::Random => None,
            Strategy::Pct { depth, .. } => Some(Plan::pct(seed, *depth, estimate, fair_after)),
            Strategy::Dfs { depth, .. } => Some(Plan::delays(
                prefix.clone(),
                depth.unwrap_or(u64::MAX),
                fair_after,
            )),
        };
        let (outcome, plan) = quietly(|| run_planned(seed, None, plan, body));
        report.runs += 1;
        hashes.insert(outcome.stats.schedule_hash);
        report.schedules = hashes.len();
        report.max_polls = report.max_polls.max(outcome.stats.steps);
        let choices = plan.map(|p| p.choices).unwrap_or_default();
        report.truncated |= choices.iter().any(|c| !c.choice && c.ready > 1);
        // A DFS run must replay its prefix: the same delays, at polls that
        // met the same ready sets as the run it came from.
        let diverged = choices.len() < prefix.len()
            || prefix.iter().enumerate().any(|(i, &delays)| {
                let (c, p) = (choices[i], parent[i]);
                c.delays != delays || c.ready != p.ready || c.set != p.set || c.choice != p.choice
            });
        if diverged {
            report.divergences += 1;
        }
        if outcome.failure.is_some() {
            report.failures += 1;
            if report.found.is_none() {
                report.found = Some(Found {
                    run: report.runs,
                    seed,
                    strategy: replayed,
                    delays: matches!(strategy, Strategy::Dfs { .. })
                        .then(|| choices.iter().map(|c| c.delays).collect()),
                    outcome,
                });
            }
            if stop {
                break;
            }
        }
        match strategy {
            Strategy::Random => {}
            Strategy::Pct { steps, .. } => {
                if steps.is_none() {
                    estimate = report.max_polls.max(1);
                }
            }
            // A diverged run says nothing reliable about the tree: stop.
            Strategy::Dfs { .. } if diverged => break,
            Strategy::Dfs { bound, .. } => match next_prefix(&choices, *bound) {
                Some(next) => {
                    prefix = next;
                    parent = choices;
                }
                None => {
                    report.exhausted = true;
                    break;
                }
            },
        }
    }
    report
}

/// The next schedule in depth-first order: the deepest poll that can take one
/// more delay within `bound`, with the polls before it unchanged and none
/// after it.
fn next_prefix(choices: &[Choice], bound: u32) -> Option<Vec<u32>> {
    let mut used: Vec<u32> = Vec::with_capacity(choices.len() + 1);
    used.push(0);
    for c in choices {
        used.push(used.last().unwrap() + c.delays);
    }
    (0..choices.len()).rev().find_map(|i| {
        let c = choices[i];
        (c.choice && c.delays + 1 < c.ready && used[i] + c.delays < bound).then(|| {
            let mut next: Vec<u32> = choices[..i].iter().map(|c| c.delays).collect();
            next.push(c.delays + 1);
            next
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strategies_parse_and_print() {
        for (text, printed) in [
            ("random", "random"),
            ("pct", "pct:d=3"),
            ("pct:d=2,k=50", "pct:d=2,k=50"),
            ("pct:d=2,k=50,fair=9", "pct:d=2,k=50,fair=9"),
            ("dfs", "dfs:bound=2"),
            ("dfs:bound=3,depth=40", "dfs:bound=3,depth=40"),
            ("dfs:bound=1,fair=1", "dfs:bound=1,fair=1"),
        ] {
            let strategy = Strategy::parse(text).unwrap();
            assert_eq!(strategy.to_string(), printed);
            assert_eq!(Strategy::parse(printed).unwrap(), strategy);
        }
        assert!(Strategy::parse("pct:x=1").is_err());
        assert!(Strategy::parse("bfs").is_err());
    }

    /// Every ordering of `0..n`.
    fn permutations(n: usize) -> Vec<Vec<usize>> {
        if n == 0 {
            return vec![Vec::new()];
        }
        let mut out = Vec::new();
        for rest in permutations(n - 1) {
            for at in 0..=rest.len() {
                let mut p = rest.clone();
                p.insert(at, n - 1);
                out.push(p);
            }
        }
        out
    }

    /// Ordered tuples of `len` distinct values in `0..k`.
    fn tuples(len: usize, k: u64) -> Vec<Vec<u64>> {
        if len == 0 {
            return vec![Vec::new()];
        }
        let mut out = Vec::new();
        for rest in tuples(len - 1, k) {
            for v in 0..k {
                if !rest.contains(&v) {
                    let mut t = rest.clone();
                    t.push(v);
                    out.push(t);
                }
            }
        }
        out
    }

    const TASKS: usize = 3;
    const STEPS: usize = 3;

    /// A fixture: task `t` can run its step `j` only once every
    /// `(other, n)` in `needs[t][j]` has run `n` steps.
    type Needs = [[&'static [(usize, usize)]; STEPS]; TASKS];
    /// Three always-ready tasks.
    const FREE: Needs = [[&[]; STEPS]; TASKS];
    /// Blocking and re-enabling: B's second step waits for A's second, C
    /// starts only after B's first, and A's last step waits for C's second.
    const BLOCKING: Needs = [
        [&[], &[], &[(2, 2)]],
        [&[], &[(0, 2)], &[]],
        [&[(1, 1)], &[], &[]],
    ];

    fn enabled(needs: &Needs, done: &[usize; TASKS]) -> Vec<usize> {
        (0..TASKS)
            .filter(|&t| done[t] < STEPS && needs[t][done[t]].iter().all(|&(o, n)| done[o] >= n))
            .collect()
    }

    /// The paper's algorithm (Burckhardt et al. 2010, Fig. 2), written
    /// independently of `Plan`: the enabled thread with the highest priority
    /// executes the step; if the step is `k_i`, that thread's priority then
    /// becomes `i` (1-based).
    fn paper_schedule(needs: &Needs, initial: &[i64], change_points: &[u64]) -> String {
        let mut priority = initial.to_vec();
        let mut done = [0; TASKS];
        let mut out = String::new();
        let mut step = 0u64;
        loop {
            let ready = enabled(needs, &done);
            let Some(&t) = ready.iter().max_by_key(|&&t| priority[t]) else {
                break;
            };
            done[t] += 1;
            out.push((b'A' + t as u8) as char);
            if let Some(i) = change_points.iter().position(|&k| k == step) {
                priority[t] = i as i64 + 1;
            }
            step += 1;
        }
        out
    }

    /// The same schedule produced by `Plan` (task ids 1..=3).
    fn plan_schedule(needs: &Needs, initial: &[i64], change_points: &[u64]) -> String {
        let mut plan = Plan::new(
            Kind::Pct {
                change_points: change_points.to_vec(),
            },
            0,
            u64::MAX,
        );
        for (t, &p) in initial.iter().enumerate() {
            plan.priority.insert(t as u64 + 1, p);
        }
        let mut done = [0; TASKS];
        let mut out = String::new();
        loop {
            let ready: Vec<u64> = enabled(needs, &done)
                .into_iter()
                .map(|t| t as u64 + 1)
                .collect();
            if ready.is_empty() {
                break;
            }
            let t = ready[plan.pick(&ready)] as usize - 1;
            done[t] += 1;
            out.push((b'A' + t as u8) as char);
        }
        out
    }

    /// Every schedule reachable with `d = 3`, by `Plan` and by the paper,
    /// over all initial priority orders and change-point assignments.
    fn reachable(needs: &Needs) -> (BTreeSet<String>, BTreeSet<String>) {
        let depth = 3;
        let k = (TASKS * STEPS) as u64;
        let (mut paper, mut ours) = (BTreeSet::new(), BTreeSet::new());
        for order in permutations(TASKS) {
            let initial: Vec<i64> = order.iter().map(|&r| depth + r as i64).collect();
            for points in tuples(depth as usize - 1, k) {
                let expected = paper_schedule(needs, &initial, &points);
                let got = plan_schedule(needs, &initial, &points);
                assert_eq!(
                    got, expected,
                    "priorities {initial:?}, change points {points:?}"
                );
                paper.insert(expected);
                ours.insert(got);
            }
        }
        (paper, ours)
    }

    /// Codex/agy: PCT follows the paper. The task that executes step `k_i`
    /// then drops to priority `i`, independently of where `k_i` falls in time
    /// (sorting the points made `AAABBCBCC` unreachable). Checked against an
    /// independent implementation of the paper's algorithm, for always-ready
    /// tasks and for tasks that block and are re-enabled.
    #[test]
    fn pct_matches_the_papers_algorithm() {
        let (paper, ours) = reachable(&FREE);
        assert_eq!(ours, paper);
        assert!(ours.contains("AAABBCBCC"), "{} schedules", ours.len());
        assert_eq!(ours.len(), 126);
        let (paper, ours) = reachable(&BLOCKING);
        assert_eq!(ours, paper);
        assert_eq!(ours.len(), 55);

        // The sampled change points keep their draw order: some seeds put
        // k_1 after k_2 (sorting them was the bug).
        let unsorted = (0..200u64).any(|seed| match Plan::pct(seed, 3, 9, u64::MAX).kind {
            Kind::Pct { change_points } => change_points[0] > change_points[1],
            Kind::Delays { .. } => unreachable!(),
        });
        assert!(unsorted, "PCT change points are always drawn in time order");
    }

    /// Codex (round 2): initial PCT priorities do not depend on the order
    /// the ready list happens to be in.
    #[test]
    fn pct_priorities_do_not_depend_on_ready_order() {
        for seed in 0..32 {
            let mut a = Plan::pct(seed, 3, 9, u64::MAX);
            let mut b = Plan::pct(seed, 3, 9, u64::MAX);
            let forward = [1, 2, 3, 4];
            let backward = [4, 3, 2, 1];
            assert_eq!(
                forward[a.pick(&forward)],
                backward[b.pick(&backward)],
                "seed {seed}"
            );
            assert_eq!(a.priority, b.priority, "seed {seed}");
        }
    }

    #[test]
    fn dfs_enumerates_each_bounded_schedule_once() {
        // Three polls with 2, 3 and 1 ready tasks; bound 2.
        let ready = [2, 3, 1];
        let mut prefix = Vec::new();
        let mut seen = BTreeSet::new();
        loop {
            let choices: Vec<Choice> = ready
                .iter()
                .enumerate()
                .map(|(i, &r)| Choice {
                    ready: r,
                    delays: prefix.get(i).copied().unwrap_or(0),
                    choice: true,
                    set: i as u64,
                })
                .collect();
            let schedule: Vec<u32> = choices.iter().map(|c| c.delays).collect();
            assert!(schedule.iter().sum::<u32>() <= 2);
            assert!(seen.insert(schedule), "visited twice");
            match next_prefix(&choices, 2) {
                Some(next) => prefix = next,
                None => break,
            }
        }
        // a in 0..2, b in 0..3, a + b <= 2: (0,0) (0,1) (0,2) (1,0) (1,1).
        assert_eq!(seen.len(), 5);
    }
}
