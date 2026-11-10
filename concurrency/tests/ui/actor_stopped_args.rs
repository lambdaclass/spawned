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
    fn on_stop(&mut self, _ctx: &Context<Self>, _extra: u8) {}
}
fn main() {}
