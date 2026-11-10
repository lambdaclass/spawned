//! Bounded waits for unit tests.
//!
//! A fixed sleep is not synchronization: on a loaded machine the effect it
//! waits for may not have happened yet. These helpers poll the real condition
//! and give up only after a deadline far longer than any healthy run needs, so
//! a slow scheduler cannot turn a correct run into a failure. The caller still
//! asserts on the returned value, so a timeout reports the observed state.

use std::time::{Duration, Instant};

/// Generous upper bound for an effect that should happen "soon".
pub(crate) const WAIT: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(2);

/// Probe until `done` holds or [`WAIT`] elapses, returning the last value.
pub(crate) fn wait_for<T>(mut probe: impl FnMut() -> T, done: impl Fn(&T) -> bool) -> T {
    let deadline = Instant::now() + WAIT;
    loop {
        let value = probe();
        if done(&value) || Instant::now() >= deadline {
            return value;
        }
        std::thread::sleep(POLL);
    }
}

/// Async counterpart of [`wait_for`]; yields to the runtime between probes.
#[cfg(feature = "async")]
pub(crate) async fn wait_for_async<T, F, Fut>(mut probe: F, done: impl Fn(&T) -> bool) -> T
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let deadline = Instant::now() + WAIT;
    loop {
        let value = probe().await;
        if done(&value) || Instant::now() >= deadline {
            return value;
        }
        spawned_rt::tasks::sleep(POLL).await;
    }
}
