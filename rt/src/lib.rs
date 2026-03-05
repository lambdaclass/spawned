//! Runtime abstraction layer for `spawned`.
//!
//! This crate wraps tokio and standard library primitives behind a uniform
//! interface. Users typically don't depend on `spawned-rt` types directly —
//! the relevant re-exports (`run`, `CancellationToken`, etc.) are available
//! through `spawned_concurrency::tasks` and `spawned_concurrency::threads`.
//!
//! # Modules
//!
//! - [`tasks`] — async runtime backed by tokio: `run()`, `CancellationToken`,
//!   `send_after`, `send_interval`, `send_message_on`
//! - [`threads`] — blocking runtime using OS threads: `CancellationToken`,
//!   `send_after`, `send_interval`

pub mod tasks;
pub mod threads;
mod tracing;
