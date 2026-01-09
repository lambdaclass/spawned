use spawned_rt::tasks as rt;
use std::time::Duration;
use std::{process::exit, thread};

use spawned_concurrency::tasks::{
    RequestResult, MessageResult, Actor, ActorRef, send_after,
};

// We test a scenario with a badly behaved task
struct BadlyBehavedTask;

impl BadlyBehavedTask {
    pub fn new() -> Self {
        BadlyBehavedTask
    }
}

#[derive(Clone)]
pub enum InMessage {
    GetCount,
    Stop,
}

#[derive(Clone)]
pub enum Reply {
    Count(u64),
}

impl Actor for BadlyBehavedTask {
    type Request = InMessage;
    type Message = ();
    type Reply = ();
    type Error = ();

    async fn handle_request(
        &mut self,
        _: Self::Request,
        _: &ActorRef<Self>,
    ) -> RequestResult<Self> {
        RequestResult::Stop(())
    }

    async fn handle_message(&mut self, _: Self::Message, _: &ActorRef<Self>) -> MessageResult {
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

impl Actor for WellBehavedTask {
    type Request = InMessage;
    type Message = ();
    type Reply = Reply;
    type Error = ();

    async fn handle_request(
        &mut self,
        message: Self::Request,
        _: &ActorRef<Self>,
    ) -> RequestResult<Self> {
        match message {
            InMessage::GetCount => {
                let count = self.count;
                RequestResult::Reply(Reply::Count(count))
            }
            InMessage::Stop => RequestResult::Stop(Reply::Count(self.count)),
        }
    }

    async fn handle_message(
        &mut self,
        _: Self::Message,
        handle: &ActorRef<Self>,
    ) -> MessageResult {
        self.count += 1;
        println!("{:?}: good still alive", thread::current().id());
        send_after(Duration::from_millis(100), handle.to_owned(), ());
        MessageResult::NoReply
    }
}

/// Example of start_blocking to fix issues #8 https://github.com/lambdaclass/spawned/issues/8
/// Tasks that block can block the entire tokio runtime (and other cooperative multitasking models)
/// To fix this we implement start_blocking, which under the hood launches a new thread to deal with the issue
pub fn main() {
    rt::run(async move {
        // If we change BadlyBehavedTask to start instead, it can stop the entire program
        let mut badboy = BadlyBehavedTask::new().start_on_thread();
        let _ = badboy.cast(()).await;
        let mut goodboy = WellBehavedTask::new(0).start();
        let _ = goodboy.cast(()).await;
        rt::sleep(Duration::from_secs(1)).await;
        let count = goodboy.call(InMessage::GetCount).await.unwrap();

        match count {
            Reply::Count(num) => {
                assert!(num == 10);
            }
        }

        goodboy.call(InMessage::Stop).await.unwrap();
        exit(0);
    })
}
