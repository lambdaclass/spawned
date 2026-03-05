use spawned_concurrency::threads::{Actor, Context, Handler};
use spawned_concurrency::actor;

use crate::protocols::ping_receiver::Ping;
use crate::protocols::{PingReceiver, PongReceiverRef};

pub struct Consumer {
    pub producer: PongReceiverRef,
}

#[actor(protocol = PingReceiver)]
impl Consumer {
    #[send_handler]
    fn handle_ping(&mut self, _msg: Ping, _ctx: &Context<Self>) {
        tracing::info!("Consumer received Ping, sending Pong");
        let _ = self.producer.pong();
    }
}
