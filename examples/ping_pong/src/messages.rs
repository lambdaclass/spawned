use spawned_rt::tasks::mpsc::Sender;

#[derive(Debug, Clone)]
pub enum Message {
    Ping { from: Sender<Message> },
    Pong,
}
