use spawned_rt::mpsc::Sender;

#[derive(Debug, Clone)]
pub enum Message {
    Ping { from: Sender<Message> },
    Pong,
}
