use spawned_rt::tasks as rt;
use std::time::Duration;
use std::{process::exit, thread};

use spawned_concurrency::messages;
use spawned_concurrency::tasks::{Actor, ActorStart, Backend, Context, Handler, send_after};

messages! {
    GetCount -> u64;
    StopActor -> u64;
    BadWork -> ();
    GoodWork -> ()
}

struct BadlyBehavedTask;

impl BadlyBehavedTask {
    pub fn new() -> Self {
        BadlyBehavedTask
    }
}

impl Actor for BadlyBehavedTask {}

impl Handler<StopActor> for BadlyBehavedTask {
    async fn handle(&mut self, _: StopActor, ctx: &Context<Self>) -> u64 {
        ctx.stop();
        0
    }
}

impl Handler<BadWork> for BadlyBehavedTask {
    async fn handle(&mut self, _: BadWork, _ctx: &Context<Self>) {
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

impl WellBehavedTask {
    pub fn new(initial_count: u64) -> Self {
        WellBehavedTask {
            count: initial_count,
        }
    }
}

impl Actor for WellBehavedTask {}

impl Handler<GetCount> for WellBehavedTask {
    async fn handle(&mut self, _: GetCount, _ctx: &Context<Self>) -> u64 {
        self.count
    }
}

impl Handler<StopActor> for WellBehavedTask {
    async fn handle(&mut self, _: StopActor, ctx: &Context<Self>) -> u64 {
        ctx.stop();
        self.count
    }
}

impl Handler<GoodWork> for WellBehavedTask {
    async fn handle(&mut self, _: GoodWork, ctx: &Context<Self>) {
        self.count += 1;
        println!("{:?}: good still alive", thread::current().id());
        send_after(Duration::from_millis(100), ctx.clone(), GoodWork);
    }
}

/// Example of Backend::Thread to fix issues #8 https://github.com/lambdaclass/spawned/issues/8
/// Tasks that block can block the entire tokio runtime (and other cooperative multitasking models)
/// To fix this we use Backend::Thread, which under the hood launches a new thread to deal with the issue
pub fn main() {
    rt::run(async move {
        // If we change BadlyBehavedTask to Backend::Async instead, it can stop the entire program
        let badboy = BadlyBehavedTask::new().start_with_backend(Backend::Thread);
        let _ = badboy.send(BadWork);
        let goodboy = WellBehavedTask::new(0).start();
        let _ = goodboy.send(GoodWork);
        rt::sleep(Duration::from_secs(1)).await;
        let count = goodboy.request(GetCount).await.unwrap();

        assert!(count == 10);

        goodboy.request(StopActor).await.unwrap();
        exit(0);
    })
}
