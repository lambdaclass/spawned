//! Test to verify signal handling across different Actor backends (tasks version).
//!
//! This example demonstrates using `send_message_on` to handle Ctrl+C signals.
//! The signal handler is set up in the Actor's `started()` function.
//!
//! Run with: cargo run --bin signal_test -- [async|blocking|thread]
//!
//! Then press Ctrl+C and observe:
//! - Does the actor stop gracefully?
//! - Does teardown run?

use spawned_concurrency::message::Message;
use spawned_concurrency::tasks::{
    send_interval, send_message_on, Actor, ActorStart as _, Backend, Context, Handler,
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

#[derive(Debug, Clone)]
struct Tick;
impl Message for Tick {
    type Result = ();
}

#[derive(Debug)]
struct Shutdown;
impl Message for Shutdown {
    type Result = ();
}

impl Actor for TickingActor {
    async fn started(&mut self, ctx: &Context<Self>) {
        tracing::info!("[{}] Actor initialized", self.name);

        // Set up periodic ticking — need an ActorRef to use with send_message_on
        let timer = send_interval(Duration::from_secs(1), ctx.clone(), Tick);
        self.timer_token = Some(timer.cancellation_token);
    }

    async fn stopped(&mut self, _ctx: &Context<Self>) {
        tracing::info!(
            "[{}] Teardown called! Final count: {}",
            self.name,
            self.count
        );
    }
}

impl Handler<Tick> for TickingActor {
    async fn handle(&mut self, _msg: Tick, _ctx: &Context<Self>) {
        self.count += 1;
        tracing::info!("[{}] Tick #{}", self.name, self.count);
    }
}

impl Handler<Shutdown> for TickingActor {
    async fn handle(&mut self, _msg: Shutdown, ctx: &Context<Self>) {
        tracing::info!("[{}] Received shutdown signal", self.name);
        ctx.stop();
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let backend = args.get(1).map(|s| s.as_str()).unwrap_or("async");

    rt::run(async move {
        tracing::info!("Starting signal test with backend: {}", backend);
        tracing::info!("Press Ctrl+C to test signal handling...");

        // Start two actors - both should react to Ctrl+C
        let (actor1, actor2) = match backend {
            "async" => {
                tracing::info!("Using Backend::Async");
                (
                    TickingActor::new("async-1").start_with_backend(Backend::Async),
                    TickingActor::new("async-2").start_with_backend(Backend::Async),
                )
            }
            "blocking" => {
                tracing::info!("Using Backend::Blocking");
                (
                    TickingActor::new("blocking-1").start_with_backend(Backend::Blocking),
                    TickingActor::new("blocking-2").start_with_backend(Backend::Blocking),
                )
            }
            "thread" => {
                tracing::info!("Using Backend::Thread");
                (
                    TickingActor::new("thread-1").start_with_backend(Backend::Thread),
                    TickingActor::new("thread-2").start_with_backend(Backend::Thread),
                )
            }
            _ => {
                tracing::error!(
                    "Unknown backend: {}. Use: async, blocking, or thread",
                    backend
                );
                return;
            }
        };

        // Set up Ctrl+C handler using send_message_on
        send_message_on(actor1.context(), rt::ctrl_c(), Shutdown);
        send_message_on(actor2.context(), rt::ctrl_c(), Shutdown);

        // Wait for both actors to stop
        actor1.join().await;
        actor2.join().await;

        tracing::info!("Main task exiting");
    });

    tracing::info!("Main function exiting");
}
