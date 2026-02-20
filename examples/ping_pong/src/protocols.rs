use spawned_concurrency::error::ActorError;
use std::sync::Arc;

pub trait PingReceiver: Send + Sync {
    fn ping(&self) -> Result<(), ActorError>;
}

pub trait PongReceiver: Send + Sync {
    fn pong(&self) -> Result<(), ActorError>;
}

pub type PingInbox = Arc<dyn PingReceiver>;
pub type PongInbox = Arc<dyn PongReceiver>;

pub trait AsPingReceiver {
    fn as_ping_receiver(&self) -> PingInbox;
}

pub trait AsPongReceiver {
    fn as_pong_receiver(&self) -> PongInbox;
}
