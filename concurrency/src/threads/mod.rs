//! spawned concurrency
//! IO threads-based traits and structs to implement concurrent code à-la-Erlang.

mod actor;
mod process;
mod stream;
mod time;

#[cfg(test)]
mod timer_tests;

pub use actor::{
    send_message_on, Actor, ActorInMsg, ActorRef, InitResult, InitResult::NoSuccess,
    InitResult::Success, MessageResponse, RequestResponse,
};
pub use process::{send, Process, ProcessInfo};
pub use stream::spawn_listener;
pub use time::{send_after, send_interval};
