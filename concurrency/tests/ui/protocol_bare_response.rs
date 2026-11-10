#![allow(unused_imports)]
use spawned_concurrency::{protocol, Response};
#[protocol]
trait Invalid {
    fn read(&self) -> Response;
}
fn main() {}
