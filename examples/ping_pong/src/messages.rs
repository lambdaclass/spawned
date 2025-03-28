use spawned_rt::Sender;

#[derive(Debug, Clone)]
pub enum Message {
    Ping { from: Sender<Message> },
    Pong,
}
