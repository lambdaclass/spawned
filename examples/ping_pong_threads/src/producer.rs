use spawned_concurrency::message::Message;
use spawned_concurrency::threads::{Actor, Context, Handler};
use spawned_concurrency::actor;

use crate::protocols::pong_receiver::Pong;
use crate::protocols::{PingReceiverRef, PongReceiver};

pub struct SetConsumer(pub PingReceiverRef);
impl Message for SetConsumer {
    type Result = ();
}

pub struct Producer {
    pub consumer: Option<PingReceiverRef>,
}

#[actor(protocol = PongReceiver)]
impl Producer {
    #[send_handler]
    fn handle_set_consumer(&mut self, msg: SetConsumer, _ctx: &Context<Self>) {
        self.consumer = Some(msg.0);
    }

    #[send_handler]
    fn handle_pong(&mut self, _msg: Pong, _ctx: &Context<Self>) {
        tracing::info!("Producer received Pong, sending Ping");
        if let Some(consumer) = &self.consumer {
            let _ = consumer.ping();
        }
    }
}
