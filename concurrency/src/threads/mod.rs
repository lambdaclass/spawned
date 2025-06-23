//! spawned concurrency
//! IO threads-based traits and structs to implement concurrent code à-la-Erlang.

mod error;
mod gen_server;
mod process;
mod time;

pub use gen_server::{CallResponse, CastResponse, GenServer, GenServerHandle, GenServerInMsg};
pub use process::{send, Process, ProcessInfo};
pub use time::{send_after, send_interval};
