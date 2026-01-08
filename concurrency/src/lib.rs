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

// Re-export commonly used types at the crate root
pub use link::{MonitorRef, SystemMessage};
pub use pid::{ExitReason, HasPid, Pid};
pub use process_table::LinkError;
pub use registry::RegistryError;
pub use supervisor::{
    BoxedChildHandle, ChildHandle, ChildInfo, ChildSpec, ChildType, RestartStrategy, RestartType,
    Shutdown, Supervisor, SupervisorCall, SupervisorCast, SupervisorCounts, SupervisorError,
    SupervisorResponse, SupervisorSpec,
};
