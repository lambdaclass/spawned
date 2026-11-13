//! End-of-run oracles over the factory log and the instance log: restart
//! decisions (V1, V2), required restarts against budget exhaustion (V5),
//! backoff delays in virtual time (V6) and cancelled timers. They judge only
//! what the log determines; anything a race could explain is skipped, so a
//! violation is a contract breach, never a schedule the harness misread.
//!
//! Restart and backoff checks cover flat root supervisors (every declared
//! child a worker). A worker instance "could stop by itself" when something
//! other than its supervisor may have stopped it: a driver or handler stop,
//! an injected panic or destructor panic, a link, or a startup it may not
//! have finished. An instance that could not was stopped by its supervisor
//! (a cohort round, a close or a failure).

use crate::world::{Call, Inst, Outcome, State, SupInst, World};
use spawned_concurrency::{
    supervision::{RestartMode, SupervisorStrategy},
    ExitReason,
};
use std::time::Duration;

/// Slack on the upper bound of an observed backoff delay: virtual time
/// creeps 1 ms per busy stretch of polls.
fn slack(bound: Duration) -> Duration {
    Duration::from_millis(10) + bound / 4
}

pub fn check(world: &World) {
    let mut s = world.lock();
    let mut found: Vec<(&'static str, String)> = Vec::new();
    for (inst, sup) in s.sups.iter().enumerate() {
        if !sup.flat {
            continue;
        }
        let calls: Vec<(usize, &Call)> = s
            .calls
            .iter()
            .enumerate()
            .filter(|(_, c)| c.inst == inst)
            .collect();
        let slots: Vec<Vec<&Call>> = (0..sup.children)
            .map(|j| {
                calls
                    .iter()
                    .filter(|(_, c)| c.index == j)
                    .map(|(_, c)| *c)
                    .collect()
            })
            .collect();
        restart_justified(&s, sup, &slots, &mut found);
        required_restarts(&s, sup, &slots, &mut found);
        budget_exhaustion(&s, sup, &calls, &mut found);
        if sup.strategy == SupervisorStrategy::OneForOne {
            backoff_delays(&s, sup, &slots, &mut found);
        }
    }
    timers(&s, &mut found);
    for (kind, detail) in found {
        s.violate(kind, detail);
    }
}

fn instance<'a>(s: &'a State, c: &Call) -> Option<(usize, &'a Inst)> {
    match c.outcome {
        Outcome::Instance(i) => Some((i, &s.insts[i])),
        _ => None,
    }
}

/// Something other than its supervisor may have stopped this instance.
fn could_stop_itself(s: &State, x: &Inst) -> bool {
    !x.published || x.panicked || x.drop_panicked || x.id.is_none_or(|id| s.may_stop.contains(&id))
}

/// Tick at which an attempt ended: the factory's return, or the drop of the
/// instance's state (before its exit is published, L6/L7).
fn ended(s: &State, c: &Call) -> Option<u64> {
    match c.outcome {
        Outcome::Error | Outcome::Panic => Some(c.seq),
        Outcome::Instance(i) => s.insts[i].dropped,
        Outcome::Pending => None,
    }
}

fn ended_at(s: &State, c: &Call) -> Option<Duration> {
    match c.outcome {
        Outcome::Error | Outcome::Panic => Some(c.at),
        Outcome::Instance(i) => s.insts[i].dropped_at,
        Outcome::Pending => None,
    }
}

/// Siblings whose failure or stop can make `j` collateral: every other slot
/// for OneForAll, the earlier ones for RestForOne (V2).
fn upstream(strategy: SupervisorStrategy, j: usize, k: usize) -> bool {
    match strategy {
        SupervisorStrategy::OneForOne => false,
        SupervisorStrategy::OneForAll => k != j,
        SupervisorStrategy::RestForOne => k < j,
        _ => false, // #[non_exhaustive]: a later strategy is not judged here
    }
}

fn abnormal(reason: &Option<ExitReason>) -> bool {
    reason.as_ref().is_none_or(ExitReason::is_abnormal)
}

