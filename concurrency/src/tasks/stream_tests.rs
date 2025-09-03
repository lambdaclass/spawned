use crate::tasks::{
    send_after, stream::spawn_listener, CallResponse, CastResponse, GenServer, GenServerHandle,
};
use spawned_rt::tasks::{self as rt, BroadcastStream, ReceiverStream};
use std::time::Duration;

type SummatoryHandle = GenServerHandle<Summatory>;

struct Summatory {
    count: u16,
}

impl Summatory {
    pub fn new(count: u16) -> Self {
        Self { count }
    }
}

type SummatoryOutMessage = u16;

#[derive(Clone)]
enum SummatoryCastMessage {
    Add(u16),
    StreamError,
    Stop,
}

impl Summatory {
    pub async fn get_value(server: &mut SummatoryHandle) -> Result<u16, ()> {
        server.call(()).await.map_err(|_| ())
    }
}

impl GenServer for Summatory {
    type CallMsg = (); // We only handle one type of call, so there is no need for a specific message type.
    type CastMsg = SummatoryCastMessage;
    type OutMsg = SummatoryOutMessage;
    type Error = ();

    async fn handle_cast(
        &mut self,
        message: Self::CastMsg,
        _handle: &GenServerHandle<Self>,
    ) -> CastResponse {
        match message {
            SummatoryCastMessage::Add(val) => {
                self.count += val;
                CastResponse::NoReply
            }
            SummatoryCastMessage::StreamError => CastResponse::NoReply,
            SummatoryCastMessage::Stop => CastResponse::Stop,
        }
    }

    async fn handle_call(
        &mut self,
        _message: Self::CallMsg,
        _handle: &SummatoryHandle,
    ) -> CallResponse<Self> {
        let current_value = self.count;
        CallResponse::Reply(current_value)
    }
}

// In this example, the stream sends u8 values, which are converted to the type
// supported by the GenServer (SummatoryCastMessage / u16).
fn message_builder<E>(value: Result<u8, E>) -> SummatoryCastMessage {
    match value {
        Ok(number) => SummatoryCastMessage::Add(number.into()),
        Err(_) => SummatoryCastMessage::StreamError,
    }
}

#[test]
pub fn test_sum_numbers_from_stream() {
    let runtime = rt::Runtime::new().unwrap();
    runtime.block_on(async move {
        let mut summatory_handle = Summatory::new(0).start();
        let stream = tokio_stream::iter(vec![1u8, 2, 3, 4, 5].into_iter().map(Ok::<u8, ()>));

        spawn_listener(summatory_handle.clone(), message_builder, stream);

        // Wait for 1 second so the whole stream is processed
        rt::sleep(Duration::from_secs(1)).await;

        let val = Summatory::get_value(&mut summatory_handle).await.unwrap();
        assert_eq!(val, 15);
    })
}

#[test]
pub fn test_sum_numbers_from_channel() {
    let runtime = rt::Runtime::new().unwrap();
    runtime.block_on(async move {
        let mut summatory_handle = Summatory::new(0).start();
        let (tx, rx) = spawned_rt::tasks::mpsc::channel::<Result<u8, ()>>();

        // Spawn a task to send numbers to the channel
        spawned_rt::tasks::spawn(async move {
            for i in 1..=5 {
                tx.send(Ok(i)).unwrap();
            }
        });

        spawn_listener(
            summatory_handle.clone(),
            message_builder,
            ReceiverStream::new(rx),
        );

        // Wait for 1 second so the whole stream is processed
        rt::sleep(Duration::from_secs(1)).await;

        let val = Summatory::get_value(&mut summatory_handle).await.unwrap();
        assert_eq!(val, 15);
    })
}

#[test]
pub fn test_sum_numbers_from_broadcast_channel() {
    let runtime = rt::Runtime::new().unwrap();
    runtime.block_on(async move {
        let mut summatory_handle = Summatory::new(0).start();
        let (tx, rx) = tokio::sync::broadcast::channel::<u8>(5);

        // Spawn a task to send numbers to the channel
        spawned_rt::tasks::spawn(async move {
            for i in 1u8..=5 {
                tx.send(i).unwrap();
            }
        });

        spawn_listener(
            summatory_handle.clone(),
            message_builder,
            BroadcastStream::new(rx),
        );

        // Wait for 1 second so the whole stream is processed
        rt::sleep(Duration::from_secs(1)).await;

        let val = Summatory::get_value(&mut summatory_handle).await.unwrap();
        assert_eq!(val, 15);
    })
}

#[test]
pub fn test_stream_cancellation() {
    const RUNNING_TIME: u64 = 1000;

    let runtime = rt::Runtime::new().unwrap();
    runtime.block_on(async move {
        let mut summatory_handle = Summatory::new(0).start();
        let (tx, rx) = spawned_rt::tasks::mpsc::channel::<Result<u8, ()>>();

        // Spawn a task to send numbers to the channel
        spawned_rt::tasks::spawn(async move {
            for i in 1..=5 {
                tx.send(Ok(i)).unwrap();
                rt::sleep(Duration::from_millis(RUNNING_TIME / 4)).await;
            }
        });

        let listener_handle = spawn_listener(
            summatory_handle.clone(),
            message_builder,
            ReceiverStream::new(rx),
        );

        // Start a timer to stop the stream after a certain time
        let summatory_handle_clone = summatory_handle.clone();
        let _ = send_after(
            Duration::from_millis(RUNNING_TIME + 10),
            summatory_handle_clone,
            SummatoryCastMessage::Stop,
        );

        // Just before the stream is cancelled we retrieve the current value.
        rt::sleep(Duration::from_millis(RUNNING_TIME)).await;
        let val = Summatory::get_value(&mut summatory_handle).await.unwrap();

        // The reasoning for this assertion is that each message takes a quarter of the total time
        // to be processed, so having a stream of 5 messages, the last one won't be processed.
        // We could safely assume that it will get to process 4 messages, but in case of any extenal
        // slowdown, it could process less.
        assert!((1..=10).contains(&val));

        assert!(listener_handle.await.is_ok());

        // Finnally, we check that the server is stopped, by getting an error when trying to call it.
        rt::sleep(Duration::from_millis(10)).await;
        assert!(Summatory::get_value(&mut summatory_handle).await.is_err());
    })
}

#[test]
pub fn test_stream_skipping_decoding_error() {
    let runtime = rt::Runtime::new().unwrap();
    runtime.block_on(async move {
        let mut summatory_handle = Summatory::new(0).start();
        let stream = tokio_stream::iter(vec![Ok(1), Ok(2), Ok(3), Err(()), Ok(4), Ok(5)]);

        spawn_listener(summatory_handle.clone(), message_builder, stream);

        // Wait for 1 second so the whole stream is processed
        rt::sleep(Duration::from_secs(1)).await;

        let val = Summatory::get_value(&mut summatory_handle).await.unwrap();
        assert_eq!(val, 15);
    })
}
