//! λ-kit concurrency
//! Some basic traits and structs to implement À-la-Erlang concurrent code.

mod error;
mod gen_server;
mod process;
mod time;

pub use gen_server::{CallResponse, CastResponse, GenServer, GenServerHandle, GenServerInMsg};
pub use process::{send, Process, ProcessInfo};
pub use time::send_after;