// Cleanup failure preserves the published reason, but independently restarts
// a Transient child (MIGRATION, destructor panics; RestartMode::restarts).
fn abnormal_or_cleanup_failure(instance: &Inst) -> bool {
    abnormal(&instance.reason) || instance.drop_panicked
}

/// V1/V2: every replacement is one the restart table or the strategy asks
/// for. A replacement of slot `j` follows an attempt that ended by itself
/// in a way its restart mode restarts (a failed or unpublished attempt
/// unless Temporary, any exit for Permanent, an abnormal one for
/// Transient, including a cleanup failure with a clean reason), or an attempt of an upstream sibling that ended after the
/// previous attempt of `j` began (`j` was collateral of that sibling's
/// round). Temporary replacements are V1's own check.
fn restart_justified(
    s: &State,
    sup: &SupInst,
    slots: &[Vec<&Call>],
    found: &mut Vec<(&'static str, String)>,
) {
    for (j, attempts) in slots.iter().enumerate() {
        for pair in attempts.windows(2) {
            let (prev, next) = (pair[0], pair[1]);
            if next.mode == RestartMode::Temporary {
                continue;
            }
            let own = match instance(s, prev) {
                None => true,
                Some((_, x)) => {
                    if x.dropped.is_none_or(|d| d > next.seq) {
                        continue; // V3 reports an overlap.
                    }
                    could_stop_itself(s, x)
                        && match next.mode {
                            RestartMode::Permanent => true,
                            RestartMode::Transient => {
                                !x.published || abnormal_or_cleanup_failure(x)
                            }
                            RestartMode::Temporary => false,
                        }
                }
            };
            if own {
                continue;
            }
            let collateral = slots.iter().enumerate().any(|(k, other)| {
                upstream(sup.strategy, j, k)
                    && other.iter().any(|c| {
                        c.seq < next.seq
                            && ended(s, c).is_some_and(|e| e > prev.seq && e < next.seq)
                    })
            });
            if !collateral {
                let reason = instance(s, prev).map(|(i, x)| (i, x.reason.clone()));
                found.push((
                    "V1/V2: a child was restarted although neither its exit nor its strategy requires it",
                    format!(
                        "{} ({:?}, {:?}) after {reason:?}",
                        next.slot, next.mode, sup.strategy
                    ),
                ));
                return;
            }
        }
    }
}

/// V1/V2/V5: in a tree that neither the driver closed nor failed, every
/// restart the contract requires happened by the time teardown found it
/// settled: a published Permanent instance, or a published Transient one
/// that exited abnormally or failed cleanup, has a successor; so does every live,
/// non-Temporary downstream sibling (its cohort). A missing restart means
/// the budget was exhausted, which must fail the supervisor (V5).
fn required_restarts(
    s: &State,
    sup: &SupInst,
    slots: &[Vec<&Call>],
    found: &mut Vec<(&'static str, String)>,
) {
    let (Some(settled), None) = (sup.settled, &sup.failure) else {
        return;
    };
    let after = |j: usize, tick: u64| slots[j].iter().any(|c| c.seq > tick);
    for (k, attempts) in slots.iter().enumerate() {
        for c in attempts {
            let Some((i, x)) = instance(s, c) else {
                continue;
            };
            let Some(dropped) = x.dropped.filter(|d| *d < settled) else {
                continue;
            };
            let required = x.published
                && match c.mode {
                    RestartMode::Permanent => true,
                    RestartMode::Transient => abnormal_or_cleanup_failure(x),
                    RestartMode::Temporary => false,
                };
            if !required {
                continue;
            }
            if !after(k, dropped) {
                found.push((
                    "V5: a required restart did not happen and the supervisor did not fail",
                    format!("{} w{i} {:?} ({:?})", c.slot, x.reason, c.mode),
                ));
                return;
            }
            // Downstream siblings running untouched when this one ended.
            // (If this instance was itself collateral, its round began
            // upstream and already stopped them: they are not running.)
            for (j, others) in slots.iter().enumerate() {
                if !(sup.strategy == SupervisorStrategy::RestForOne && j > k
                    || sup.strategy == SupervisorStrategy::OneForAll && j != k)
                {
                    continue;
                }
                let live = others.iter().any(|o| {
                    o.mode != RestartMode::Temporary
                        && o.seq < dropped
                        && instance(s, o).is_some_and(|(_, y)| {
                            y.published
                                && y.stopped_begin.is_none_or(|b| b > dropped)
                                && y.dropped.is_none_or(|d| d > dropped)
                                && (o.mode == RestartMode::Permanent || !could_stop_itself(s, y))
                        })
                });
                if live && !after(j, dropped) {
                    found.push((
                        "V2: a sibling in the failed child's cohort was not restarted",
                        format!(
                            "{} after {} w{i} ({:?})",
                            slot_name(sup, j),
                            c.slot,
                            sup.strategy
                        ),
                    ));
                    return;
                }
            }
        }
    }
}

fn slot_name(sup: &SupInst, j: usize) -> String {
    format!("r{}/c{j}", sup.root)
}

/// V5: a supervisor fails for its restart budget only when the window was
/// full. The exhausting reservation was refused at some moment between the
/// end of that child's last attempt and the supervisor's last factory call
/// (a collateral child reserves only after its round's cleanup, and a
/// superseding round can make it reserve again): at one of those moments the
/// grants in the window plus the reservations siblings can hold (at most one
/// each) reach `max_restarts`.
fn budget_exhaustion(
    s: &State,
    sup: &SupInst,
    calls: &[(usize, &Call)],
    found: &mut Vec<(&'static str, String)>,
) {
    let Some(failure) = &sup.failure else { return };
    let Some(rest) = failure.split("restart budget exhausted by ").nth(1) else {
        return;
    };
    let name: String = rest
        .chars()
        .take_while(|c| !c.is_whitespace() && *c != ',' && *c != ')')
        .collect();
    let Some(last) = calls.iter().rev().find(|(_, c)| c.slot == name) else {
        return;
    };
    let Some(end) = ended_at(s, last.1) else {
        return;
    };
    let mut first = std::collections::HashSet::new();
    let grants: Vec<Duration> = calls
        .iter()
        .filter(|(_, c)| !first.insert(c.index))
        .map(|(_, c)| c.at)
        .collect();
    let in_window = |t: Duration| {
        grants
            .iter()
            .filter(|g| **g <= t && t < **g + sup.window)
            .count()
    };
    let most = std::iter::once(end)
        .chain(grants.iter().copied().filter(|g| *g >= end))
        .map(in_window)
        .max()
        .unwrap_or(0);
    let siblings = sup.children.saturating_sub(1);
    if most + siblings < sup.max_restarts {
        found.push((
            "V5: the restart budget was reported exhausted before the window was full",
            format!(
                "{name}: at most {most} grants + {siblings} siblings < {} in {:?}",
                sup.max_restarts, sup.window
            ),
        ));
    }
}

/// V6, OneForOne only: the delay between the end of an attempt and the next
/// factory call is the backoff bound for the child's failure streak (from
/// zero through it with jitter). The streak counts restarts and resets after
/// a generation that stayed ready for `stable_after`; it is tracked as an
/// interval where stability is too close to call.
fn backoff_delays(
    s: &State,
    sup: &SupInst,
    slots: &[Vec<&Call>],
    found: &mut Vec<(&'static str, String)>,
) {
    let b = &sup.backoff;
    let bound = |failures: u32| {
        b.initial
            .saturating_mul(1u32 << failures.min(31))
            .min(b.maximum)
    };
    let margin = Duration::from_millis(1);
    for attempts in slots {
        let (mut lo, mut hi) = (0u32, 0u32);
        for pair in attempts.windows(2) {
            let (prev, next) = (pair[0], pair[1]);
            let Some(end) = ended_at(s, prev) else { break };
            match instance(s, prev).map(|(_, x)| (x.started_end_at, x.stopped_begin_at)) {
                Some((Some(ready), Some(stop))) => {
                    let up = stop.saturating_sub(ready);
                    if up >= b.stable_after + margin {
                        (lo, hi) = (0, 0);
                    } else if up + margin > b.stable_after {
                        lo = 0;
                    }
                }
                Some((Some(_), None)) => lo = 0,
                _ => {}
            }
            let delay = next.at.saturating_sub(end);
            let min = if b.jitter { Duration::ZERO } else { bound(lo) };
            let max = bound(hi);
            if delay < min || delay > max + slack(max) {
                found.push((
                    "V6: a restart delay is outside the backoff bound for the failure streak",
                    format!(
                        "{}: {delay:?} not in [{min:?}, {max:?}] (streak {lo}..={hi}, {:?})",
                        next.slot, b
                    ),
                ));
                return;
            }
            (lo, hi) = (lo.saturating_add(1), hi.saturating_add(1));
        }
    }
}

/// A timer cancelled before a tick was due never delivers that tick: a
/// one-shot timer at most once if cancelled at or after `created + period`,
/// an interval at most once per period elapsed before the cancel.
fn timers(s: &State, found: &mut Vec<(&'static str, String)>) {
    for (i, t) in s.timers.iter().enumerate() {
        let Some(cancelled) = t.cancelled else {
            continue;
        };
        let due = (cancelled.saturating_sub(t.created).as_nanos() / t.period.as_nanos()) as u64;
        let allowed = if t.interval { due } else { due.min(1) };
        if t.handled > allowed {
            found.push((
                "T: a cancelled timer delivered a message after its cancellation",
                format!(
                    "timer {i} ({}, {:?}): {} handled, {allowed} due by the cancel",
                    if t.interval { "interval" } else { "once" },
                    t.period,
                    t.handled
                ),
            ));
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::State;
    use spawned_concurrency::supervision::Backoff;

    fn tree() -> SupInst {
        SupInst {
            root: 0,
            max_restarts: 4,
            window: Duration::from_secs(1),
            cohort: false,
            strategy: SupervisorStrategy::OneForOne,
            backoff: Backoff::default(),
            flat: true,
            children: 1,
            closed_at: None,
            settled: Some(5),
            failure: None,
        }
    }
    fn attempt(seq: u64, outcome: Outcome) -> Call {
        Call {
            inst: 0,
            slot: "r0/c0".into(),
            index: 0,
            at: Duration::ZERO,
            seq,
            mode: RestartMode::Transient,
            outcome,
        }
    }
    fn state(cleanup_failed: bool) -> State {
        let mut state = State::default();
        state.insts.push(Inst {
            published: true,
            dropped: Some(2),
            reason: Some(ExitReason::Normal),
            drop_panicked: cleanup_failed,
            ..Inst::default()
        });
        state
    }
    #[test]
    fn transient_restart_requires_abnormal_exit_or_failed_cleanup() {
        let previous = attempt(1, Outcome::Instance(0));
        let next = attempt(3, Outcome::Error);
        for cleanup_failed in [false, true] {
            let mut found = Vec::new();
            restart_justified(
                &state(cleanup_failed),
                &tree(),
                &[vec![&previous, &next]],
                &mut found,
            );
            assert_eq!(
                found.is_empty(),
                cleanup_failed,
                "a clean Normal exit alone must not permit a Transient restart"
            );
        }
    }
    #[test]
    fn missing_transient_cleanup_restart_is_still_reported() {
        let previous = attempt(1, Outcome::Instance(0));
        for cleanup_failed in [false, true] {
            let mut found = Vec::new();
            required_restarts(
                &state(cleanup_failed),
                &tree(),
                &[vec![&previous]],
                &mut found,
            );
            assert_eq!(
                !found.is_empty(),
                cleanup_failed,
                "failed cleanup requires a restart even when the reason stayed Normal"
            );
        }
    }
}
