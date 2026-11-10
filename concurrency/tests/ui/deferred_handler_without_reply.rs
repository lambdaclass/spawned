#![allow(unused_imports)]
use spawned_concurrency::{actor, protocol, Response};
#[protocol]
pub trait CounterProtocol: Send + Sync {
    fn read(&self) -> Response<u64>;
}
use counter_protocol::Read;
pub struct Counter(u64);
use spawned_concurrency::threads::{Actor, Context, Handler};
#[actor]
impl Counter {
    #[deferred_handler]
    fn read(&mut self, _msg: Read, _ctx: &Context<Self>) {}
}
fn main() {}
