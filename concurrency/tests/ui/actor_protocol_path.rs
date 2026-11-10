#![allow(unused_imports)]
mod api {
    use spawned_concurrency::{protocol, Response};
    #[protocol]
    pub trait CounterProtocol: Send + Sync {
        fn read(&self) -> Response<u64>;
    }
}
use spawned_concurrency::actor;
use spawned_concurrency::threads::{Actor, Context, Handler};
pub struct Counter;
#[actor(protocol = api::CounterProtocol)]
impl Counter {
    #[request_handler]
    fn read(&mut self, _: api::counter_protocol::Read, _: &Context<Self>) -> u64 { 0 }
}
fn main() {}
