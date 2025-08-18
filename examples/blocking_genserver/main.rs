use spawned_rt::tasks as rt;
use std::time::Duration;
use std::{process::exit, thread};

use spawned_concurrency::tasks::{
    CallResponse, CastResponse, GenServer, GenServerHandle, send_after,
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

impl GenServer for BadlyBehavedTask {
    type CallMsg = InMessage;
    type CastMsg = ();
    type OutMsg = ();
    type Error = ();

    async fn handle_call(
        &mut self,
        _: Self::CallMsg,
        _: &GenServerHandle<Self>,
    ) -> CallResponse<Self> {
        CallResponse::Stop(())
    }

    async fn handle_cast(&mut self, _: Self::CastMsg, _: &GenServerHandle<Self>) -> CastResponse {
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

impl GenServer for WellBehavedTask {
    type CallMsg = InMessage;
    type CastMsg = ();
    type OutMsg = OutMsg;
    type Error = ();

    async fn handle_call(
        &mut self,
        message: Self::CallMsg,
        _: &GenServerHandle<Self>,
    ) -> CallResponse<Self> {
        match message {
            InMessage::GetCount => {
                let count = self.count;
                CallResponse::Reply(OutMsg::Count(count))
            }
            InMessage::Stop => CallResponse::Stop(OutMsg::Count(self.count)),
        }
    }

    async fn handle_cast(
        &mut self,
        _: Self::CastMsg,
        handle: &GenServerHandle<Self>,
    ) -> CastResponse {
        self.count += 1;
        println!("{:?}: good still alive", thread::current().id());
        send_after(Duration::from_millis(100), handle.to_owned(), ());
        CastResponse::NoReply
    }
}

/// Example of start_blocking to fix issues #8 https://github.com/lambdaclass/spawned/issues/8
/// Tasks that block can block the entire tokio runtime (and other cooperative multitasking models)
/// To fix this we implement start_blocking, which under the hood launches a new thread to deal with the issue
pub fn main() {
    rt::run(async move {
        // If we change BadlyBehavedTask to start instead, it can stop the entire program
        let mut badboy = BadlyBehavedTask::new().start_blocking();
        let _ = badboy.cast(()).await;
        let mut goodboy = WellBehavedTask::new(0).start();
        let _ = goodboy.cast(()).await;
        rt::sleep(Duration::from_secs(1)).await;
        let count = goodboy.call(InMessage::GetCount).await.unwrap();

        match count {
            OutMsg::Count(num) => {
                assert!(num == 10);
            }
        }

        goodboy.call(InMessage::Stop).await.unwrap();
        exit(0);
    })
}
