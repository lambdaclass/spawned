use spawned_rt::tasks as rt;
use std::time::Duration;
use std::{process::exit, thread};

use spawned_concurrency::tasks::{
    Actor, ActorRef, Backend, MessageResponse, RequestResponse, send_after,
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
pub enum OutMsg {
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
    ) -> RequestResponse<Self> {
        RequestResponse::Stop(())
    }

    async fn handle_message(&mut self, _: Self::Message, _: &ActorRef<Self>) -> MessageResponse {
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
    type Reply = OutMsg;
    type Error = ();

    async fn handle_request(
        &mut self,
        message: Self::Request,
        _: &ActorRef<Self>,
    ) -> RequestResponse<Self> {
        match message {
            InMessage::GetCount => {
                let count = self.count;
                RequestResponse::Reply(OutMsg::Count(count))
            }
            InMessage::Stop => RequestResponse::Stop(OutMsg::Count(self.count)),
        }
    }

    async fn handle_message(
        &mut self,
        _: Self::Message,
        handle: &ActorRef<Self>,
    ) -> MessageResponse {
        self.count += 1;
        println!("{:?}: good still alive", thread::current().id());
        send_after(Duration::from_millis(100), handle.to_owned(), ());
        MessageResponse::NoReply
    }
}

/// Example of Backend::Thread to fix issues #8 https://github.com/lambdaclass/spawned/issues/8
/// Tasks that block can block the entire tokio runtime (and other cooperative multitasking models)
/// To fix this we use Backend::Thread, which under the hood launches a new thread to deal with the issue
pub fn main() {
    rt::run(async move {
        // If we change BadlyBehavedTask to Backend::Async instead, it can stop the entire program
        let mut badboy = BadlyBehavedTask::new().start_with_backend(Backend::Thread);
        let _ = badboy.send(()).await;
        let mut goodboy = WellBehavedTask::new(0).start();
        let _ = goodboy.send(()).await;
        rt::sleep(Duration::from_secs(1)).await;
        let count = goodboy.request(InMessage::GetCount).await.unwrap();

        match count {
            OutMsg::Count(num) => {
                assert!(num == 10);
            }
        }

        goodboy.request(InMessage::Stop).await.unwrap();
        exit(0);
    })
}
