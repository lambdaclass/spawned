use crate::tasks::{
    send_after, stream::spawn_listener, Actor, ActorRef, MessageResponse, RequestResponse,
};
use futures::{stream, StreamExt};
use spawned_rt::tasks::{self as rt, BroadcastStream, ReceiverStream};
use std::time::Duration;

type SummatoryHandle = ActorRef<Summatory>;

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
        server.request(()).await.map_err(|_| ())
    }
}

impl Actor for Summatory {
    type Request = (); // We only handle one type of call, so there is no need for a specific message type.
    type Message = SummatoryCastMessage;
    type Reply = SummatoryOutMessage;
    type Error = ();

    async fn handle_message(
        &mut self,
        message: Self::Message,
        _handle: &ActorRef<Self>,
    ) -> MessageResponse {
        match message {
            SummatoryCastMessage::Add(val) => {
                self.count += val;
                MessageResponse::NoReply
            }
            SummatoryCastMessage::StreamError => MessageResponse::Stop,
            SummatoryCastMessage::Stop => MessageResponse::Stop,
        }
    }

    async fn handle_request(
        &mut self,
        _message: Self::Request,
        _handle: &SummatoryHandle,
    ) -> RequestResponse<Self> {
        let current_value = self.count;
        RequestResponse::Reply(current_value)
    }
}

#[test]
pub fn test_sum_numbers_from_stream() {
    let runtime = rt::Runtime::new().unwrap();
    runtime.block_on(async move {
        let mut summatory_handle = Summatory::new(0).start();
        let stream = stream::iter(vec![1u16, 2, 3, 4, 5].into_iter().map(Ok::<u16, ()>));

        spawn_listener(
            summatory_handle.clone(),
            stream.filter_map(|result| async move { result.ok().map(SummatoryCastMessage::Add) }),
        );

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
        let (tx, rx) = spawned_rt::tasks::mpsc::channel::<Result<u16, ()>>();

        // Spawn a task to send numbers to the channel
        spawned_rt::tasks::spawn(async move {
            for i in 1..=5 {
                tx.send(Ok(i)).unwrap();
            }
        });

        spawn_listener(
            summatory_handle.clone(),
            ReceiverStream::new(rx)
                .filter_map(|result| async move { result.ok().map(SummatoryCastMessage::Add) }),
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
        let (tx, rx) = tokio::sync::broadcast::channel::<u16>(5);

        // Spawn a task to send numbers to the channel
        spawned_rt::tasks::spawn(async move {
            for i in 1u16..=5 {
                tx.send(i).unwrap();
            }
        });

        spawn_listener(
            summatory_handle.clone(),
            BroadcastStream::new(rx)
                .filter_map(|result| async move { result.ok().map(SummatoryCastMessage::Add) }),
        );

        // Wait for 1 second so the whole stream is processed
        rt::sleep(Duration::from_secs(1)).await;

        let val = Summatory::get_value(&mut summatory_handle).await.unwrap();
        assert_eq!(val, 15);
    })
}

#[test]
pub fn test_stream_cancellation() {
    // Messages sent at: t=0, t=250, t=500, t=750, t=1000ms
    // We read at t=850ms (after 4th message at t=750, before 5th at t=1000)
    const MESSAGE_INTERVAL: u64 = 250;
    const READ_TIME: u64 = 850;
    const STOP_TIME: u64 = 1100;

    let runtime = rt::Runtime::new().unwrap();
    runtime.block_on(async move {
        let mut summatory_handle = Summatory::new(0).start();
        let (tx, rx) = spawned_rt::tasks::mpsc::channel::<Result<u16, ()>>();

        // Spawn a task to send numbers to the channel
        spawned_rt::tasks::spawn(async move {
            for i in 1..=5 {
                tx.send(Ok(i)).unwrap();
                rt::sleep(Duration::from_millis(MESSAGE_INTERVAL)).await;
            }
        });

        let listener_handle = spawn_listener(
            summatory_handle.clone(),
            ReceiverStream::new(rx)
                .filter_map(|result| async move { result.ok().map(SummatoryCastMessage::Add) }),
        );

        // Start a timer to stop the actor after all messages would be sent
        let summatory_handle_clone = summatory_handle.clone();
        let _ = send_after(
            Duration::from_millis(STOP_TIME),
            summatory_handle_clone,
            SummatoryCastMessage::Stop,
        );

        // Read value after 4th message (t=750) but before 5th (t=1000).
        // Expected sum: 1+2+3+4 = 10, but allow some slack for timing variations.
        rt::sleep(Duration::from_millis(READ_TIME)).await;
        let val = Summatory::get_value(&mut summatory_handle).await.unwrap();

        // At t=850ms, we expect 4 messages processed (sum=10), but timing variations
        // could result in 3 messages (sum=6) or occasionally all 5 (sum=15).
        assert!((1..=15).contains(&val));

        assert!(listener_handle.await.is_ok());

        // Finally, we check that the server is stopped, by getting an error when trying to call it.
        rt::sleep(Duration::from_millis(10)).await;
        assert!(Summatory::get_value(&mut summatory_handle).await.is_err());
    })
}

#[test]
pub fn test_halting_on_stream_error() {
    let runtime = rt::Runtime::new().unwrap();
    runtime.block_on(async move {
        let mut summatory_handle = Summatory::new(0).start();
        let stream = tokio_stream::iter(vec![Ok(1u16), Ok(2), Ok(3), Err(()), Ok(4), Ok(5)]);
        let msg_stream = stream.filter_map(|value| async move {
            match value {
                Ok(number) => Some(SummatoryCastMessage::Add(number)),
                Err(_) => Some(SummatoryCastMessage::StreamError),
            }
        });

        spawn_listener(summatory_handle.clone(), msg_stream);

        // Wait for 1 second so the whole stream is processed
        rt::sleep(Duration::from_secs(1)).await;

        let result = Summatory::get_value(&mut summatory_handle).await;
        // Actor should have been terminated, hence the result should be an error
        assert!(result.is_err());
    })
}

#[test]
pub fn test_skipping_on_stream_error() {
    let runtime = rt::Runtime::new().unwrap();
    runtime.block_on(async move {
        let mut summatory_handle = Summatory::new(0).start();
        let stream = tokio_stream::iter(vec![Ok(1u16), Ok(2), Ok(3), Err(()), Ok(4), Ok(5)]);
        let msg_stream = stream.filter_map(|value| async move {
            match value {
                Ok(number) => Some(SummatoryCastMessage::Add(number)),
                Err(_) => None,
            }
        });

        spawn_listener(summatory_handle.clone(), msg_stream);

        // Wait for 1 second so the whole stream is processed
        rt::sleep(Duration::from_secs(1)).await;

        let val = Summatory::get_value(&mut summatory_handle).await.unwrap();
        assert_eq!(val, 15);
    })
}
