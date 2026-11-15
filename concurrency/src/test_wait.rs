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

/// Bound for a wait a bug can turn into a hang (`tests/support/deadline.rs`
/// uses the same): far above any healthy wait, which are all handshakes.
pub(crate) const HANG: Duration = Duration::from_secs(120);

/// Run a test's body (its runtime and that runtime's teardown included) on
/// its own thread, named after the test. A body still running after [`HANG`]
/// fails the test by name instead of hanging the job; a panic in the body
/// (a failed assertion, or a `#[should_panic]` test's expected panic) is
/// re-raised unchanged.
pub(crate) fn bounded<T: Send + 'static>(
    name: &str,
    body: impl FnOnce() -> T + Send + 'static,
) -> T {
    let (done, finished) = std::sync::mpsc::channel();
    let thread = std::thread::Builder::new()
        .name(name.to_string())
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body));
            let _ = done.send(result);
        })
        .expect("spawn the test body");
    match finished.recv_timeout(HANG) {
        Ok(Ok(value)) => {
            let _ = thread.join();
            value
        }
        Ok(Err(panic)) => std::panic::resume_unwind(panic),
        Err(_) => panic!("timed out after {HANG:?}: {name} did not finish"),
    }
}

/// Async counterpart of [`bounded`] for `#[tokio::test]` bodies: the body's
/// awaits are bounded by [`HANG`] (the runtime's own teardown is not).
#[cfg(feature = "async")]
pub(crate) async fn within<F: std::future::Future>(name: &str, body: F) -> F::Output {
    match spawned_rt::tasks::timeout(HANG, body).await {
        Ok(output) => output,
        Err(_) => panic!("timed out after {HANG:?}: {name} did not finish"),
    }
}
