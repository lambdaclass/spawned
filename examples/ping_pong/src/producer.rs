use spawned_concurrency::message::Message;
use spawned_concurrency::tasks::{Actor, Context, Handler};
use spawned_macros::actor;

use crate::protocols::pong_receiver::Pong;
use crate::protocols::PingReceiverRef;

pub struct SetConsumer(pub PingReceiverRef);
impl Message for SetConsumer {
    type Result = ();
}

pub struct Producer {
    pub consumer: Option<PingReceiverRef>,
}

impl Actor for Producer {}

#[actor]
impl Producer {
    #[send_handler]
    async fn handle_set_consumer(&mut self, msg: SetConsumer, _ctx: &Context<Self>) {
        self.consumer = Some(msg.0);
    }

    #[send_handler]
    async fn handle_pong(&mut self, _msg: Pong, _ctx: &Context<Self>) {
        tracing::info!("Producer received Pong, sending Ping");
        if let Some(consumer) = &self.consumer {
            let _ = consumer.ping();
        }
    }
}
