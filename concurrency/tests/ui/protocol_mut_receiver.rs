#![allow(unused_imports)]
use spawned_concurrency::protocol;
#[protocol]
trait Invalid {
    fn set(&mut self, value: u8);
}
fn main() {}
