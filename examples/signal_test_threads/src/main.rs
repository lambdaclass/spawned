//! Test to verify signal handling for threads Actor.
//!
//! This example demonstrates using `send_message_on` to handle Ctrl+C signals.
//! The signal handler is set up in the Actor's `started()` function.
//!
//! Run with: cargo run --bin signal_test_threads
//!
//! Then press Ctrl+C and observe:
//! - Does the actor stop gracefully?
//! - Does stopped run?

use spawned_concurrency::threads::{
    send_interval, send_message_on, Actor, ActorStart, Context, Handler, TimerHandle,
};
use spawned_rt::threads as rt;
use std::time::Duration;

struct TickingActor {
    name: String,
    count: u64,
    timer: Option<TimerHandle>,
}

impl TickingActor {
    fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            count: 0,
            timer: None,
        }
    }
}

spawned_concurrency::messages! {
    #[derive(Clone)]
    Tick -> ();
    Shutdown -> ()
}

impl Actor for TickingActor {
    fn started(&mut self, ctx: &Context<Self>) {
        tracing::info!("[{}] Actor initialized", self.name);

        // Set up periodic ticking
        let timer = send_interval(Duration::from_secs(1), ctx.clone(), Tick);
        self.timer = Some(timer);

        // Set up Ctrl+C handler using send_message_on
        send_message_on(ctx.clone(), rt::ctrl_c(), Shutdown);
    }

    fn stopped(&mut self, _ctx: &Context<Self>) {
        tracing::info!(
            "[{}] Stopped called! Final count: {}",
            self.name,
            self.count
        );
    }
}

impl Handler<Tick> for TickingActor {
    fn handle(&mut self, _msg: Tick, _ctx: &Context<Self>) {
        self.count += 1;
        tracing::info!("[{}] Tick #{}", self.name, self.count);
    }
}

impl Handler<Shutdown> for TickingActor {
    fn handle(&mut self, _msg: Shutdown, ctx: &Context<Self>) {
        tracing::info!("[{}] Received shutdown signal", self.name);
        ctx.stop();
    }
}

fn main() {
    rt::run(|| {
        tracing::info!("Starting signal test for threads Actor");
        tracing::info!("Press Ctrl+C to test signal handling...");

        // Start two actors - both will react to Ctrl+C
        let actor1 = TickingActor::new("actor-1").start();
        let actor2 = TickingActor::new("actor-2").start();

        // Wait for both actors to stop
        actor1.join();
        actor2.join();

        tracing::info!("Main thread exiting");
    });

    tracing::info!("Main function exiting");
}
