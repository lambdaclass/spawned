use spawned_concurrency::message::Message;
use spawned_concurrency::protocol_impl;
use spawned_concurrency::tasks::{Actor, ActorRef, Context, Handler};

use crate::messages::Pong;
use crate::protocols::{PingInbox, PongReceiver};

pub struct SetConsumer(pub PingInbox);
impl Message for SetConsumer {
    type Result = ();
}

pub struct Producer {
    pub consumer: Option<PingInbox>,
}

impl Actor for Producer {}

impl Handler<SetConsumer> for Producer {
    async fn handle(&mut self, msg: SetConsumer, _ctx: &Context<Self>) {
        self.consumer = Some(msg.0);
    }
}

impl Handler<Pong> for Producer {
    async fn handle(&mut self, _msg: Pong, _ctx: &Context<Self>) {
        tracing::info!("Producer received Pong, sending Ping");
        if let Some(consumer) = &self.consumer {
            let _ = consumer.ping();
        }
    }
}

protocol_impl! {
    PongReceiver for ActorRef<Producer> {
        send fn pong() => Pong;
    }
}
