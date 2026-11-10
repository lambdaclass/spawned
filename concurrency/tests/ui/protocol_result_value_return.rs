#![allow(unused_imports)]
use spawned_concurrency::{protocol, ActorError};
#[protocol]
trait Invalid {
    fn read(&self) -> Result<u64, ActorError>;
}
fn main() {}
