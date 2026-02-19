use spawned_concurrency::protocol_impl;
use spawned_concurrency::threads::{Actor, ActorRef, Context, Handler};

use crate::messages::Ping;
use crate::protocols::{PingReceiver, PongInbox};

pub struct Consumer {
    pub producer: PongInbox,
}

impl Actor for Consumer {}

impl Handler<Ping> for Consumer {
    fn handle(&mut self, _msg: Ping, _ctx: &Context<Self>) {
        tracing::info!("Consumer received Ping, sending Pong");
        let _ = self.producer.pong();
    }
}

protocol_impl! {
    PingReceiver for ActorRef<Consumer> {
        send fn ping() => Ping;
    }
}
