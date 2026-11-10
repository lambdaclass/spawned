#![allow(unused_imports)]
use spawned_concurrency::{protocol, Response};
use std::time::Duration;
#[protocol]
trait Invalid: Send + Sync {
    #[request_timeout(Duration::ZERO, Duration::MAX)]
    fn read(&self) -> Response<u64>;
}
fn main() {}
