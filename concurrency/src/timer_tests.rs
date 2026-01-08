use crate::{
    send_after, send_interval, Backend, CallResponse, CastResponse, GenServer, GenServerHandle,
    InitResult, InitResult::Success,
};
use spawned_rt::tasks::{self as rt, CancellationToken};
use std::time::Duration;

type RepeaterHandle = GenServerHandle<Repeater>;

#[derive(Clone, Debug)]
enum RepeaterCastMessage {
    Inc,
    StopTimer,
}

#[derive(Clone, Debug)]
enum RepeaterCallMessage {
    GetCount,
}

#[derive(PartialEq, Debug)]
enum RepeaterOutMessage {
    Count(i32),
}

struct Repeater {
    pub(crate) count: i32,
    pub(crate) cancellation_token: Option<CancellationToken>,
}

impl Repeater {
    pub fn new(initial_count: i32) -> Self {
        Repeater {
            count: initial_count,
            cancellation_token: None,
        }
    }
}

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
    type Error = ();

    async fn init(mut self, handle: &RepeaterHandle) -> Result<InitResult<Self>, Self::Error> {
        let timer = send_interval(
            Duration::from_millis(100),
            handle.clone(),
            RepeaterCastMessage::Inc,
        );
        self.cancellation_token = Some(timer.cancellation_token);
        Ok(Success(self))
    }

    async fn handle_call(
        &mut self,
        _message: Self::CallMsg,
        _handle: &RepeaterHandle,
    ) -> CallResponse<Self> {
        let count = self.count;
        CallResponse::Reply(RepeaterOutMessage::Count(count))
    }

    async fn handle_cast(
        &mut self,
        message: Self::CastMsg,
        _handle: &GenServerHandle<Self>,
    ) -> CastResponse {
        match message {
            RepeaterCastMessage::Inc => {
                self.count += 1;
            }
            RepeaterCastMessage::StopTimer => {
                if let Some(ct) = self.cancellation_token.clone() {
                    ct.cancel()
                };
            }
        };
        CastResponse::NoReply
    }
}

#[test]
pub fn test_send_interval_and_cancellation() {
    let runtime = rt::Runtime::new().unwrap();
    runtime.block_on(async move {
        // Start a Repeater
        let mut repeater = Repeater::new(0).start(Backend::Async);

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

#[derive(Clone, Debug)]
enum DelayedCastMessage {
    Inc,
}

#[derive(Clone, Debug)]
enum DelayedCallMessage {
    GetCount,
    Stop,
}

#[derive(PartialEq, Debug)]
enum DelayedOutMessage {
    Count(i32),
}

struct Delayed {
    pub(crate) count: i32,
}

impl Delayed {
    pub fn new(initial_count: i32) -> Self {
        Delayed {
            count: initial_count,
        }
    }
}

impl Delayed {
    pub async fn get_count(server: &mut DelayedHandle) -> Result<DelayedOutMessage, ()> {
        server
            .call(DelayedCallMessage::GetCount)
            .await
            .map_err(|_| ())
    }

    pub async fn stop(server: &mut DelayedHandle) -> Result<DelayedOutMessage, ()> {
        server.call(DelayedCallMessage::Stop).await.map_err(|_| ())
    }
}

impl GenServer for Delayed {
    type CallMsg = DelayedCallMessage;
    type CastMsg = DelayedCastMessage;
    type OutMsg = DelayedOutMessage;
    type Error = ();

    async fn handle_call(
        &mut self,
        message: Self::CallMsg,
        _handle: &DelayedHandle,
    ) -> CallResponse<Self> {
        match message {
            DelayedCallMessage::GetCount => {
                let count = self.count;
                CallResponse::Reply(DelayedOutMessage::Count(count))
            }
            DelayedCallMessage::Stop => CallResponse::Stop(DelayedOutMessage::Count(self.count)),
        }
    }

    async fn handle_cast(
        &mut self,
        message: Self::CastMsg,
        _handle: &DelayedHandle,
    ) -> CastResponse {
        match message {
            DelayedCastMessage::Inc => {
                self.count += 1;
            }
        };
        CastResponse::NoReply
    }
}

#[test]
pub fn test_send_after_and_cancellation() {
    let runtime = rt::Runtime::new().unwrap();
    runtime.block_on(async move {
        // Start a Delayed
        let mut repeater = Delayed::new(0).start(Backend::Async);

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
        let mut repeater = Delayed::new(0).start(Backend::Async);

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

        // Stop the GenServer before timeout
        let count2 = Delayed::stop(&mut repeater).await.unwrap();

        // Wait another 200 milliseconds
        rt::sleep(Duration::from_millis(200)).await;

        // As timer was cancelled, count should remain at 1
        assert_eq!(DelayedOutMessage::Count(1), count2);
    });
}
