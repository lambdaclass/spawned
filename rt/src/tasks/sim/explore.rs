//! Systematic interleaving exploration for the simulator.
//!
//! The simulator makes one choice per poll: which ready task runs next. The
//! default answers it with a seeded random walk. This module adds two
//! strategies that search that choice space with known guarantees:
//!
//! - **PCT** (probabilistic concurrency testing, Burckhardt et al., ASPLOS
//!   2010). Every task gets a random priority when it first becomes ready and
//!   the highest-priority ready task always runs. `d - 1` change points,
//!   drawn uniformly over an estimate `k` of the run length, each demote the
//!   task about to run below every other task. A bug of depth `d` (`d`
//!   ordering constraints) is found by one run with probability at least
//!   `1 / (n * k^(d-1))` for `n` tasks, independent of how many ways there
//!   are to interleave the rest.
//! - **Bounded DFS over delays.** The same priority scheduler made
//!   deterministic: priorities start in task-id order, so with no delays it
//!   is the canonical schedule (always the smallest ready id). A *delay* at a
//!   poll demotes the task about to run below every other task, for good
//!   (delay bounding, Emmi, Qadeer and Rakamarić, POPL 2011, with a
//!   persistent demotion as in PCT). The search enumerates, depth first and
//!   statelessly (each schedule is a replayed prefix of delay counts), every
//!   schedule with at most `bound` delays, and reports whether it exhausted
//!   that space. There is no partial-order reduction: the executor sees
//!   wakers, not which shared state a poll touches.
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
//! those later polls are not choice points.
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
    /// PCT with bug depth `depth` (`depth - 1` change points) over a run
    /// length estimate `steps`; `None` estimates it as the longest run so far.
    Pct { depth: u32, steps: Option<u64> },
    /// Depth-first enumeration of every schedule with at most `bound` delays;
    /// with `depth`, only the first `depth` polls of a run are choice points.
    Dfs { bound: u32, depth: Option<u64> },
}

impl Strategy {
    /// Parse `random`, `pct[:d=3][,k=200]` or `dfs[:bound=2][,depth=500]`.
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
            },
            "dfs" => Strategy::Dfs {
                bound: take(&["bound", "b", "delays"]).unwrap_or(2) as u32,
                depth: take(&["depth"]),
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
            Strategy::Pct { depth, steps } => {
                write!(f, "pct:d={depth}")?;
                match steps {
                    Some(k) => write!(f, ",k={k}"),
                    None => Ok(()),
                }
            }
            Strategy::Dfs { bound, depth } => {
                write!(f, "dfs:bound={bound}")?;
                match depth {
                    Some(depth) => write!(f, ",depth={depth}"),
                    None => Ok(()),
                }
            }
        }
    }
}

/// One choice point of a PCT or DFS run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Choice {
    /// Ready tasks at this poll (1 when the poll was not a choice point).
    pub ready: u32,
    /// Delays (demotions) applied before picking.
    pub delays: u32,
}

#[derive(Debug)]
enum Kind {
    Pct { depth: u32, change_points: Vec<u64> },
    Delays { prefix: Vec<u32>, depth: u64 },
}

/// The per-run priority scheduler behind PCT and DFS.
#[derive(Debug)]
pub(super) struct Plan {
    kind: Kind,
    rng: u64,
    /// Higher runs first; ties go to the smaller task id.
    priority: BTreeMap<u64, i64>,
    last_polled: BTreeMap<u64, u64>,
    step: u64,
    fair_after: u64,
    demotions: i64,
    choices: Vec<Choice>,
}

fn fair_after() -> u64 {
    std::env::var("SPAWNED_SIM_FAIR_AFTER")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(DEFAULT_FAIR_AFTER)
}

impl Plan {
    fn new(kind: Kind, rng: u64) -> Self {
        Self {
            kind,
            rng,
            priority: BTreeMap::new(),
            last_polled: BTreeMap::new(),
            step: 0,
            fair_after: fair_after(),
            demotions: 0,
            choices: Vec::new(),
        }
    }

