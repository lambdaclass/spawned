//! spawned concurrency
//! IO threads-based traits and structs to implement concurrent code à-la-Erlang.

mod gen_server;
mod process;
mod stream;
mod time;

#[cfg(test)]
mod timer_tests;

pub use gen_server::{
    CallResponse, CastResponse, GenServer, GenServerHandle, GenServerInMsg, InfoResponse,
    InitResult,
};
pub use process::{send, Process, ProcessInfo};
pub use stream::spawn_listener;
pub use time::{send_after, send_interval};

// Re-export Pid and link types for convenience
pub use crate::link::{MonitorRef, SystemMessage};
pub use crate::pid::{ExitReason, HasPid, Pid};
pub use crate::process_table::LinkError;
