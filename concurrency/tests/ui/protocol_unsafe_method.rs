#![allow(unused_imports)]
use spawned_concurrency::protocol;
#[protocol]
trait Invalid: Send + Sync {
    unsafe fn ping(&self);
}
fn main() {}
