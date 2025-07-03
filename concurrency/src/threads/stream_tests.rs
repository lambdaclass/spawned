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

#[test]
pub fn test_stream_cancellation() {
    let mut summatory_handle = Summatory::start(0);
    let stream = vec![1u8, 2, 3, 4, 5].into_iter().map(Ok::<u8, ()>);

    const RUNNING_TIME: u64 = 1000;

    // Add 'processing time' to each message
    let message_builder = |x: u8| {
        rt::sleep(Duration::from_millis(RUNNING_TIME / 4));
        x as u16
    };

    spawn_listener_from_iter(summatory_handle.clone(), message_builder, stream);

    // Wait for the stream to finish processing
    rt::sleep(Duration::from_millis(RUNNING_TIME));

    // The reasoning for this assertion is that each message takes a quarter of the total time
    // to be processed, so having a stream of 5 messages, some of them will never be processed.
    // At first glance we would expect val == 10 considering it has time to process four messages,
    // but in reality it will only get to process three messages due to other unacounted (minimal) overheads.
    let val = Summatory::get_value(&mut summatory_handle).unwrap();
    assert_eq!(val, 6);
}
