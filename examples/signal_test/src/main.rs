//! Test to verify signal handling across different Actor backends (tasks version).
//!
//! This example demonstrates using `send_message_on` to handle Ctrl+C signals.
//! The signal handler is set up in the Actor's `init()` function.
//!
//! Run with: cargo run --bin signal_test -- [async|blocking|thread]
//!
//! Then press Ctrl+C and observe:
//! - Does the actor stop gracefully?
//! - Does teardown run?

use spawned_concurrency::{
    messages::Unused,
    tasks::{
        send_interval, send_message_on, Actor, ActorRef, Backend, InitResult, MessageResponse,
    },
};
use spawned_rt::tasks::{self as rt, CancellationToken};
use std::{env, time::Duration};

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

    async fn init(mut self, handle: &ActorRef<Self>) -> Result<InitResult<Self>, Self::Error> {
        tracing::info!("[{}] Actor initialized", self.name);

        // Set up periodic ticking
        let timer = send_interval(Duration::from_secs(1), handle.clone(), Msg::Tick);
        self.timer_token = Some(timer.cancellation_token);

        // Set up Ctrl+C handler using send_message_on
        send_message_on(handle.clone(), rt::ctrl_c(), Msg::Shutdown);

        Ok(InitResult::Success(self))
    }

    async fn handle_message(
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

    async fn teardown(self, _handle: &ActorRef<Self>) -> Result<(), Self::Error> {
        tracing::info!(
            "[{}] Teardown called! Final count: {}",
            self.name,
            self.count
        );
        Ok(())
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let backend = args.get(1).map(|s| s.as_str()).unwrap_or("async");

    rt::run(async move {
        tracing::info!("Starting signal test with backend: {}", backend);
        tracing::info!("Press Ctrl+C to test signal handling...");

        let actor = match backend {
            "async" => {
                tracing::info!("Using Backend::Async");
                TickingActor::new("async").start_with_backend(Backend::Async)
            }
            "blocking" => {
                tracing::info!("Using Backend::Blocking");
                TickingActor::new("blocking").start_with_backend(Backend::Blocking)
            }
            "thread" => {
                tracing::info!("Using Backend::Thread");
                TickingActor::new("thread").start_with_backend(Backend::Thread)
            }
            _ => {
                tracing::error!(
                    "Unknown backend: {}. Use: async, blocking, or thread",
                    backend
                );
                return;
            }
        };

        // Wait for the actor to stop
        actor.join().await;

        tracing::info!("Main task exiting");
    });

    tracing::info!("Main function exiting");
}
