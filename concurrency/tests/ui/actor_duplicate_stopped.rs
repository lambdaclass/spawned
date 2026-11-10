#![allow(unused_imports)]
use spawned_concurrency::{actor, protocol, Response};
#[protocol]
pub trait CounterProtocol: Send + Sync {
    fn read(&self) -> Response<u64>;
    fn bump(&self, by: u64);
}
use counter_protocol::{Bump, Read};
pub struct Counter(u64);
use spawned_concurrency::threads::{Actor, Context, Handler};
#[actor]
impl Counter {
    #[stopped]
    fn first(&mut self, _ctx: &Context<Self>) {}
    #[stopped]
    fn second(&mut self, _ctx: &Context<Self>) {}
}
fn main() {}
