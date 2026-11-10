#![allow(unused_imports)]
use spawned_concurrency::{protocol, Response};
#[protocol(timeout = 30)]
trait Invalid: Send + Sync {
    fn read(&self) -> Response<u64>;
}
fn main() {}
