use spawned_rt::sync::mpsc::Sender;

#[derive(Debug, Clone)]
pub enum Message {
    Ping { from: Sender<Message> },
    Pong,
}
