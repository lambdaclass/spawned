use spawned_concurrency::threads::{Actor, Context, Handler};

use crate::messages::Ping;
use crate::protocols::PongInbox;

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
