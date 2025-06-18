use spawned_rt::tasks as rt;
use std::time::Duration;
use std::{process::exit, thread};

use spawned_concurrency::tasks::{
    CallResponse, CastResponse, GenServer, GenServerHandle, send_after,
};

// We test a scenario with a badly behaved task
struct BadlyBehavedTask;

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
    type State = ();
    type Error = ();

    fn new() -> Self {
        Self {}
    }

    async fn init(
        &mut self,
        _handle: &GenServerHandle<Self>,
        _state: &mut Self::State,
    ) -> Result<(), Self::Error> {
        Ok(())
    }

    async fn handle_call(
        &mut self,
        _: Self::CallMsg,
        _: &GenServerHandle<Self>,
        _: &mut Self::State,
    ) -> CallResponse<Self::OutMsg> {
        CallResponse::Stop(())
    }

    async fn handle_cast(
        &mut self,
        _: Self::CastMsg,
        _: &GenServerHandle<Self>,
        _: &mut Self::State,
    ) -> CastResponse {
        rt::sleep(Duration::from_millis(20)).await;
        loop {
            println!("{:?}: bad still alive", thread::current().id());
            thread::sleep(Duration::from_millis(50));
        }
    }
}

struct WellBehavedTask;

#[derive(Clone)]
struct CountState {
    pub count: u64,
}

impl GenServer for WellBehavedTask {
    type CallMsg = InMessage;
    type CastMsg = ();
    type OutMsg = OutMsg;
    type State = CountState;
    type Error = ();

    fn new() -> Self {
        Self {}
    }

    async fn init(
        &mut self,
        _handle: &GenServerHandle<Self>,
        _state: &mut Self::State,
    ) -> Result<(), Self::Error> {
        Ok(())
    }

    async fn handle_call(
        &mut self,
        message: Self::CallMsg,
        _: &GenServerHandle<Self>,
        state: &mut Self::State,
    ) -> CallResponse<Self::OutMsg> {
        match message {
            InMessage::GetCount => CallResponse::Reply(OutMsg::Count(state.count)),
            InMessage::Stop => CallResponse::Stop(OutMsg::Count(state.count)),
        }
    }

    async fn handle_cast(
        &mut self,
        _: Self::CastMsg,
        handle: &GenServerHandle<Self>,
        state: &mut Self::State,
    ) -> CastResponse {
        state.count += 1;
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
        let mut badboy = BadlyBehavedTask::start_blocking(());
        let _ = badboy.cast(()).await;
        let mut goodboy = WellBehavedTask::start(CountState { count: 0 });
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
