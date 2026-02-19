//! User actor — knows about `SayToRoom`, `Deliver`, and `ChatBroadcaster` trait.
//! Does NOT know about the `ChatRoom` type.

use spawned_concurrency::error::ActorError;
use spawned_concurrency::tasks::{Actor, ActorRef, Context, Handler};
use std::sync::Arc;

use crate::messages::{Deliver, SayToRoom};
use crate::protocols::{ChatBroadcaster, ChatParticipant};

pub struct User {
    pub name: String,
    pub room: Arc<dyn ChatBroadcaster>,
}

impl Actor for User {}

impl Handler<SayToRoom> for User {
    async fn handle(&mut self, msg: SayToRoom, _ctx: &Context<Self>) {
        // Forward to the room via Arc<dyn ChatBroadcaster> — no ChatRoom type needed
        let _ = self.room.say(self.name.clone(), msg.text);
    }
}

impl Handler<Deliver> for User {
    async fn handle(&mut self, msg: Deliver, _ctx: &Context<Self>) {
        tracing::info!("[{}] got message from {}: {}", self.name, msg.from, msg.text);
    }
}

// Bridge: ActorRef<User> implements ChatParticipant
impl ChatParticipant for ActorRef<User> {
    fn deliver(&self, from: String, text: String) -> Result<(), ActorError> {
        self.send(Deliver { from, text })
    }
}
