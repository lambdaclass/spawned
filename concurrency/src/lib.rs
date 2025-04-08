//! λ-kit concurrency
//! Some basic traits and structs to implement À-la-Erlang concurrent code.

mod gen_server;
mod process;

pub use gen_server::{
    CallResponse, CastResponse, GenServer, GenServerError, GenServerHandle, GenServerInMsg,
};
pub use process::{Process, ProcessInfo, send};
