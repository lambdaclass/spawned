#![allow(unused_imports)]
use spawned_concurrency::actor;
use spawned_concurrency::threads::{Actor, Context, Handler};
pub struct Counter;
#[actor(protocol)]
impl Counter {}
fn main() {}
