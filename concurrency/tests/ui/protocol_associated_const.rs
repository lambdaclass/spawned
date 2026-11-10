#![allow(unused_imports)]
use spawned_concurrency::protocol;
#[protocol]
trait Invalid {
    const LIMIT: usize;
    fn ping(&self);
}
fn main() {}
