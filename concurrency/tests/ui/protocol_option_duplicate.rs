#![allow(unused_imports)]
use spawned_concurrency::{protocol, Response};
use std::time::Duration;
#[protocol(timeout = Duration::ZERO, timeout = Duration::MAX)]
trait Invalid: Send + Sync {
    fn read(&self) -> Response<u64>;
}
fn main() {}
