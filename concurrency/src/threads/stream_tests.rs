use std::time::Duration;

use crate::threads::{
    stream::spawn_listener, stream_to_iter, CallResponse, CastResponse, GenServer, GenServerHandle,
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
pub fn test_sum_numbers_from_iter() {
    let mut summatory_handle = Summatory::start(0);
    let stream = vec![1u8, 2, 3, 4, 5].into_iter().map(Ok::<u8, ()>);

    spawn_listener(summatory_handle.clone(), message_builder, stream);

    // Wait for the stream to finish processing
    rt::sleep(Duration::from_secs(1));

    let val = Summatory::get_value(&mut summatory_handle).unwrap();
    assert_eq!(val, 15);
}

#[test]
pub fn test_sum_numbers_from_async_stream() {
    let mut summatory_handle = Summatory::start(0);
    let async_stream = tokio_stream::iter(vec![1u8, 2, 3, 4, 5].into_iter().map(Ok::<u8, ()>));
    let sync_stream = stream_to_iter(async_stream);

    spawn_listener(summatory_handle.clone(), message_builder, sync_stream);

    // Wait for the stream to finish processing
    rt::sleep(Duration::from_secs(1));

    let val = Summatory::get_value(&mut summatory_handle).unwrap();
    assert_eq!(val, 15);
}
