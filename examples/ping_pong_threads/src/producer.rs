use spawned_concurrency::message::Message;
use spawned_concurrency::protocol_impl;
use spawned_concurrency::threads::{Actor, ActorRef, Context, Handler};
use std::sync::Arc;

use crate::messages::Pong;
use crate::protocols::{AsPongReceiver, PingInbox, PongInbox, PongReceiver};

pub struct SetConsumer(pub PingInbox);
impl Message for SetConsumer {
    type Result = ();
}

pub struct Producer {
    pub consumer: Option<PingInbox>,
}

impl Actor for Producer {}

impl Handler<SetConsumer> for Producer {
    fn handle(&mut self, msg: SetConsumer, _ctx: &Context<Self>) {
        self.consumer = Some(msg.0);
    }
}

impl Handler<Pong> for Producer {
    fn handle(&mut self, _msg: Pong, _ctx: &Context<Self>) {
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

impl AsPongReceiver for ActorRef<Producer> {
    fn as_pong_receiver(&self) -> PongInbox {
        Arc::new(self.clone())
    }
}
