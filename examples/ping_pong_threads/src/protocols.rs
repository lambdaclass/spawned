use spawned_concurrency::error::ActorError;
use spawned_macros::protocol;

#[protocol]
pub trait PingReceiver: Send + Sync {
    fn ping(&self) -> Result<(), ActorError>;
}

#[protocol]
pub trait PongReceiver: Send + Sync {
    fn pong(&self) -> Result<(), ActorError>;
}
