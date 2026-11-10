// Methods with the same name under mutually exclusive cfg predicates are
// valid; only unconditional duplicates are rejected.
use spawned_concurrency::threads::{Actor, ActorStart, Context, Handler};
use spawned_concurrency::{actor, protocol, Response};

#[protocol]
pub trait Variant: Send + Sync {
    #[cfg(all())]
    fn get(&self) -> Response<u8>;
    #[cfg(any())]
    fn get(&self, value: u8) -> Response<u8>;
}

pub struct Fixed;

#[actor(protocol = Variant)]
impl Fixed {
    #[request_handler]
    fn get(&mut self, _: variant::Get, _: &Context<Self>) -> u8 {
        3
    }
}

fn main() {
    let actor = Fixed.start();
    assert_eq!(Variant::get(&actor).unwrap(), 3);
    actor.context().stop();
    actor.join();
}
