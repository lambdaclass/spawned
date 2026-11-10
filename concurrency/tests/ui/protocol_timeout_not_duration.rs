#![allow(unused_imports)]
use spawned_concurrency::{protocol, Response};
#[protocol]
trait Invalid: Send + Sync {
    #[request_timeout(5)]
    fn read(&self) -> Response<u64>;
}
fn main() {}
