//! # spawned-concurrency
//!
//! Erlang/OTP-style concurrency primitives for Rust.
//!
//! This crate provides building blocks for implementing concurrent, fault-tolerant
//! systems using patterns inspired by Erlang/OTP:
//!
//! - **[`GenServer`]** - A generic server abstraction for request-reply patterns
//! - **[`Supervisor`]** - Manages child processes with automatic restart
//! - **[`DynamicSupervisor`]** - Runtime-configurable supervisor for dynamic children
//! - **[`Process`]** - Simple process abstraction for message passing
//!
//! ## Core Concepts
//!
//! ### Process Identification
//!
//! Every process has a unique [`Pid`] (Process ID) that can be used for:
//! - Sending messages
//! - Linking and monitoring
//! - Registration in the global registry
//!
//! ### Links and Monitors
//!
//! Processes can be **linked** or **monitored**:
//! - **Links** are bidirectional - if one process dies abnormally, linked processes die too
//! - **Monitors** are unidirectional - the monitoring process receives a [`SystemMessage::Down`]
//!
//! Use [`process_table::link`] and [`process_table::monitor`] for these operations.
//!
//! ### Name Registration
//!
//! Processes can be registered with a name using the [`registry`] module:
//!
//! ```ignore
//! use spawned_concurrency::registry;
//!
//! // Register a process
//! registry::register("my_server", pid)?;
//!
//! // Look up by name
//! if let Some(pid) = registry::whereis("my_server") {
//!     // send message to pid
//! }
//! ```
//!
//! ## Quick Start: GenServer
//!
//! The [`GenServer`] trait is the primary abstraction for building concurrent servers:
//!
//! ```ignore
//! use spawned_concurrency::{GenServer, GenServerHandle, Backend};
//!
//! struct Counter { count: u32 }
//!
//! impl GenServer for Counter {
//!     type CallMsg = ();
//!     type CastMsg = ();
//!     type OutMsg = u32;
//!     type State = Self;
//!     type Error = ();
//!
//!     // Implement callbacks...
//! }
//!
//! // Start the server
//! let handle = Counter { count: 0 }.start(Backend::Async);
//! ```
//!
//! ## Supervision Trees
//!
//! Build fault-tolerant systems using [`Supervisor`] and [`DynamicSupervisor`]:
//!
//! ```ignore
//! use spawned_concurrency::{ChildSpec, SupervisorSpec, RestartStrategy};
//!
//! let spec = SupervisorSpec::new(RestartStrategy::OneForOne)
//!     .child(ChildSpec::worker("worker1", || MyWorker::new().start(Backend::Async)));
//!
//! let supervisor = Supervisor::start(spec).await?;
//! ```
//!
//! ## Backends
//!
//! GenServers can run on different backends via [`Backend`]:
//! - `Backend::Async` - Tokio async tasks (default)
//! - `Backend::Blocking` - Tokio blocking thread pool
//! - `Backend::Thread` - Dedicated OS thread

pub mod error;
mod gen_server;
pub mod link;
pub mod messages;
pub mod pid;
mod process;
pub mod process_table;
pub mod registry;
mod stream;
pub mod supervisor;
mod time;

#[cfg(test)]
mod stream_tests;
#[cfg(test)]
mod timer_tests;

pub use error::GenServerError;
pub use gen_server::{
    send_message_on, Backend, CallResponse, CastResponse, GenServer, GenServerHandle,
    GenServerInMsg, InitResult, InitResult::NoSuccess, InitResult::Success,
};
pub use link::{MonitorRef, SystemMessage};
pub use pid::{ExitReason, HasPid, Pid};
pub use process::{send, Process, ProcessInfo};
pub use process_table::LinkError;
pub use registry::RegistryError;
pub use stream::spawn_listener;
pub use supervisor::{
    BoxedChildHandle, ChildHandle, ChildInfo, ChildSpec, ChildType, DynamicSupervisor,
    DynamicSupervisorCall, DynamicSupervisorCast, DynamicSupervisorError, DynamicSupervisorResponse,
    DynamicSupervisorSpec, RestartStrategy, RestartType, Shutdown, Supervisor, SupervisorCall,
    SupervisorCast, SupervisorCounts, SupervisorError, SupervisorResponse, SupervisorSpec,
};
pub use time::{send_after, send_interval};
