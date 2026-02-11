//! ChatRoom actor — knows about `Say`, `Join`, and `ChatParticipant` trait.
//! Does NOT know about the `User` type.

use spawned_concurrency::error::ActorError;
use spawned_concurrency::message::Message;
use spawned_concurrency::tasks::{Actor, ActorRef, Context, Handler};
use std::sync::Arc;

use crate::messages::Say;
use crate::protocols::{ChatBroadcaster, ChatParticipant};

// Join carries an Arc<dyn ChatParticipant>, so we define it here (not via macro)
pub struct Join {
    pub name: String,
    pub inbox: Arc<dyn ChatParticipant>,
}

impl Message for Join {
    type Result = ();
}

impl std::fmt::Debug for Join {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Join").field("name", &self.name).finish()
    }
}

pub struct ChatRoom {
    members: Vec<(String, Arc<dyn ChatParticipant>)>,
}

impl ChatRoom {
    pub fn new() -> Self {
        Self {
            members: Vec::new(),
        }
    }
}

impl Actor for ChatRoom {}

impl Handler<Join> for ChatRoom {
    async fn handle(&mut self, msg: Join, _ctx: &Context<Self>) {
        tracing::info!("[room] {} joined", msg.name);
        self.members.push((msg.name, msg.inbox));
    }
}

impl Handler<Say> for ChatRoom {
    async fn handle(&mut self, msg: Say, _ctx: &Context<Self>) {
        tracing::info!("[room] {} says: {}", msg.from, msg.text);

        // Broadcast to all members except the sender
        for (name, inbox) in &self.members {
            if *name != msg.from {
                let _ = inbox.deliver(msg.from.clone(), msg.text.clone());
            }
        }
    }
}

// Bridge: ActorRef<ChatRoom> implements ChatBroadcaster
impl ChatBroadcaster for ActorRef<ChatRoom> {
    fn say(&self, from: String, text: String) -> Result<(), ActorError> {
        self.send(Say { from, text })
    }

    fn add_member(&self, name: String, inbox: Arc<dyn ChatParticipant>) -> Result<(), ActorError> {
        self.send(Join { name, inbox })
    }
}
