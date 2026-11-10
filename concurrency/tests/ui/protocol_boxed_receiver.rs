#![allow(unused_imports)]
use spawned_concurrency::protocol;
#[protocol]
trait Invalid {
    fn consume(self: Box<Self>);
}
fn main() {}
