#![allow(unused_imports)]
use spawned_concurrency::protocol;
#[protocol]
trait Invalid {
    fn put<T: Send + 'static>(&self, value: T);
}
fn main() {}
