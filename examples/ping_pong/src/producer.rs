use spawned_concurrency::tasks::{self as concurrency, Process, ProcessInfo};
use spawned_rt::tasks::mpsc::Sender;

use crate::messages::Message;

pub struct Producer {
    consumer: Sender<Message>,
}

impl Producer {
    pub async fn spawn_new(consumer: Sender<Message>) -> ProcessInfo<Message> {
        Self { consumer }.spawn().await
    }

    fn send_ping(&self, tx: &Sender<Message>, consumer: &Sender<Message>) {
        let message = Message::Ping { from: tx.clone() };
        tracing::info!("Producer sent Ping");
        concurrency::send(consumer, message);
    }
}

impl Process<Message> for Producer {
    async fn init(&mut self, tx: &Sender<Message>) {
        self.send_ping(tx, &self.consumer);
    }

    async fn handle(&mut self, message: Message, tx: &Sender<Message>) -> Message {
        tracing::info!("Producer received {message:?}");
        self.send_ping(tx, &self.consumer);
        message
    }
}
