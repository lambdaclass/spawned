use spawned_rt::threads::mpsc::Sender;

#[derive(Debug, Clone)]
pub enum Message {
    Ping { from: Sender<Message> },
    Pong,
}
