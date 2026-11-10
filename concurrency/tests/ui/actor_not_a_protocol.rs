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
#[actor(protocol = Counter)]
impl Counter {
    #[request_handler]
    fn read(&mut self, _msg: Read, _ctx: &Context<Self>) -> u64 { self.0 }
}
fn main() {}
