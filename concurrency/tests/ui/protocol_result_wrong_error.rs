#![allow(unused_imports)]
use spawned_concurrency::protocol;
#[protocol]
trait Invalid: Send + Sync {
    fn notify(&self) -> Result<(), String>;
}
fn main() {}
