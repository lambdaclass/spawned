use crate::tasks::{
    send_after, Actor, ActorStart, Context, Handler,
    stream::spawn_listener,
};
use crate::message::Message;
use futures::{stream, StreamExt};
use spawned_rt::tasks::{self as rt, BroadcastStream, ReceiverStream};
use std::time::Duration;

// --- Messages ---

#[derive(Debug)]
enum StreamMsg {
    Add(u16),
    Error,
}
impl Message for StreamMsg { type Result = (); }

#[derive(Debug)]
struct StopSum;
impl Message for StopSum { type Result = (); }

#[derive(Debug)]
struct GetValue;
impl Message for GetValue { type Result = u16; }

// --- Summatory Actor ---

struct Summatory {
    count: u16,
}

impl Summatory {
    pub fn new(count: u16) -> Self {
        Self { count }
    }
}

impl Actor for Summatory {}

impl Handler<StreamMsg> for Summatory {
    async fn handle(&mut self, msg: StreamMsg, ctx: &Context<Self>) {
        match msg {
            StreamMsg::Add(val) => self.count += val,
            StreamMsg::Error => ctx.stop(),
        }
    }
}

impl Handler<StopSum> for Summatory {
    async fn handle(&mut self, _msg: StopSum, ctx: &Context<Self>) {
        ctx.stop();
    }
}

impl Handler<GetValue> for Summatory {
    async fn handle(&mut self, _msg: GetValue, _ctx: &Context<Self>) -> u16 {
        self.count
    }
}

#[test]
pub fn test_sum_numbers_from_stream() {
    let runtime = rt::Runtime::new().unwrap();
    runtime.block_on(async move {
        let summatory = Summatory::new(0).start();
        let stream = stream::iter(vec![1u16, 2, 3, 4, 5].into_iter().map(Ok::<u16, ()>));

        let ctx = Context::from_ref(&summatory);
        spawn_listener(
            ctx,
            stream.filter_map(|result| async move { result.ok().map(StreamMsg::Add) }),
        );

        rt::sleep(Duration::from_secs(1)).await;

        let val = summatory.send_request(GetValue).await.unwrap();
        assert_eq!(val, 15);
    })
}

#[test]
pub fn test_sum_numbers_from_channel() {
    let runtime = rt::Runtime::new().unwrap();
    runtime.block_on(async move {
        let summatory = Summatory::new(0).start();
        let (tx, rx) = spawned_rt::tasks::mpsc::channel::<Result<u16, ()>>();

        spawned_rt::tasks::spawn(async move {
            for i in 1..=5 {
                tx.send(Ok(i)).unwrap();
            }
        });

        let ctx = Context::from_ref(&summatory);
        spawn_listener(
            ctx,
            ReceiverStream::new(rx)
                .filter_map(|result| async move { result.ok().map(StreamMsg::Add) }),
        );

        rt::sleep(Duration::from_secs(1)).await;

        let val = summatory.send_request(GetValue).await.unwrap();
        assert_eq!(val, 15);
    })
}

#[test]
pub fn test_sum_numbers_from_broadcast_channel() {
    let runtime = rt::Runtime::new().unwrap();
    runtime.block_on(async move {
        let summatory = Summatory::new(0).start();
        let (tx, rx) = tokio::sync::broadcast::channel::<u16>(5);

        spawned_rt::tasks::spawn(async move {
            for i in 1u16..=5 {
                tx.send(i).unwrap();
            }
        });

        let ctx = Context::from_ref(&summatory);
        spawn_listener(
            ctx,
            BroadcastStream::new(rx)
                .filter_map(|result| async move { result.ok().map(StreamMsg::Add) }),
        );

        rt::sleep(Duration::from_secs(1)).await;

        let val = summatory.send_request(GetValue).await.unwrap();
        assert_eq!(val, 15);
    })
}

#[test]
pub fn test_stream_cancellation() {
    const MESSAGE_INTERVAL: u64 = 250;
    const READ_TIME: u64 = 850;
    const STOP_TIME: u64 = 1100;

    let runtime = rt::Runtime::new().unwrap();
    runtime.block_on(async move {
        let summatory = Summatory::new(0).start();
        let (tx, rx) = spawned_rt::tasks::mpsc::channel::<Result<u16, ()>>();

        spawned_rt::tasks::spawn(async move {
            for i in 1..=5 {
                tx.send(Ok(i)).unwrap();
                rt::sleep(Duration::from_millis(MESSAGE_INTERVAL)).await;
            }
        });

        let ctx = Context::from_ref(&summatory);
        let listener_handle = spawn_listener(
            ctx.clone(),
            ReceiverStream::new(rx)
                .filter_map(|result| async move { result.ok().map(StreamMsg::Add) }),
        );

        let _ = send_after(
            Duration::from_millis(STOP_TIME),
            ctx,
            StopSum,
        );

        rt::sleep(Duration::from_millis(READ_TIME)).await;
        let val = summatory.send_request(GetValue).await.unwrap();

        assert!((1..=15).contains(&val));

        assert!(listener_handle.await.is_ok());

        rt::sleep(Duration::from_millis(10)).await;
        assert!(summatory.send_request(GetValue).await.is_err());
    })
}

#[test]
pub fn test_halting_on_stream_error() {
    let runtime = rt::Runtime::new().unwrap();
    runtime.block_on(async move {
        let summatory = Summatory::new(0).start();
        let stream = tokio_stream::iter(vec![Ok(1u16), Ok(2), Ok(3), Err(()), Ok(4), Ok(5)]);
        let msg_stream = stream.filter_map(|value| async move {
            match value {
                Ok(number) => Some(StreamMsg::Add(number)),
                Err(_) => Some(StreamMsg::Error),
            }
        });

        let ctx = Context::from_ref(&summatory);
        spawn_listener(ctx, msg_stream);

        rt::sleep(Duration::from_secs(1)).await;

        let result = summatory.send_request(GetValue).await;
        assert!(result.is_err());
    })
}

#[test]
pub fn test_skipping_on_stream_error() {
    let runtime = rt::Runtime::new().unwrap();
    runtime.block_on(async move {
        let summatory = Summatory::new(0).start();
        let stream = tokio_stream::iter(vec![Ok(1u16), Ok(2), Ok(3), Err(()), Ok(4), Ok(5)]);
        let msg_stream = stream.filter_map(|value| async move {
            match value {
                Ok(number) => Some(StreamMsg::Add(number)),
                Err(_) => None,
            }
        });

        let ctx = Context::from_ref(&summatory);
        spawn_listener(ctx, msg_stream);

        rt::sleep(Duration::from_secs(1)).await;

        let val = summatory.send_request(GetValue).await.unwrap();
        assert_eq!(val, 15);
    })
}
