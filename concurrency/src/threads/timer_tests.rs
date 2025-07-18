use crate::threads::{send_interval, CallResponse, CastResponse, GenServer, GenServerHandle};
use spawned_rt::threads::{self as rt, CancellationToken};
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

#[derive(Default)]
struct Repeater;

impl Repeater {
    pub fn stop_timer(server: &mut RepeaterHandle) -> Result<(), ()> {
        server.cast(RepeaterCastMessage::StopTimer).map_err(|_| ())
    }

    pub fn get_count(server: &mut RepeaterHandle) -> Result<RepeaterOutMessage, ()> {
        server.call(RepeaterCallMessage::GetCount).map_err(|_| ())
    }
}

impl GenServer for Repeater {
    type CallMsg = RepeaterCallMessage;
    type CastMsg = RepeaterCastMessage;
    type OutMsg = RepeaterOutMessage;
    type State = RepeaterState;
    type Error = ();

    fn init(
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

    fn handle_call(
        &mut self,
        _message: Self::CallMsg,
        _handle: &RepeaterHandle,
        state: Self::State,
    ) -> CallResponse<Self> {
        let count = state.count;
        CallResponse::Reply(state, RepeaterOutMessage::Count(count))
    }

    fn handle_cast(
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
                if let Some(mut ct) = state.cancellation_token.clone() {
                    ct.cancel()
                };
            }
        };
        CastResponse::NoReply(state)
    }
}

#[test]
pub fn test_send_interval_and_cancellation() {
    // Start a Repeater
    let mut repeater = Repeater::start(RepeaterState {
        count: 0,
        cancellation_token: None,
    });

    // Wait for 1 second
    rt::sleep(Duration::from_secs(1));

    // Check count
    let count = Repeater::get_count(&mut repeater).unwrap();

    // 9 messages in 1 second (after first 100 milliseconds sleep)
    assert_eq!(RepeaterOutMessage::Count(9), count);

    // Pause timer
    Repeater::stop_timer(&mut repeater).unwrap();

    // Wait another second
    rt::sleep(Duration::from_secs(1));

    // Check count again
    let count2 = Repeater::get_count(&mut repeater).unwrap();

    // As timer was paused, count should remain at 9
    assert_eq!(RepeaterOutMessage::Count(9), count2);
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

#[derive(Default)]
struct Delayed;

impl Delayed {
    pub fn get_count(server: &mut DelayedHandle) -> Result<DelayedOutMessage, ()> {
        server.call(DelayedCallMessage::GetCount).map_err(|_| ())
    }
}

impl GenServer for Delayed {
    type CallMsg = DelayedCallMessage;
    type CastMsg = DelayedCastMessage;
    type OutMsg = DelayedOutMessage;
    type State = DelayedState;
    type Error = ();

    fn handle_call(
        &mut self,
        _message: Self::CallMsg,
        _handle: &DelayedHandle,
        state: Self::State,
    ) -> CallResponse<Self> {
        let count = state.count;
        CallResponse::Reply(state, DelayedOutMessage::Count(count))
    }

    fn handle_cast(
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
    // Start a Delayed
    let mut repeater = Delayed::start(DelayedState { count: 0 });

    // Set a just once timed message
    let _ = send_after(
        Duration::from_millis(100),
        repeater.clone(),
        DelayedCastMessage::Inc,
    );

    // Wait for 200 milliseconds
    rt::sleep(Duration::from_millis(200));

    // Check count
    let count = Delayed::get_count(&mut repeater).unwrap();

    // Only one message (no repetition)
    assert_eq!(DelayedOutMessage::Count(1), count);

    // New timer
    let mut timer = send_after(
        Duration::from_millis(100),
        repeater.clone(),
        DelayedCastMessage::Inc,
    );

    // Cancel the new timer before timeout
    timer.cancellation_token.cancel();

    // Wait another 200 milliseconds
    rt::sleep(Duration::from_millis(200));

    // Check count again
    let count2 = Delayed::get_count(&mut repeater).unwrap();

    // As timer was cancelled, count should remain at 1
    assert_eq!(DelayedOutMessage::Count(1), count2);
}
