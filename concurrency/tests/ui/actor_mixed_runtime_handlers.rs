#![allow(unused_imports)]
use spawned_concurrency::{actor, protocol, Response};
#[protocol]
pub trait CounterProtocol: Send + Sync {
    fn read(&self) -> Response<u64>;
    fn bump(&self, by: u64);
}
use counter_protocol::{Bump, Read};
pub struct Counter(u64);
use spawned_concurrency::tasks::{Actor, Context, Handler};
#[actor(protocol = CounterProtocol)]
impl Counter {
    #[request_handler]
    async fn read(&mut self, _msg: Read, _ctx: &Context<Self>) -> u64 { self.0 }
    #[send_handler]
    fn bump(&mut self, msg: Bump, _ctx: &Context<Self>) { self.0 += msg.by; }
}
fn main() {}
