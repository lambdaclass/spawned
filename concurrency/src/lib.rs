//! λ-kit concurrency
//! Some basic traits and structs to implement À-la-Erlang concurrent code.

mod r#async;

pub use r#async::error::GenServerError;
pub use r#async::gen_server::{CallResponse, CastResponse, GenServer, GenServerHandle, GenServerInMsg};
pub use r#async::process::{Process, ProcessInfo, send};
pub use r#async::time::send_after;
