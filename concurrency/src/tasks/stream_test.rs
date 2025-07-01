use std::time::Duration;

use spawned_rt::tasks::{self as rt};

use crate::tasks::{spawn_listener, CallResponse, CastResponse, GenServer, GenServerHandle};

type SummatoryHandle = GenServerHandle<Summatory>;

struct Summatory;

type SummatoryState = u16;
type SummatoryCastMessage = SummatoryState;
type SummatoryOutMessage = SummatoryState;

impl Summatory {
    pub async fn get_value(server: &mut SummatoryHandle) -> Result<SummatoryState, ()> {
        server.call(()).await.map_err(|_| ())
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

    async fn handle_cast(
        &mut self,
        message: Self::CastMsg,
        _handle: &GenServerHandle<Self>,
        state: Self::State,
    ) -> CastResponse<Self> {
        let new_state = state + message;
        CastResponse::NoReply(new_state)
    }

    async fn handle_call(
        &mut self,
        _message: Self::CallMsg,
        _handle: &SummatoryHandle,
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
    let runtime = rt::Runtime::new().unwrap();
    runtime.block_on(async move {
        let mut summatory_handle = Summatory::start(0);
        let stream = tokio_stream::iter(vec![1u8, 2, 3, 4, 5].into_iter().map(Ok));

        spawn_listener(summatory_handle.clone(), message_builder, stream);

        // Wait for 1 second so the whole stream is processed
        rt::sleep(Duration::from_secs(1)).await;

        let val = Summatory::get_value(&mut summatory_handle).await.unwrap();
        assert_eq!(val, 15);
    })
}
