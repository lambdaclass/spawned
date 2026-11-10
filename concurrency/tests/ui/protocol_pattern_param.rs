#![allow(unused_imports)]
use spawned_concurrency::protocol;
#[protocol]
trait Invalid {
    fn pair(&self, (left, right): (u8, u8));
}
fn main() {}
