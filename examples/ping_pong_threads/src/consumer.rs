use spawned_concurrency::threads::{Actor, Context, Handler, Recipient};

use crate::messages::{Ping, Pong};

pub struct Consumer {
    pub producer: Recipient<Pong>,
}

impl Actor for Consumer {}

impl Handler<Ping> for Consumer {
    fn handle(&mut self, _msg: Ping, _ctx: &Context<Self>) {
        tracing::info!("Consumer received Ping, sending Pong");
        let _ = self.producer.send(Pong);
    }
}
