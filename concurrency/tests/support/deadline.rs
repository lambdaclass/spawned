//! Bounded waits for integration tests. A wait that a bug turns into a hang
//! (a supervisor that never fails, an exit that is never published, a message
//! that never arrives) fails the test with the expression and line it was
//! waiting on, instead of hanging the job until CI's timeout. The bound is far
//! above any legitimate wait, which are all handshakes, never fixed sleeps: on
//! the simulator it is virtual time, on Tokio it is only reached by a bug.
//!
//! ```ignore
//! #[macro_use]
//! #[path = "support/deadline.rs"]
//! mod deadline;
//! let tree = within!(tree.start()).await.unwrap();
//! ```
#![allow(dead_code, unused_macros)]
use std::time::Duration;

pub const WAIT: Duration = Duration::from_secs(120);

#[cfg(feature = "async")]
pub async fn within<F: std::future::Future>(what: &str, future: F) -> F::Output {
    match spawned_rt::tasks::timeout(WAIT, future).await {
        Ok(output) => output,
        Err(_) => panic!("timed out after {WAIT:?} waiting for {what}"),
    }
}

macro_rules! within {
    ($future:expr) => {
        $crate::deadline::within(
            concat!("`", stringify!($future), "` at ", file!(), ":", line!()),
            $future,
        )
    };
}
