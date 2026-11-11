//! Clock and entropy seam for library code.
//!
//! In normal builds [`Instant`] is `std::time::Instant` and [`entropy`] reads
//! the wall clock, so behaviour is identical to calling them directly. Under
//! `--cfg spawned_sim` both come from the running simulation: `Instant` reads
//! virtual time and `entropy` is a stream derived from the simulation seed, so
//! restart windows, shutdown deadlines, backoff jitter and metrics timings are
//! deterministic per seed and advance in virtual time.

#[cfg(not(all(spawned_sim, feature = "async")))]
pub use std::time::Instant;

#[cfg(all(spawned_sim, feature = "async"))]
pub use crate::tasks::sim::Instant;

/// Seed material for jitter and similar randomization: wall-clock nanoseconds
/// mixed with a process-wide counter (distinct per call), or, under
/// simulation, the next value of the simulation's entropy stream.
pub fn entropy() -> u64 {
    #[cfg(all(spawned_sim, feature = "async"))]
    {
        crate::tasks::sim::entropy()
    }
    #[cfg(not(all(spawned_sim, feature = "async")))]
    {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64
            ^ NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }
}

#[doc(hidden)] // clock plumbing for spawned-concurrency; no stability promise
/// Largest tick value an instant maps to; `u64::MAX` is reserved for "never".
pub const MAX_TICKS: u64 = u64::MAX - 1;

#[doc(hidden)] // clock plumbing for spawned-concurrency; no stability promise
/// Nanosecond clock ticks of `instant`, for packing deadlines into atomics.
///
/// Ticks count from a process-wide epoch (the first call), or, under
/// simulation, from the start of the simulated world. Instants before the
/// epoch map to 0 and instants beyond the range saturate at [`MAX_TICKS`].
pub fn to_ticks(instant: Instant) -> u64 {
    let nanos = elapsed_since_epoch(instant).as_nanos();
    nanos.min(MAX_TICKS as u128) as u64
}

#[doc(hidden)] // clock plumbing for spawned-concurrency; no stability promise
/// Inverse of [`to_ticks`].
pub fn from_ticks(ticks: u64) -> Instant {
    let elapsed = std::time::Duration::from_nanos(ticks.min(MAX_TICKS));
    #[cfg(all(spawned_sim, feature = "async"))]
    {
        crate::tasks::sim::Instant::from_start(elapsed)
    }
    #[cfg(not(all(spawned_sim, feature = "async")))]
    {
        let epoch = epoch();
        epoch.checked_add(elapsed).unwrap_or(epoch)
    }
}

#[doc(hidden)] // clock plumbing for spawned-concurrency; no stability promise
/// Ticks of [`Instant::now`].
pub fn now_ticks() -> u64 {
    to_ticks(Instant::now())
}

#[cfg(not(all(spawned_sim, feature = "async")))]
fn epoch() -> Instant {
    static EPOCH: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

fn elapsed_since_epoch(instant: Instant) -> std::time::Duration {
    #[cfg(all(spawned_sim, feature = "async"))]
    {
        instant.since_start()
    }
    #[cfg(not(all(spawned_sim, feature = "async")))]
    {
        instant.saturating_duration_since(epoch())
    }
}

#[cfg(test)]
mod tick_tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn ticks_round_trip_and_order() {
        let _ = now_ticks(); // initialize the epoch before `a`
        let a = Instant::now();
        let b = a + Duration::from_millis(5);
        assert!(to_ticks(b) > to_ticks(a));
        assert_eq!(to_ticks(b) - to_ticks(a), 5_000_000);
        let back = from_ticks(to_ticks(b));
        assert_eq!(to_ticks(back), to_ticks(b));
        assert!(now_ticks() >= to_ticks(a));
    }
}
