//! Kani bounded model-checking harnesses for the supervisor's pure arithmetic.
//!
//! Compiled only under `cargo kani` (`cfg(kani)`); normal builds never see it.
//! Run with `cargo kani -p spawned-concurrency` (see docs/TESTING.md). Each
//! harness states the property it proves and the bounds it holds under.
//! Harnesses behind `cfg(kani_slow)` are deeper versions that do not finish
//! within the CI budget; run them with
//! `RUSTFLAGS="--cfg kani_slow" cargo kani -p spawned-concurrency --harness <name>`.
use crate::supervision::{admit_in_window, jitter_draw, jitter_seed, Backoff, RecoveryQueue};
use std::{collections::VecDeque, time::Duration};

/// Any `Duration` (full `u64` seconds, sub-second nanos below one second).
fn any_duration() -> Duration {
    let nanos: u32 = kani::any();
    kani::assume(nanos < 1_000_000_000);
    Duration::new(kani::any(), nanos)
}

/// Any sub-second `Duration`.
fn any_subsec() -> Duration {
    let nanos: u32 = kani::any();
    kani::assume(nanos < 1_000_000_000);
    Duration::new(0, nanos)
}

/// A backoff in the domain `SupervisorBuilder` accepts: `initial <= maximum`.
fn backoff(initial: Duration, maximum: Duration, jitter: bool) -> Backoff {
    kani::assume(initial <= maximum);
    Backoff {
        initial,
        maximum,
        stable_after: Duration::ZERO,
        jitter,
        seed: None,
    }
}

/// For every valid config (any `Duration`s) and every failure count: no
/// panic, the pre-jitter bound never exceeds `maximum` and never drops below
/// `initial`, and the first retry waits exactly `initial`.
#[kani::proof]
#[kani::solver(kissat)]
fn backoff_bound_is_capped() {
    let policy = backoff(any_duration(), any_duration(), false);
    let failures: u32 = kani::any();
    let bound = policy.bound(failures);
    assert!(bound <= policy.maximum);
    assert!(bound >= policy.initial);
    if failures == 0 {
        assert_eq!(bound, policy.initial);
    }
}

/// Without jitter, `delay` is exactly the bound and does not advance the
/// jitter stream (sub-second config).
#[kani::proof]
fn backoff_delay_without_jitter_is_bound() {
    let policy = backoff(any_subsec(), any_subsec(), false);
    let failures: u32 = kani::any();
    let mut rng: u64 = kani::any();
    let before = rng;
    assert_eq!(policy.delay(failures, &mut rng), policy.bound(failures));
    assert_eq!(rng, before);
}

/// The pre-jitter bound is monotone non-decreasing in the failure count, for
/// configs in whole seconds (any `u64` seconds).
#[kani::proof]
fn backoff_bound_is_monotone_whole_seconds() {
    let policy = backoff(
        Duration::from_secs(kani::any()),
        Duration::from_secs(kani::any()),
        false,
    );
    let a: u32 = kani::any();
    let b: u32 = kani::any();
    kani::assume(a <= b);
    assert!(policy.bound(a) <= policy.bound(b));
}

/// As above, for sub-second configs (any nanosecond count below one second).
#[kani::proof]
fn backoff_bound_is_monotone_subsecond() {
    let policy = backoff(any_subsec(), any_subsec(), false);
    let a: u32 = kani::any();
    let b: u32 = kani::any();
    kani::assume(a <= b);
    assert!(policy.bound(a) <= policy.bound(b));
}

/// Monotonicity over mixed seconds-and-nanos configs. Times out (>5 min)
/// with both CaDiCaL and Kissat; the two harnesses above cover the
/// whole-second and sub-second sub-domains.
#[cfg(kani_slow)]
#[kani::proof]
#[kani::solver(kissat)]
fn backoff_bound_is_monotone_any_duration() {
    let policy = backoff(any_duration(), any_duration(), false);
    let a: u32 = kani::any();
    let b: u32 = kani::any();
    kani::assume(a <= b);
    assert!(policy.bound(a) <= policy.bound(b));
}

