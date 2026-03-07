use spawned_concurrency::error::ActorError;
use spawned_concurrency::tasks::{
    send_after, Actor, ActorStart as _, Backend, Context, Handler,
};
use spawned_concurrency::Response;
use spawned_concurrency::{actor, protocol};
use spawned_rt::tasks as rt;
use std::time::Duration;
use std::{process::exit, thread};

#[protocol]
pub trait BadlyBehavedProtocol: Send + Sync {
    fn do_block(&self) -> Result<(), ActorError>;
}

#[protocol]
pub trait WellBehavedProtocol: Send + Sync {
    fn get_count(&self) -> Response<u64>;
    fn stop_actor(&self) -> Response<u64>;
    fn tick(&self) -> Result<(), ActorError>;
}

use badly_behaved_protocol::DoBlock;
use well_behaved_protocol::{GetCount, StopActor, Tick};

// We test a scenario with a badly behaved task
struct BadlyBehavedTask;

#[actor(protocol = BadlyBehavedProtocol)]
impl BadlyBehavedTask {
    #[send_handler]
    async fn handle_do_block(&mut self, _msg: DoBlock, _ctx: &Context<Self>) {
        rt::sleep(Duration::from_millis(20)).await;
        loop {
            tracing::info!("{:?}: bad still alive", thread::current().id());
            thread::sleep(Duration::from_millis(50));
        }
    }
}

struct WellBehavedTask {
    count: u64,
}

#[actor(protocol = WellBehavedProtocol)]
impl WellBehavedTask {
    #[request_handler]
    async fn handle_get_count(&mut self, _msg: GetCount, _ctx: &Context<Self>) -> u64 {
        self.count
    }

    #[request_handler]
    async fn handle_stop_actor(&mut self, _msg: StopActor, ctx: &Context<Self>) -> u64 {
        ctx.stop();
        self.count
    }

    #[send_handler]
    async fn handle_tick(&mut self, _msg: Tick, ctx: &Context<Self>) {
        self.count += 1;
        tracing::info!("{:?}: good still alive", thread::current().id());
        send_after(Duration::from_millis(100), ctx.clone(), Tick);
    }
}

/// Example of Backend::Thread to fix issues #8 https://github.com/lambdaclass/spawned/issues/8
/// Tasks that block can block the entire tokio runtime (and other cooperative multitasking models)
/// To fix this we use Backend::Thread, which under the hood launches a new thread to deal with the issue
pub fn main() {
    rt::run(async move {
        // If we change BadlyBehavedTask to Backend::Async instead, it can stop the entire program
        let badboy = BadlyBehavedTask.start_with_backend(Backend::Thread);
        let _ = badboy.do_block();
        let goodboy = WellBehavedTask { count: 0 }.start();
        let _ = goodboy.tick();
        rt::sleep(Duration::from_secs(1)).await;
        let count = goodboy.get_count().await.unwrap();

        assert!(count == 10);

        goodboy.stop_actor().await.unwrap();
        exit(0);
    })
}
