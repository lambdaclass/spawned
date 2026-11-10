#![allow(unused_imports)]
use spawned_concurrency::{protocol, Response};
use std::time::Duration;
#[protocol]
trait Invalid: Send + Sync {
    #[request_timeout(Duration::from_secs(-1))]
    fn read(&self) -> Response<u64>;
}
fn main() {}
