#![allow(unused_imports)]
use spawned_concurrency::protocol;
#[protocol]
trait Invalid: Send + Sync {
    fn read_value(&self);
    fn read__value(&self, value: u8);
}
fn main() {}
