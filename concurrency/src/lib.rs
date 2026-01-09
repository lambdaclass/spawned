//! spawned concurrency
//! Runtime traits and structs to implement concurrent code à-la-Erlang.

pub mod application;
pub mod error;
mod gen_server;
pub mod link;
pub mod messages;
pub mod pg;
pub mod pid;
mod process;
pub mod process_table;
pub mod registry;
mod stream;
pub mod supervisor;
pub mod sys;
mod time;

#[cfg(test)]
mod stream_tests;
#[cfg(test)]
mod timer_tests;

pub use error::{Error, GenServerError};
pub use gen_server::{
    get_default_backend, send_message_on, set_default_backend, Backend, CallResponse, CastResponse,
    GenServer, GenServerHandle, GenServerInMsg, InitResult, InitResult::NoSuccess,
    InitResult::Success,
};
pub use link::{MonitorRef, SystemMessage};
pub use pg::PgError;
pub use pid::{ExitReason, HasPid, Pid};
pub use process::{send, Process, ProcessInfo as SpawnInfo};
pub use process_table::{LinkError, ProcessInfo};
pub use registry::RegistryError;
pub use stream::spawn_listener;
pub use supervisor::{
    BoxedChildHandle, ChildHandle, ChildInfo, ChildSpec, ChildType, DynamicSupervisor,
    DynamicSupervisorCall, DynamicSupervisorCast, DynamicSupervisorError, DynamicSupervisorResponse,
    DynamicSupervisorSpec, RestartStrategy, RestartType, Shutdown, Supervisor, SupervisorCall,
    SupervisorCast, SupervisorCounts, SupervisorError, SupervisorResponse, SupervisorSpec,
};
pub use time::{send_after, send_interval};
