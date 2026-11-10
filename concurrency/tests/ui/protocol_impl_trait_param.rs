#![allow(unused_imports)]
use spawned_concurrency::protocol;
#[protocol]
trait Invalid {
    fn show(&self, value: impl std::fmt::Display);
}
fn main() {}
