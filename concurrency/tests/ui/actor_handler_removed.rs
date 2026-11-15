// `#[handler]` was removed in 0.8: it hid whether a method answers.
use spawned_concurrency::actor;
use spawned_concurrency::message::Message;
use spawned_concurrency::tasks::{Actor, Context, Handler};
pub struct Ping;
impl Message for Ping {
    type Result = u32;
}
pub struct Counter;
#[actor]
impl Counter {
    #[handler]
    async fn ping(&mut self, _msg: Ping, _ctx: &Context<Self>) -> u32 {
        1
    }
}
fn main() {}
