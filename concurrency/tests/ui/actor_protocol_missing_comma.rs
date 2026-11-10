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
#[protocol]
pub trait ResetProtocol: Send + Sync {
    fn reset(&self);
}
#[actor(protocol = CounterProtocol protocol = ResetProtocol)]
impl Counter {
    #[request_handler]
    fn read(&mut self, _: Read, _: &Context<Self>) -> u64 { self.0 }
    #[send_handler]
    fn bump(&mut self, msg: Bump, _: &Context<Self>) { self.0 += msg.by; }
    #[send_handler]
    fn reset(&mut self, _: reset_protocol::Reset, _: &Context<Self>) { self.0 = 0; }
}
fn main() {}
