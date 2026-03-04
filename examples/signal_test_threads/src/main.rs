use spawned_concurrency::error::ActorError;
use spawned_concurrency::threads::{
    send_interval, send_message_on, Actor, ActorStart as _, Context, Handler,
};
use spawned_macros::{actor, protocol};
use spawned_rt::threads::{self as rt, CancellationToken};
use std::time::Duration;

#[protocol]
pub trait TickingProtocol: Send + Sync {
    fn tick(&self) -> Result<(), ActorError>;
    fn shutdown(&self) -> Result<(), ActorError>;
}

use ticking_protocol::{Shutdown, Tick};

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

#[actor(protocol = TickingProtocol)]
impl TickingActor {
    #[started]
    fn started(&mut self, ctx: &Context<Self>) {
        tracing::info!("[{}] Actor initialized", self.name);
        let timer = send_interval(Duration::from_secs(1), ctx.clone(), Tick);
        self.timer_token = Some(timer.cancellation_token);
    }

    #[stopped]
    fn stopped(&mut self, _ctx: &Context<Self>) {
        tracing::info!(
            "[{}] Teardown called! Final count: {}",
            self.name,
            self.count
        );
    }

    #[send_handler]
    fn handle_tick(&mut self, _msg: Tick, _ctx: &Context<Self>) {
        self.count += 1;
        tracing::info!("[{}] Tick #{}", self.name, self.count);
    }

    #[send_handler]
    fn handle_shutdown(&mut self, _msg: Shutdown, ctx: &Context<Self>) {
        tracing::info!("[{}] Received shutdown signal", self.name);
        ctx.stop();
    }
}

fn main() {
    rt::run(|| {
        tracing::info!("Starting signal test for threads Actor");
        tracing::info!("Press Ctrl+C to test signal handling...");

        let actor1 = TickingActor::new("actor-1").start();
        let actor2 = TickingActor::new("actor-2").start();

        send_message_on(actor1.context(), rt::ctrl_c(), Shutdown);
        send_message_on(actor2.context(), rt::ctrl_c(), Shutdown);

        actor1.join();
        actor2.join();

        tracing::info!("Main thread exiting");
    });

    tracing::info!("Main function exiting");
}
