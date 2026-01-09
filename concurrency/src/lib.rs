//! spawned concurrency
//! Some basic traits and structs to implement concurrent code à-la-Erlang.
pub mod error;
pub mod link;
pub mod messages;
pub mod pid;
pub mod process_table;
pub mod registry;
pub mod supervisor;
pub mod tasks;
pub mod threads;

/// Backend selection for Actor execution.
///
/// Determines how an Actor is spawned and executed:
/// - `Async`: Runs on the async runtime (tokio tasks) - cooperative multitasking
/// - `Blocking`: Runs on a blocking thread pool (spawn_blocking) - for blocking I/O
/// - `Thread`: Runs on a dedicated OS thread - for long-running singletons
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Backend {
    /// Run on the async runtime (default). Best for non-blocking, I/O-bound tasks.
    #[default]
    Async,
    /// Run on a blocking thread pool. Best for blocking I/O or CPU-bound tasks.
    Blocking,
    /// Run on a dedicated OS thread. Best for long-running singleton actors.
    Thread,
}

// Re-export commonly used types at the crate root
pub use link::{MonitorRef, SystemMessage};
pub use pid::{ExitReason, HasPid, Pid};
pub use process_table::LinkError;
pub use registry::RegistryError;
pub use supervisor::{
    BoxedChildHandle, ChildHandle, ChildInfo, ChildSpec, ChildType, DynamicSupervisor,
    DynamicSupervisorCall, DynamicSupervisorCast, DynamicSupervisorError, DynamicSupervisorResponse,
    DynamicSupervisorSpec, RestartStrategy, RestartType, Shutdown, Supervisor, SupervisorCall,
    SupervisorCast, SupervisorCounts, SupervisorError, SupervisorResponse, SupervisorSpec,
};
