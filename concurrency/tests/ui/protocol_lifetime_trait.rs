#![allow(unused_imports)]
use spawned_concurrency::protocol;
#[protocol]
trait Invalid<'a> {
    fn put(&self, value: &'a str);
}
fn main() {}
