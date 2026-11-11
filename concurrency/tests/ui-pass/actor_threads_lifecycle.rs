// Generic native actor with every lifecycle hook, a `&self` handler, plain
// inherent methods kept in the block, and a cfg-disabled protocol method.
use spawned_concurrency::threads::{Actor, ActorStart, Context, Handler};
use spawned_concurrency::{actor, protocol, Exit, Response};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

#[protocol]
pub trait HolderProtocol: Send + Sync {
    fn get(&self) -> Response<String>;
    #[cfg(any())]
    fn never(&self) -> Response<u8>;
}

pub struct Holder<T> {
    value: T,
    hooks: Arc<AtomicUsize>,
}

// The hook counter is shared with the test on purpose, to observe the hooks.
#[actor(protocol(HolderProtocol), allow_shared_state)]
impl<T: ToString + Send + 'static> Holder<T> {
    pub fn new(value: T, hooks: Arc<AtomicUsize>) -> Self {
        Holder { value, hooks }
    }
    #[started]
    fn boot(&mut self, _ctx: &Context<Self>) {
        self.hooks.fetch_add(1, Ordering::SeqCst);
    }
    #[stopped]
    fn halt(&mut self, _ctx: &Context<Self>) {
        self.hooks.fetch_add(10, Ordering::SeqCst);
    }
    #[exit_received]
    fn trapped(&mut self, _exit: Exit, _ctx: &Context<Self>) {}
    #[request_handler]
    fn get(&self, _: holder_protocol::Get, _: &Context<Self>) -> String {
        self.value.to_string()
    }
    #[cfg(any())]
    #[request_handler]
    fn never(&mut self, _: holder_protocol::Never, _: &Context<Self>) -> u8 {
        0
    }
}

fn main() {
    let hooks = Arc::new(AtomicUsize::new(0));
    let actor = Holder::new(7u32, hooks.clone()).start();
    assert_eq!(actor.get().unwrap(), "7");
    actor.context().stop();
    actor.join();
    assert_eq!(hooks.load(Ordering::SeqCst), 11);
}