    /// PCT for one run: `depth - 1` distinct change points in `0..steps`.
    fn pct(seed: u64, depth: u32, steps: u64) -> Self {
        // Separate from the seeded random walk's stream.
        let mut rng = seed ^ 0x5043_545f_5043_545f;
        let mut points = BTreeSet::new();
        let wanted = (depth.saturating_sub(1) as u64).min(steps.max(1));
        while (points.len() as u64) < wanted {
            points.insert(splitmix(&mut rng) % steps.max(1));
        }
        Self::new(
            Kind::Pct {
                depth,
                change_points: points.into_iter().collect(),
            },
            rng,
        )
    }

    /// Delay-bounded run: `prefix[i]` delays at poll `i`, none afterwards.
    fn delays(prefix: Vec<u32>, depth: u64) -> Self {
        Self::new(Kind::Delays { prefix, depth }, 0)
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
        for &id in ready {
            if !self.priority.contains_key(&id) {
                let priority = match &self.kind {
                    // Initial PCT priorities are all above the d - 1 change
                    // priorities 1..d.
                    Kind::Pct { depth, .. } => {
                        *depth as i64 + (splitmix(&mut self.rng) >> 33) as i64
                    }
                    Kind::Delays { .. } => 0,
                };
                self.priority.insert(id, priority);
            }
        }
        let index = if step >= self.fair_after {
            self.choices.push(Choice {
                ready: 1,
                delays: 0,
            });
            (0..ready.len())
                .min_by_key(|&i| (self.last_polled.get(&ready[i]).copied(), ready[i]))
                .unwrap_or(0)
        } else {
            let mut order = self.order(ready);
            let mut delays = 0;
            match &self.kind {
                Kind::Pct { change_points, .. } => {
                    // The i-th change point (1-based) sets the task about to
                    // run to priority i: below every initial priority, and
                    // later change points rank above earlier ones (as in the
                    // paper).
                    if let Some(i) = change_points.iter().position(|p| *p == step) {
                        self.priority.insert(ready[order[0]], i as i64 + 1);
                        order = self.order(ready);
                        delays = 1;
                    }
                    self.choices.push(Choice {
                        ready: ready.len() as u32,
                        delays,
                    });
                }
                Kind::Delays { prefix, depth } => {
                    let (choice_point, wanted) = if step < *depth {
                        (true, prefix.get(step as usize).copied().unwrap_or(0))
                    } else {
                        (false, 0)
                    };
                    let wanted = wanted.min(ready.len() as u32 - 1);
                    for _ in 0..wanted {
                        // Demote below everything, including earlier delays.
                        self.demotions += 1;
                        self.priority.insert(ready[order[0]], -self.demotions);
                        order = self.order(ready);
                        delays += 1;
                    }
                    self.choices.push(Choice {
                        ready: if choice_point { ready.len() as u32 } else { 1 },
                        delays,
                    });
                }
            }
            order[0]
        };
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
    /// `k` filled in); for DFS, replay the tape.
    pub strategy: Strategy,
    /// DFS only: delays per poll that reproduce the run.
    pub delays: Option<Vec<u32>>,
    pub outcome: Outcome,
}

/// Result of [`explore`].
#[derive(Debug, Clone)]
pub struct Report {
    pub strategy: Strategy,
    /// Runs made, including the failing one.
    pub runs: usize,
    /// Distinct schedules (by schedule hash) among those runs.
    pub schedules: usize,
    /// Longest run, in polls.
    pub max_polls: u64,
    /// DFS only: every schedule within the bound was run without a failure.
    pub exhausted: bool,
    /// DFS only: runs whose replayed prefix did not apply (the program made a
    /// different choice available than the recorded run); nonzero means the
    /// body is not deterministic under the simulator.
    pub divergences: usize,
    pub found: Option<Found>,
}

/// Run `body` up to `runs` times under `strategy`, stopping at the first
/// failure (panic output is suppressed; the failure is in the report). Random and PCT use seeds `first_seed..`; DFS passes `first_seed`
/// to every run.
pub fn explore<F, Fut>(strategy: &Strategy, first_seed: u64, runs: usize, body: &F) -> Report
where
    F: Fn(u64) -> Fut,
    Fut: Future<Output = ()>,
{
    let mut report = Report {
        strategy: strategy.clone(),
        runs: 0,
        schedules: 0,
        max_polls: 0,
        exhausted: false,
        divergences: 0,
        found: None,
    };
    let mut hashes = BTreeSet::new();
    let mut note = |report: &mut Report, outcome: &Outcome| {
        report.runs += 1;
        hashes.insert(outcome.stats.schedule_hash);
        report.schedules = hashes.len();
        report.max_polls = report.max_polls.max(outcome.stats.steps);
    };
    match strategy {
        Strategy::Random => {
            for seed in (first_seed..).take(runs) {
                let (outcome, _) = quietly(|| run_planned(seed, None, None, body));
                note(&mut report, &outcome);
                if outcome.failure.is_some() {
                    report.found = Some(Found {
                        run: report.runs,
                        seed,
                        strategy: Strategy::Random,
                        delays: None,
                        outcome,
                    });
                    break;
                }
            }
        }
        Strategy::Pct { depth, steps } => {
            let mut estimate = steps.unwrap_or(DEFAULT_PCT_STEPS);
            for seed in (first_seed..).take(runs) {
                let k = estimate;
                let plan = Plan::pct(seed, *depth, k);
                let (outcome, _) = quietly(|| run_planned(seed, None, Some(plan), body));
                note(&mut report, &outcome);
                if outcome.failure.is_some() {
                    report.found = Some(Found {
                        run: report.runs,
                        seed,
                        strategy: Strategy::Pct {
                            depth: *depth,
                            steps: Some(k),
                        },
                        delays: None,
                        outcome,
                    });
                    break;
                }
                if steps.is_none() {
                    estimate = report.max_polls.max(1);
                }
            }
        }
        Strategy::Dfs { bound, depth } => {
            let depth = depth.unwrap_or(u64::MAX);
            let mut prefix: Vec<u32> = Vec::new();
            while report.runs < runs {
                let plan = Plan::delays(prefix.clone(), depth);
                let (outcome, plan) = quietly(|| run_planned(first_seed, None, Some(plan), body));
                note(&mut report, &outcome);
                let choices = plan.map(|p| p.choices).unwrap_or_default();
                let applied = choices.iter().map(|c| c.delays);
                if !applied.take(prefix.len()).eq(prefix.iter().copied()) {
                    report.divergences += 1;
                }
                if outcome.failure.is_some() {
                    report.found = Some(Found {
                        run: report.runs,
                        seed: first_seed,
                        strategy: strategy.clone(),
                        delays: Some(choices.iter().map(|c| c.delays).collect()),
                        outcome,
                    });
                    break;
                }
                match next_prefix(&choices, *bound) {
                    Some(next) => prefix = next,
                    None => {
                        report.exhausted = true;
                        break;
                    }
                }
            }
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
        (c.delays + 1 < c.ready && used[i] + c.delays < bound).then(|| {
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
            ("dfs", "dfs:bound=2"),
            ("dfs:bound=3,depth=40", "dfs:bound=3,depth=40"),
        ] {
            let strategy = Strategy::parse(text).unwrap();
            assert_eq!(strategy.to_string(), printed);
            assert_eq!(Strategy::parse(printed).unwrap(), strategy);
        }
        assert!(Strategy::parse("pct:x=1").is_err());
        assert!(Strategy::parse("bfs").is_err());
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
