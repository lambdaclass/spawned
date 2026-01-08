use spawned_concurrency::{self as concurrency, Process, ProcessInfo};
use spawned_rt::tasks::mpsc::Sender;

use crate::messages::Message;

pub struct Consumer {}

impl Consumer {
    pub async fn spawn_new() -> ProcessInfo<Message> {
        Self {}.spawn().await
    }
}

impl Process<Message> for Consumer {
    async fn handle(&mut self, message: Message, _tx: &Sender<Message>) -> Message {
        tracing::info!("Consumer received {message:?}");
        match message.clone() {
            Message::Ping { from } => {
                tracing::info!("Consumer sent Pong");
                concurrency::send(&from, Message::Pong);
            }
            Message::Pong => (),
        };
        message
    }
}
