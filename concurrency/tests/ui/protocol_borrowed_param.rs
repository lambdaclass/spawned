#![allow(unused_imports)]
use spawned_concurrency::protocol;
#[protocol]
trait Invalid {
    fn rename(&self, name: &str);
}
fn main() {}