/// The jitter draw maps every stream value into `0..=nanos` without panicking,
/// including `nanos == 0` (a zero `initial`) and `nanos == u64::MAX` (a bound
/// of 584 years or more). Kills the `nanos + 1` -> `- 1` / `* 1` mutants.
#[kani::proof]
fn jitter_draw_stays_in_range() {
    let nanos: u64 = kani::any();
    let rng: u64 = kani::any();
    assert!(jitter_draw(nanos, rng) <= nanos);
}

/// The top and bottom of the draw range are both reachable.
#[kani::proof]
fn jitter_draw_reaches_both_ends() {
    let nanos: u64 = kani::any();
    kani::assume(nanos < u64::MAX);
    assert_eq!(jitter_draw(nanos, nanos), nanos);
    assert_eq!(jitter_draw(nanos, 0), 0);
}

/// The `u64` clamp in `jitter` never lets a draw exceed the bound's true
/// nanosecond count, for every `Duration` bound (including bounds whose
/// nanosecond count does not fit in `u64`).
#[kani::proof]
fn jitter_clamp_stays_within_bound() {
    let bound = any_duration();
    let nanos = bound.as_nanos().min(u64::MAX as u128) as u64;
    let draw: u64 = kani::any();
    kani::assume(draw <= nanos);
    assert!((draw as u128) <= bound.as_nanos());
}

/// Converting a draw back to a `Duration` stays within a sub-second bound.
#[kani::proof]
fn jitter_conversion_stays_within_bound() {
    let bound = any_subsec();
    let nanos = bound.as_nanos().min(u64::MAX as u128) as u64;
    let draw: u64 = kani::any();
    kani::assume(draw <= nanos);
    assert!(Duration::from_nanos(draw) <= bound);
}

/// End to end on sub-second configs: `delay` with jitter never panics and
/// never exceeds `bound(failures)` or `maximum`, for every failure count and
/// stream state.
#[kani::proof]
fn backoff_jitter_stays_in_range() {
    let policy = backoff(any_subsec(), any_subsec(), true);
    let failures: u32 = kani::any();
    let mut rng: u64 = kani::any();
    let delay = policy.delay(failures, &mut rng);
    assert!(delay <= policy.bound(failures));
    assert!(delay <= policy.maximum);
}

/// Jitter on a zero `initial` is exactly zero and does not panic, for any
/// `maximum`, failure count and stream state.
#[kani::proof]
fn backoff_jitter_zero_bound() {
    let policy = backoff(Duration::ZERO, any_duration(), true);
    let failures: u32 = kani::any();
    let mut rng: u64 = kani::any();
    assert_eq!(policy.delay(failures, &mut rng), Duration::ZERO);
}

/// Per-child stream mixing is injective in the supervisor seed for any child
/// name up to 4 bytes: two distinct seeds never collapse onto one stream.
/// Kills the `^` -> `|` / `&` mutants.
#[kani::proof]
#[kani::unwind(5)]
fn jitter_seed_is_injective_in_seed() {
    let bytes: [u8; 4] = kani::any();
    let len: usize = kani::any();
    kani::assume(len <= bytes.len());
    let name = &bytes[..len];
    let a: u64 = kani::any();
    let b: u64 = kani::any();
    kani::assume(a != b);
    assert_ne!(jitter_seed(a, name), jitter_seed(b, name));
}

