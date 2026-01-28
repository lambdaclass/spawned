//! Test to verify signal handling for threads Actor.
//!
//! This example demonstrates using `send_message_on` to handle Ctrl+C signals.
//! The signal handler is set up in the Actor's `init()` function.
//!
//! Run with: cargo run --bin signal_test_threads
//!
//! Then press Ctrl+C and observe:
//! - Does the actor stop gracefully?
//! - Does teardown run?

use spawned_concurrency::{
    messages::Unused,
    threads::{send_interval, send_message_on, Actor, ActorRef, InitResult, MessageResponse},
};
use spawned_rt::threads::{self as rt, CancellationToken};
use std::time::Duration;

struct TickingActor {
    name: String,
    count: u64,
    timer_token: Option<CancellationToken>,
}

impl TickingActor {
    fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            count: 0,
            timer_token: None,
        }
    }
}

#[derive(Clone)]
enum Msg {
    Tick,
    Shutdown,
}

impl Actor for TickingActor {
    type Request = Unused;
    type Message = Msg;
    type Reply = Unused;
    type Error = ();

    fn init(mut self, handle: &ActorRef<Self>) -> Result<InitResult<Self>, Self::Error> {
        tracing::info!("[{}] Actor initialized", self.name);

        // Set up periodic ticking
        let timer = send_interval(Duration::from_secs(1), handle.clone(), Msg::Tick);
        self.timer_token = Some(timer.cancellation_token);

        // Set up Ctrl+C handler using send_message_on
        send_message_on(handle.clone(), rt::ctrl_c(), Msg::Shutdown);

        Ok(InitResult::Success(self))
    }

    fn handle_message(
        &mut self,
        message: Self::Message,
        _handle: &ActorRef<Self>,
    ) -> MessageResponse {
        match message {
            Msg::Tick => {
                self.count += 1;
                tracing::info!("[{}] Tick #{}", self.name, self.count);
                MessageResponse::NoReply
            }
            Msg::Shutdown => {
                tracing::info!("[{}] Received shutdown signal", self.name);
                MessageResponse::Stop
            }
        }
    }

    fn teardown(self, _handle: &ActorRef<Self>) -> Result<(), Self::Error> {
        tracing::info!(
            "[{}] Teardown called! Final count: {}",
            self.name,
            self.count
        );
        Ok(())
    }
}

fn main() {
    rt::run(|| {
        tracing::info!("Starting signal test for threads Actor");
        tracing::info!("Press Ctrl+C to test signal handling...");

        // Start two actors - both want to react to Ctrl+C
        // This currently panics because ctrl_c() can only be called once!
        let actor1 = TickingActor::new("actor-1").start();
        let actor2 = TickingActor::new("actor-2").start();

        // Wait for both actors to stop
        actor1.join();
        actor2.join();

        tracing::info!("Main thread exiting");
    });

    tracing::info!("Main function exiting");
}
