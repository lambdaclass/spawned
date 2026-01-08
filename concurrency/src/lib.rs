//! spawned concurrency
//! Runtime traits and structs to implement concurrent code à-la-Erlang.

pub mod error;
mod gen_server;
pub mod messages;
mod process;
mod stream;
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
pub use process::{send, Process, ProcessInfo};
pub use stream::spawn_listener;
pub use time::{send_after, send_interval};
