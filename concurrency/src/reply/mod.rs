//! Reply transport for requests: reply slots, tokens and receivers
//! (docs/design/deferred-replies.md).

pub(crate) mod slot;

/// Production synchronization types for [`slot`]; the Loom model supplies its
/// own (`tests/reply_slot_loom.rs`).
mod sync {
    pub(crate) use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    pub(crate) use std::sync::{Arc, Condvar, Mutex, MutexGuard};
    pub(crate) use std::thread::yield_now;

    /// Wait on `condvar` for at most `ticks` clock ticks (nanoseconds), or
    /// until notified when `None`.
    pub(crate) fn wait_ticks<'a, T>(
        condvar: &Condvar,
        guard: MutexGuard<'a, T>,
        ticks: Option<u64>,
    ) -> MutexGuard<'a, T> {
        match ticks {
            None => condvar.wait(guard).unwrap_or_else(|p| p.into_inner()),
            Some(ticks) => {
                condvar
                    .wait_timeout(guard, std::time::Duration::from_nanos(ticks))
                    .unwrap_or_else(|p| p.into_inner())
                    .0
            }
        }
    }
}

/// Clock ticks now, on the `spawned_rt` clock (virtual under simulation).
pub(crate) fn now_ticks() -> u64 {
    spawned_rt::time::now_ticks()
}
