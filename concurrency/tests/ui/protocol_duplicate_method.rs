#![allow(unused_imports)]
use spawned_concurrency::protocol;
#[protocol]
trait Invalid {
    fn ping(&self);
    fn ping(&self, value: u8);
}
fn main() {}
