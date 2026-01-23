use crate::threads::{
    send_interval, Actor, ActorRef, InitResult, MessageResponse, RequestResponse,
};
use spawned_rt::threads::{self as rt, CancellationToken};
use std::time::Duration;

use super::send_after;

type RepeaterHandle = ActorRef<Repeater>;

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

#[derive(Clone)]
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
    pub fn stop_timer(server: &mut RepeaterHandle) -> Result<(), ()> {
        server.send(RepeaterCastMessage::StopTimer).map_err(|_| ())
    }

    pub fn get_count(server: &mut RepeaterHandle) -> Result<RepeaterOutMessage, ()> {
        server
            .request(RepeaterCallMessage::GetCount)
            .map_err(|_| ())
    }
}

impl Actor for Repeater {
    type Request = RepeaterCallMessage;
    type Message = RepeaterCastMessage;
    type Reply = RepeaterOutMessage;
    type Error = ();

    fn init(mut self, handle: &RepeaterHandle) -> Result<InitResult<Self>, Self::Error> {
        let timer = send_interval(
            Duration::from_millis(100),
            handle.clone(),
            RepeaterCastMessage::Inc,
        );
        self.cancellation_token = Some(timer.cancellation_token);
        Ok(InitResult::Success(self))
    }

    fn handle_request(
        &mut self,
        _message: Self::Request,
        _handle: &RepeaterHandle,
    ) -> RequestResponse<Self> {
        let count = self.count;
        RequestResponse::Reply(RepeaterOutMessage::Count(count))
    }

    fn handle_message(
        &mut self,
        message: Self::Message,
        _handle: &ActorRef<Self>,
    ) -> MessageResponse {
        match message {
            RepeaterCastMessage::Inc => {
                self.count += 1;
            }
            RepeaterCastMessage::StopTimer => {
                if let Some(mut ct) = self.cancellation_token.clone() {
                    ct.cancel()
                };
            }
        };
        MessageResponse::NoReply
    }
}

#[test]
pub fn test_send_interval_and_cancellation() {
    // Start a Repeater
    let mut repeater = Repeater::new(0).start();

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

type DelayedHandle = ActorRef<Delayed>;

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

#[derive(Clone)]
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
    pub fn get_count(server: &mut DelayedHandle) -> Result<DelayedOutMessage, ()> {
        server.request(DelayedCallMessage::GetCount).map_err(|_| ())
    }
}

impl Actor for Delayed {
    type Request = DelayedCallMessage;
    type Message = DelayedCastMessage;
    type Reply = DelayedOutMessage;
    type Error = ();

    fn handle_request(
        &mut self,
        _message: Self::Request,
        _handle: &DelayedHandle,
    ) -> RequestResponse<Self> {
        let count = self.count;
        RequestResponse::Reply(DelayedOutMessage::Count(count))
    }

    fn handle_message(
        &mut self,
        message: Self::Message,
        _handle: &DelayedHandle,
    ) -> MessageResponse {
        match message {
            DelayedCastMessage::Inc => {
                self.count += 1;
            }
        };
        MessageResponse::NoReply
    }
}

#[test]
pub fn test_send_after_and_cancellation() {
    // Start a Delayed
    let mut repeater = Delayed::new(0).start();

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
