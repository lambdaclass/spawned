use std::{time::Duration, u8};

use crate::threads::{
    stream::{spawn_listener, spawn_listener_from_iter},
    CallResponse, CastResponse, GenServer, GenServerHandle,
};

use spawned_rt::threads::{self as rt};

type SummatoryHandle = GenServerHandle<Summatory>;

struct Summatory;

type SummatoryState = u16;
type SummatoryCastMessage = SummatoryState;
type SummatoryOutMessage = SummatoryState;

impl Summatory {
    pub fn get_value(server: &mut SummatoryHandle) -> Result<SummatoryState, ()> {
        server.call(()).map_err(|_| ())
    }
}

impl GenServer for Summatory {
    type CallMsg = (); // We only handle one type of call, so there is no need for a specific message type.
    type CastMsg = SummatoryCastMessage;
    type OutMsg = SummatoryOutMessage;
    type State = SummatoryState;
    type Error = ();

    fn new() -> Self {
        Self
    }

    fn handle_cast(
        &mut self,
        message: Self::CastMsg,
        _handle: &GenServerHandle<Self>,
        state: Self::State,
    ) -> CastResponse<Self> {
        let new_state = state + message;
        CastResponse::NoReply(new_state)
    }

    fn handle_call(
        &mut self,
        _message: Self::CallMsg,
        _handle: &GenServerHandle<Self>,
        state: Self::State,
    ) -> CallResponse<Self> {
        let current_value = state;
        CallResponse::Reply(state, current_value)
    }
}

// In this example, the stream sends u8 values, which are converted to the type
// supported by the GenServer (SummatoryCastMessage / u16).
fn message_builder(value: u8) -> SummatoryCastMessage {
    value.into()
}

#[test]
pub fn test_sum_numbers_from_stream() {
    // In this example we are converting an async stream into a synchronous one
    // Does this make sense in a real-world scenario?
    let mut summatory_handle = Summatory::start(0);
    let stream = tokio_stream::iter(vec![1u8, 2, 3, 4, 5].into_iter().map(Ok::<u8, ()>));

    spawn_listener(summatory_handle.clone(), message_builder, stream);

    // Wait for 1 second so the whole stream is processed
    rt::sleep(Duration::from_secs(1));

    let val = Summatory::get_value(&mut summatory_handle).unwrap();
    assert_eq!(val, 15);
}

#[test]
pub fn test_sum_numbers_from_channel() {
    let mut summatory_handle = Summatory::start(0);
    let (tx, rx) = rt::mpsc::channel::<Result<u8, ()>>(5);

    rt::spawn(move || {
        for i in 1..=5 {
            tx.send(Ok(i)).unwrap();
        }
    });

    spawn_listener_from_iter(summatory_handle.clone(), message_builder, rx.into_iter());

    // Wait for 1 second so the whole stream is processed
    rt::sleep(Duration::from_secs(1));

    let val = Summatory::get_value(&mut summatory_handle).unwrap();
    assert_eq!(val, 15);
}

#[test]
pub fn test_sum_numbers_from_unbounded_channel() {
    let mut summatory_handle = Summatory::start(0);
    let (tx, rx) = rt::mpsc::unbounded_channel::<Result<u8, ()>>();

    rt::spawn(move || {
        for i in 1..=5 {
            tx.send(Ok(i)).unwrap();
        }
    });

    spawn_listener_from_iter(summatory_handle.clone(), message_builder, rx.into_iter());

    // Wait for 1 second so the whole stream is processed
    rt::sleep(Duration::from_secs(1));

    let val = Summatory::get_value(&mut summatory_handle).unwrap();
    assert_eq!(val, 15);
}

#[test]
pub fn test_sum_numbers_from_iter() {
    let mut summatory_handle = Summatory::start(0);
    let stream = vec![1u8, 2, 3, 4, 5].into_iter().map(Ok::<u8, ()>);

    spawn_listener_from_iter(summatory_handle.clone(), message_builder, stream);

    // Wait for the stream to finish processing
    rt::sleep(Duration::from_secs(1));

    let val = Summatory::get_value(&mut summatory_handle).unwrap();
    assert_eq!(val, 15);
}

type BigSummatoryHandle = GenServerHandle<BigSummatory>;

struct BigSummatory;

type BigSummatoryState = u128;
type BigSummatoryCastMessage = BigSummatoryState;
type BigSummatoryOutMessage = BigSummatoryState;

impl BigSummatory {
    pub fn get_value(server: &mut BigSummatoryHandle) -> Result<BigSummatoryState, ()> {
        server.call(()).map_err(|_| ())
    }
}

impl GenServer for BigSummatory {
    type CallMsg = (); // We only handle one type of call, so there is no need for a specific message type.
    type CastMsg = BigSummatoryCastMessage;
    type OutMsg = BigSummatoryOutMessage;
    type State = BigSummatoryState;
    type Error = ();

    fn new() -> Self {
        Self
    }

    fn handle_cast(
        &mut self,
        message: Self::CastMsg,
        _handle: &GenServerHandle<Self>,
        state: Self::State,
    ) -> CastResponse<Self> {
        let new_state = state + message;
        CastResponse::NoReply(new_state)
    }

    fn handle_call(
        &mut self,
        _message: Self::CallMsg,
        _handle: &GenServerHandle<Self>,
        state: Self::State,
    ) -> CallResponse<Self> {
        let current_value = state;
        CallResponse::Reply(state, current_value)
    }
}

#[test]
pub fn test_stream_cancellation() {
    // TODO: This test is in some way flaky, it relies on the processing machine
    // not being fast enough, look for a more reliable way to test this.
    let mut summatory_handle = BigSummatory::start(0);

    let big_range = 1u32..=u16::MAX as u32;
    let big_stream = big_range.clone().into_iter().map(Ok::<u32, ()>);
    let expected_result: u128 = big_range.sum::<u32>() as u128;

    let (_handle, mut cancel_token) =
        spawn_listener_from_iter(summatory_handle.clone(), |x| x as u128, big_stream);

    // Wait for a moment to allow some processing
    rt::sleep(Duration::from_millis(1));

    // Cancel the stream processing
    cancel_token.cancel();

    let val = BigSummatory::get_value(&mut summatory_handle).unwrap();

    // BigSummatory should not have had enough time to process all items
    assert_ne!(val, expected_result);

    // Yet still should have processed some items
    assert!(val > 0);
}
