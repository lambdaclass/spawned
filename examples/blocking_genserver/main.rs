use spawned_concurrency::message::Message;
use spawned_concurrency::tasks::{
    send_after, Actor, ActorStart as _, Backend, Context, Handler,
};
use spawned_rt::tasks as rt;
use std::time::Duration;
use std::{process::exit, thread};

// We test a scenario with a badly behaved task
struct BadlyBehavedTask;

#[derive(Debug)]
pub struct DoBlock;
impl Message for DoBlock {
    type Result = ();
}

#[derive(Debug)]
pub struct GetCount;
impl Message for GetCount {
    type Result = u64;
}

#[derive(Debug)]
pub struct StopActor;
impl Message for StopActor {
    type Result = u64;
}

#[derive(Debug)]
pub struct Tick;
impl Message for Tick {
    type Result = ();
}

impl Actor for BadlyBehavedTask {}

impl Handler<DoBlock> for BadlyBehavedTask {
    async fn handle(&mut self, _msg: DoBlock, _ctx: &Context<Self>) {
        rt::sleep(Duration::from_millis(20)).await;
        loop {
            println!("{:?}: bad still alive", thread::current().id());
            thread::sleep(Duration::from_millis(50));
        }
    }
}

struct WellBehavedTask {
    count: u64,
}

impl Actor for WellBehavedTask {}

impl Handler<GetCount> for WellBehavedTask {
    async fn handle(&mut self, _msg: GetCount, _ctx: &Context<Self>) -> u64 {
        self.count
    }
}

impl Handler<StopActor> for WellBehavedTask {
    async fn handle(&mut self, _msg: StopActor, ctx: &Context<Self>) -> u64 {
        ctx.stop();
        self.count
    }
}

impl Handler<Tick> for WellBehavedTask {
    async fn handle(&mut self, _msg: Tick, ctx: &Context<Self>) {
        self.count += 1;
        println!("{:?}: good still alive", thread::current().id());
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
        let _ = badboy.send(DoBlock);
        let goodboy = WellBehavedTask { count: 0 }.start();
        let _ = goodboy.send(Tick);
        rt::sleep(Duration::from_secs(1)).await;
        let count = goodboy.request(GetCount).await.unwrap();

        assert!(count == 10);

        goodboy.request(StopActor).await.unwrap();
        exit(0);
    })
}
