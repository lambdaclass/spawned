#![allow(unused_imports)]
use spawned_concurrency::protocol;
#[protocol]
trait Invalid<T> {
    fn put(&self, value: T);
}
fn main() {}
