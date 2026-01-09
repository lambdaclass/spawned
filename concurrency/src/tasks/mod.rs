//! spawned concurrency
//! Runtime tasks-based traits and structs to implement concurrent code à-la-Erlang.

mod gen_server;
mod gen_statem;
mod process;
mod stream;
mod time;

#[cfg(test)]
mod stream_tests;
#[cfg(test)]
mod timer_tests;

pub use gen_server::{
    send_message_on, CallResponse, CastResponse, GenServer, GenServerHandle, GenServerInMsg,
    InfoResponse, InitResult, InitResult::NoSuccess, InitResult::Success,
};
pub use gen_statem::{
    Backend as StatemBackend, GenStatem, GenStatemHandle, GetStateEvent, StateResult, StatemError,
    StatemMsg,
};
pub use process::{send, Process, ProcessInfo};
pub use stream::spawn_listener;
pub use time::{send_after, send_interval};

// Re-export Pid, link, and registry types for convenience
pub use crate::link::{MonitorRef, SystemMessage};
pub use crate::pid::{ExitReason, HasPid, Pid};
pub use crate::process_table::LinkError;
pub use crate::registry::{self, RegistryError};

// Re-export supervisor types for convenience
pub use crate::supervisor::{
    BoxedChildHandle, ChildHandle, ChildInfo, ChildSpec, ChildType, DynamicSupervisor,
    DynamicSupervisorCall, DynamicSupervisorCast, DynamicSupervisorError, DynamicSupervisorResponse,
    DynamicSupervisorSpec, RestartStrategy, RestartType, Shutdown, Supervisor, SupervisorCall,
    SupervisorCast, SupervisorCounts, SupervisorError, SupervisorResponse, SupervisorSpec,
};
