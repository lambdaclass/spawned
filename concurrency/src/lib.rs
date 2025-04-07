//! λ-kit concurrency
//! Some basic traits and structs to implement À-la-Erlang concurrent code.


mod process;
mod gen_server;

pub use process::{Process, ProcessInfo, send};
pub use gen_server::{GenServer, GenServerInMsg, GenServerHandle, CallResponse, CastResponse};