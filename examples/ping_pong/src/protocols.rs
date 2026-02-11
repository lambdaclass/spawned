use spawned_concurrency::error::ActorError;
use spawned_concurrency::tasks::{ActorRef, Handler};
use std::sync::Arc;

use crate::consumer::Consumer;
use crate::messages::{Ping, Pong};
use crate::producer::Producer;

// --- Protocol traits: cross-actor contracts ---

pub trait PingReceiver: Send + Sync {
    fn ping(&self) -> Result<(), ActorError>;
}

pub trait PongReceiver: Send + Sync {
    fn pong(&self) -> Result<(), ActorError>;
}

// --- Bridge impls ---

impl PingReceiver for ActorRef<Consumer>
where
    Consumer: Handler<Ping>,
{
    fn ping(&self) -> Result<(), ActorError> {
        self.send(Ping)
    }
}

impl PongReceiver for ActorRef<Producer>
where
    Producer: Handler<Pong>,
{
    fn pong(&self) -> Result<(), ActorError> {
        self.send(Pong)
    }
}

pub type PingInbox = Arc<dyn PingReceiver>;
pub type PongInbox = Arc<dyn PongReceiver>;
