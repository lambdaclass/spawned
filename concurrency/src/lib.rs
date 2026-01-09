//! spawned concurrency
//! Some basic traits and structs to implement concurrent code à-la-Erlang.
pub mod error;
mod gen_server;
pub mod link;
pub mod pid;
mod process;
pub mod process_table;
pub mod registry;
mod stream;
pub mod supervisor;
mod time;

#[cfg(test)]
mod gen_server_tests;
#[cfg(test)]
mod stream_tests;
#[cfg(test)]
mod supervisor_tests;
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
    BoxedChildHandle, ChildHandle, ChildSpec, ChildType, DynamicSupervisor,
    DynamicSupervisorError, DynamicSupervisorSpec, RestartStrategy, RestartType, Shutdown,
    Supervisor, SupervisorError, SupervisorSpec,
};
pub use time::{send_after, send_interval};
