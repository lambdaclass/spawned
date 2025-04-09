//! λ-kit concurrency
//! Some basic traits and structs to implement À-la-Erlang concurrent code.

mod error;
mod gen_server;
mod process;

pub use error::GenServerError;
pub use gen_server::{CallResponse, CastResponse, GenServer, GenServerHandle, GenServerInMsg};
pub use process::{Process, ProcessInfo, send};
