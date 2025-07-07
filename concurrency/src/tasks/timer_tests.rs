use crate::tasks::{send_interval, CallResponse, CastResponse, GenServer, GenServerHandle};
use spawned_rt::tasks::{self as rt, CancellationToken};
use std::time::Duration;

use super::send_after;

type RepeaterHandle = GenServerHandle<Repeater>;

#[derive(Clone)]
struct RepeaterState {
    pub(crate) count: i32,
    pub(crate) cancellation_token: Option<CancellationToken>,
}

#[derive(Clone)]
enum RepeaterCastMessage {
    Inc,
    StopTimer,
}

#[derive(Clone)]
enum RepeaterCallMessage {
    GetCount,
}

#[derive(PartialEq, Debug)]
enum RepeaterOutMessage {
    Count(i32),
}

struct Repeater;

impl Repeater {
    pub async fn stop_timer(server: &mut RepeaterHandle) -> Result<(), ()> {
        server
            .cast(RepeaterCastMessage::StopTimer)
            .await
            .map_err(|_| ())
    }

    pub async fn get_count(server: &mut RepeaterHandle) -> Result<RepeaterOutMessage, ()> {
        server
            .call(RepeaterCallMessage::GetCount)
            .await
            .map_err(|_| ())
    }
}

impl GenServer for Repeater {
    type CallMsg = RepeaterCallMessage;
    type CastMsg = RepeaterCastMessage;
    type OutMsg = RepeaterOutMessage;
    type State = RepeaterState;
    type Error = ();

    fn new() -> Self {
        Self
    }

    async fn init(
        &mut self,
        handle: &RepeaterHandle,
        mut state: Self::State,
    ) -> Result<Self::State, Self::Error> {
        let timer = send_interval(
            Duration::from_millis(100),
            handle.clone(),
            RepeaterCastMessage::Inc,
        );
        state.cancellation_token = Some(timer.cancellation_token);
        Ok(state)
    }

    async fn handle_call(
        &mut self,
        _message: Self::CallMsg,
        _handle: &RepeaterHandle,
        state: Self::State,
    ) -> CallResponse<Self> {
        let count = state.count;
        CallResponse::Reply(state, RepeaterOutMessage::Count(count))
    }

    async fn handle_cast(
        &mut self,
        message: Self::CastMsg,
        _handle: &GenServerHandle<Self>,
        mut state: Self::State,
    ) -> CastResponse<Self> {
        match message {
            RepeaterCastMessage::Inc => {
                state.count += 1;
            }
            RepeaterCastMessage::StopTimer => {
                if let Some(ct) = state.cancellation_token.clone() {
                    ct.cancel()
                };
            }
        };
        CastResponse::NoReply(state)
    }
}

#[test]
pub fn test_send_interval_and_cancellation() {
    let runtime = rt::Runtime::new().unwrap();
    runtime.block_on(async move {
        // Start a Repeater
        let mut repeater = Repeater::start(RepeaterState {
            count: 0,
            cancellation_token: None,
        });

        // Wait for 1 second
        rt::sleep(Duration::from_secs(1)).await;

        // Check count
        let count = Repeater::get_count(&mut repeater).await.unwrap();

        // 9 messages in 1 second (after first 100 milliseconds sleep)
        assert_eq!(RepeaterOutMessage::Count(9), count);

        // Pause timer
        Repeater::stop_timer(&mut repeater).await.unwrap();

        // Wait another second
        rt::sleep(Duration::from_secs(1)).await;

        // Check count again
        let count2 = Repeater::get_count(&mut repeater).await.unwrap();

        // As timer was paused, count should remain at 9
        assert_eq!(RepeaterOutMessage::Count(9), count2);
    });
}

type DelayedHandle = GenServerHandle<Delayed>;

#[derive(Clone)]
struct DelayedState {
    pub(crate) count: i32,
}

#[derive(Clone)]
enum DelayedCastMessage {
    Inc,
}

#[derive(Clone)]
enum DelayedCallMessage {
    GetCount,
}

#[derive(PartialEq, Debug)]
enum DelayedOutMessage {
    Count(i32),
}

struct Delayed;

impl Delayed {
    pub async fn get_count(server: &mut DelayedHandle) -> Result<DelayedOutMessage, ()> {
        server
            .call(DelayedCallMessage::GetCount)
            .await
            .map_err(|_| ())
    }
}

impl GenServer for Delayed {
    type CallMsg = DelayedCallMessage;
    type CastMsg = DelayedCastMessage;
    type OutMsg = DelayedOutMessage;
    type State = DelayedState;
    type Error = ();

    fn new() -> Self {
        Self
    }

    async fn handle_call(
        &mut self,
        _message: Self::CallMsg,
        _handle: &DelayedHandle,
        state: Self::State,
    ) -> CallResponse<Self> {
        let count = state.count;
        CallResponse::Reply(state, DelayedOutMessage::Count(count))
    }

    async fn handle_cast(
        &mut self,
        message: Self::CastMsg,
        _handle: &DelayedHandle,
        mut state: Self::State,
    ) -> CastResponse<Self> {
        match message {
            DelayedCastMessage::Inc => {
                state.count += 1;
            }
        };
        CastResponse::NoReply(state)
    }
}

#[test]
pub fn test_send_after_and_cancellation() {
    let runtime = rt::Runtime::new().unwrap();
    runtime.block_on(async move {
        // Start a Delayed
        let mut repeater = Delayed::start(DelayedState { count: 0 });

        // Set a just once timed message
        let _ = send_after(
            Duration::from_millis(100),
            repeater.clone(),
            DelayedCastMessage::Inc,
        );

        // Wait for 200 milliseconds
        rt::sleep(Duration::from_millis(200)).await;

        // Check count
        let count = Delayed::get_count(&mut repeater).await.unwrap();

        // Only one message (no repetition)
        assert_eq!(DelayedOutMessage::Count(1), count);

        // New timer
        let timer = send_after(
            Duration::from_millis(100),
            repeater.clone(),
            DelayedCastMessage::Inc,
        );

        // Cancel the new timer before timeout
        timer.cancellation_token.cancel();

        // Wait another 200 milliseconds
        rt::sleep(Duration::from_millis(200)).await;

        // Check count again
        let count2 = Delayed::get_count(&mut repeater).await.unwrap();

        // As timer was cancelled, count should remain at 1
        assert_eq!(DelayedOutMessage::Count(1), count2);
    });
}

#[test]
pub fn test_send_after_gen_server_teardown() {
    let runtime = rt::Runtime::new().unwrap();
    runtime.block_on(async move {
        // Start a Delayed
        let mut repeater = Delayed::start(DelayedState { count: 0 });

        // Set a just once timed message
        let _ = send_after(
            Duration::from_millis(100),
            repeater.clone(),
            DelayedCastMessage::Inc,
        );

        // Wait for 200 milliseconds
        rt::sleep(Duration::from_millis(200)).await;

        // Check count
        let count = Delayed::get_count(&mut repeater).await.unwrap();

        // Only one message (no repetition)
        assert_eq!(DelayedOutMessage::Count(1), count);

        // New timer
        let _ = send_after(
            Duration::from_millis(100),
            repeater.clone(),
            DelayedCastMessage::Inc,
        );

        // Cancel the new timer before timeout
        repeater.teardown();

        // Wait another 200 milliseconds
        rt::sleep(Duration::from_millis(200)).await;

        // Check count again
        let count2 = Delayed::get_count(&mut repeater).await.unwrap();

        // As timer was cancelled, count should remain at 1
        assert_eq!(DelayedOutMessage::Count(1), count2);
    });
}
