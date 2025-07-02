use spawned_rt::tasks::mpsc::UnboundedSender;

#[derive(Debug, Clone)]
pub enum Message {
    Ping { from: UnboundedSender<Message> },
    Pong,
}
