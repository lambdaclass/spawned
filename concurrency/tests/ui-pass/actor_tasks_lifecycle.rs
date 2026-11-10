// Async actor without a protocol: hand-written message, async lifecycle hooks,
// and an empty `#[actor]` block (no handlers, inferred native runtime).
use spawned_concurrency::message::Message;
use spawned_concurrency::{actor, Exit};

pub struct Ping(u32);
impl Message for Ping {
    type Result = u32;
}

mod asynchronous {
    use super::*;
    use spawned_concurrency::tasks::{Actor, Context, Handler};
    pub struct Echo;
    #[actor]
    impl Echo {
        #[started]
        async fn boot(&mut self, _ctx: &Context<Self>) {}
        #[stopped]
        async fn halt(&mut self, _ctx: &Context<Self>) {}
        #[exit_received]
        async fn trapped(&mut self, _exit: Exit, _ctx: &Context<Self>) {}
        #[handler]
        async fn ping(&mut self, msg: Ping, _ctx: &Context<Self>) -> u32 {
            msg.0 + 1
        }
    }
}

mod native {
    use spawned_concurrency::actor;
    use spawned_concurrency::threads::{Actor, Context, Handler};
    pub struct Idle;
    #[actor]
    impl Idle {}
}

fn main() {
    use spawned_concurrency::tasks::ActorStart as _;
    use spawned_concurrency::threads::ActorStart as _;
    spawned_rt::tasks::run(async {
        let echo = asynchronous::Echo.start();
        assert_eq!(echo.request(Ping(1)).await.unwrap(), 2);
        echo.child_handle().stop();
        echo.join().await;
    });
    let idle = native::Idle.start();
    idle.context().stop();
    idle.join();
}
