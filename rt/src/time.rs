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
