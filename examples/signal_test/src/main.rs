use spawned_concurrency::error::ActorError;
use spawned_concurrency::tasks::{
    send_interval, send_message_on, Actor, ActorStart as _, Backend, Context, Handler,
};
use spawned_concurrency::{actor, protocol};
use spawned_rt::tasks::{self as rt, CancellationToken};
use std::{env, time::Duration};

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
    async fn started(&mut self, ctx: &Context<Self>) {
        tracing::info!("[{}] Actor initialized", self.name);
        let timer = send_interval(Duration::from_secs(1), ctx.clone(), Tick);
        self.timer_token = Some(timer.cancellation_token);
    }

    #[stopped]
    async fn stopped(&mut self, _ctx: &Context<Self>) {
        tracing::info!(
            "[{}] Teardown called! Final count: {}",
            self.name,
            self.count
        );
    }

    #[send_handler]
    async fn handle_tick(&mut self, _msg: Tick, _ctx: &Context<Self>) {
        self.count += 1;
        tracing::info!("[{}] Tick #{}", self.name, self.count);
    }

    #[send_handler]
    async fn handle_shutdown(&mut self, _msg: Shutdown, ctx: &Context<Self>) {
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

        send_message_on(actor1.context(), rt::ctrl_c(), Shutdown);
        send_message_on(actor2.context(), rt::ctrl_c(), Shutdown);

        actor1.join().await;
        actor2.join().await;

        tracing::info!("Main task exiting");
    });

    tracing::info!("Main function exiting");
}
