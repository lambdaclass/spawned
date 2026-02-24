use spawned_concurrency::tasks::{Actor, Context, Handler};
use spawned_macros::actor;

use crate::protocols::ping_receiver::Ping;
use crate::protocols::{PingReceiver, PongReceiverRef};

pub struct Consumer {
    pub producer: PongReceiverRef,
}

#[actor(protocol = PingReceiver)]
impl Consumer {
    #[send_handler]
    async fn handle_ping(&mut self, _msg: Ping, _ctx: &Context<Self>) {
        tracing::info!("Consumer received Ping, sending Pong");
        let _ = self.producer.pong();
    }
}
