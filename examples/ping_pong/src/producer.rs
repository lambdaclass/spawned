use spawned_concurrency::message::Message;
use spawned_concurrency::tasks::{Actor, Context, Handler, Recipient};

use crate::messages::{Ping, Pong};

pub struct SetConsumer(pub Recipient<Ping>);
impl Message for SetConsumer {
    type Result = ();
}

pub struct Producer {
    pub consumer: Option<Recipient<Ping>>,
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
            let _ = consumer.send(Ping);
        }
    }
}
