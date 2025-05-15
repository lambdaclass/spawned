//! Runtime wrapper to remove dependencies from code. Using this library will
//! allow to set a tokio runtime or any other runtime, once implemented just by
//! changing the enabled feature.
//! May implement the `deterministic` version based on comonware.xyz's runtime:
//! https://github.com/commonwarexyz/monorepo/blob/main/runtime/src/deterministic.rs
//!
//! Currently, only a very limited set of tokio functionality is reexported. We may want to
//! extend this functionality as needed.

pub mod r#async;
pub mod sync;
mod tracing;
