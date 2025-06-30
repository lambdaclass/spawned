//! spawned concurrency
//! Runtime tasks-based traits and structs to implement concurrent code à-la-Erlang.

mod gen_server;
mod process;
mod time;
mod stream;

#[cfg(test)]
mod timer_tests;

pub use gen_server::{CallResponse, CastResponse, GenServer, GenServerHandle, GenServerInMsg};
pub use process::{send, Process, ProcessInfo};
pub use stream::{spawn_listener};
pub use time::{send_after, send_interval};