/// Drives `admit_in_window` over `STEPS` non-decreasing timestamps (each
/// step advances by any `u8`) and checks it against an independent log: a
/// retry is admitted exactly when fewer than `max` earlier admissions lie
/// strictly inside the window ending now. So the budget is never exceeded,
/// and never stricter than its specification.
fn restart_window<const STEPS: usize>(max_limit: usize) {
    let max: usize = kani::any();
    kani::assume(max <= max_limit);
    let w: u8 = kani::any();
    let window = Duration::new(0, w as u32);
    // Preallocated so the harness explores the window logic rather than
    // ring-buffer growth (the deque never holds more than `max` stamps).
    let mut attempts: VecDeque<u16> = VecDeque::with_capacity(4);
    let mut admitted = [0u16; STEPS];
    let mut admitted_len = 0;
    let mut now = 0u16;
    for _ in 0..STEPS {
        now += kani::any::<u8>() as u16;
        let mut in_window = 0;
        for t in &admitted[..admitted_len] {
            if now - *t < w as u16 {
                in_window += 1;
            }
        }
        let taken = admit_in_window(&mut attempts, max, window, now, |now, t| {
            Duration::new(0, (now - t) as u32)
        });
        assert_eq!(taken, in_window < max);
        if taken {
            admitted[admitted_len] = now;
            admitted_len += 1;
        }
        assert!(attempts.len() <= max);
    }
}

/// Restart budget: `max <= 2`, any window up to 255 ns, 3 retries.
#[kani::proof]
#[kani::unwind(5)]
#[kani::solver(kissat)]
fn restart_window_admits_exactly_within_budget() {
    restart_window::<3>(2);
}

/// Deeper restart budget: `max <= 3`, 5 retries. Times out (>5 min).
#[cfg(kani_slow)]
#[kani::proof]
#[kani::unwind(7)]
#[kani::solver(kissat)]
fn restart_window_admits_exactly_within_budget_deep() {
    restart_window::<5>(3);
}

/// Number of request/select operations explored by the recovery-id harness.
const RECOVERY_STEPS: usize = 4;
const CHILDREN: usize = 3;

/// Recovery round ids (the `after`/`last_recovery` generation fences):
/// starting from the real initial queue, for any interleaving of
/// `RECOVERY_STEPS` requests (any child, any `after`) and round selections
/// (either strategy) over `CHILDREN` children:
/// - ids are never 0 and strictly increase, so a controller still holding an
///   older id (initially 0) always sees a newer round as `id > after`;
/// - a child's coverage never moves backwards (no old round resurrected);
/// - a request is accepted exactly when `after >= covered[index]`, so a
///   stale request from a superseded generation cannot reopen a round;
/// - a selected round clears every pending flag it covers.
///
/// `next_id` exhaustion (`checked_add` panics after 2^64 - 1 rounds) is out
/// of reach of any real run and is not explored here.
#[kani::proof]
#[kani::unwind(5)]
fn recovery_rounds_are_fresh_and_monotone() {
    let one_for_all: bool = kani::any();
    let mut queue = RecoveryQueue {
        pending: vec![false; CHILDREN],
        covered: vec![0; CHILDREN],
        next_id: 1,
    };
    let mut last_id = 0u64;
    for _ in 0..RECOVERY_STEPS {
        let before = queue.covered.clone();
        if kani::any() {
            let index: usize = kani::any();
            kani::assume(index < CHILDREN);
            let after: u64 = kani::any();
            let was_pending = queue.pending[index];
            let accepted = queue.request(index, after);
            assert_eq!(accepted, after >= before[index]);
            assert_eq!(queue.pending[index], was_pending || accepted);
        } else if let Some((origin, start, id)) = queue.select(one_for_all) {
            assert!(id > 0 && id > last_id);
            assert!(start <= origin && origin < CHILDREN);
            assert_eq!(start == 0, one_for_all || origin == 0);
            last_id = id;
            for i in start..CHILDREN {
                assert!(!queue.pending[i]);
                assert_eq!(queue.covered[i], id);
            }
            for i in 0..start {
                assert!(!queue.pending[i]);
            }
        }
        for i in 0..CHILDREN {
            assert!(queue.covered[i] >= before[i]);
            assert!(queue.covered[i] <= last_id);
        }
    }
}
