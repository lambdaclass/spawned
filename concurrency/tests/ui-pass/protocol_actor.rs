use spawned_concurrency::{actor, protocol, Response};
use spawned_concurrency::tasks::{Actor, Context, Handler, ActorStart};

#[protocol]
pub trait CounterProtocol: Send + Sync {
    fn read(&self) -> Response<u64>;
}
use counter_protocol::Read;
struct Counter;
#[actor(protocol = CounterProtocol)]
impl Counter {
    #[request_handler]
    async fn handle_read(&mut self, _: Read, _: &Context<Self>) -> u64 { 42 }
}
fn main() {
    spawned_rt::tasks::run(async {
        let actor = Counter.start();
        let interface: CounterRef = actor.to_counter_ref();
        assert_eq!(interface.read().await.unwrap(), 42);
        actor.child_handle().stop();
        actor.join().await;
    });
}
