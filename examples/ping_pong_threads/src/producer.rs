use spawned_concurrency::threads::{self as concurrency, Process, ProcessInfo};
use spawned_rt::threads::mpsc::Sender;

use crate::messages::Message;

pub struct Producer {
    consumer: Sender<Message>,
}

impl Producer {
    pub fn spawn_new(consumer: Sender<Message>) -> ProcessInfo<Message> {
        Self { consumer }.spawn()
    }

    fn send_ping(&self, tx: &Sender<Message>, consumer: &Sender<Message>) {
        let message = Message::Ping { from: tx.clone() };
        tracing::info!("Producer sent Ping");
        concurrency::send(consumer, message);
    }
}

impl Process<Message> for Producer {
    fn init(&mut self, tx: &Sender<Message>) {
        self.send_ping(tx, &self.consumer);
    }

    fn handle(&mut self, message: Message, tx: &Sender<Message>) -> Message {
        tracing::info!("Producer received {message:?}");
        self.send_ping(tx, &self.consumer);
        message
    }
}
