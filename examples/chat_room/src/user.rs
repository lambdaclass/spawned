//! User actor — knows about protocol traits, not ChatRoom.

use spawned_concurrency::error::ActorError;
use spawned_concurrency::protocol_impl;
use spawned_concurrency::send_messages;
use spawned_concurrency::tasks::{Actor, ActorRef, Context, Handler};
use spawned_macros::actor;
use std::sync::Arc;

use crate::protocols::{AsBroadcaster, AsParticipant, BroadcasterRef, ChatParticipant, ParticipantRef};

// -- Internal messages --

send_messages! {
    Deliver { from: String, text: String };
    SayToRoom { text: String };
    JoinRoom { room: BroadcasterRef }
}

// -- Protocol bridge (ChatParticipant) --

protocol_impl! {
    ChatParticipant for ActorRef<User> {
        send fn deliver(from: String, text: String) => Deliver;
    }
}

impl AsParticipant for ActorRef<User> {
    fn as_participant(&self) -> ParticipantRef {
        Arc::new(self.clone())
    }
}

// -- Caller API --

pub trait UserActions {
    fn say(&self, text: String) -> Result<(), ActorError>;
    fn join_room(&self, room: impl AsBroadcaster) -> Result<(), ActorError>;
}

impl UserActions for ActorRef<User> {
    fn say(&self, text: String) -> Result<(), ActorError> {
        self.send(SayToRoom { text })
    }

    fn join_room(&self, room: impl AsBroadcaster) -> Result<(), ActorError> {
        self.send(JoinRoom { room: room.as_broadcaster() })
    }
}

// -- Actor --

pub struct User {
    name: String,
    room: Option<BroadcasterRef>,
}

impl Actor for User {}

#[actor]
impl User {
    pub fn new(name: String) -> Self {
        Self { name, room: None }
    }

    #[send_handler]
    async fn handle_join_room(&mut self, msg: JoinRoom, ctx: &Context<Self>) {
        let _ = msg
            .room
            .add_member(self.name.clone(), ctx.actor_ref().as_participant());
        self.room = Some(msg.room);
    }

    #[send_handler]
    async fn handle_say_to_room(&mut self, msg: SayToRoom, _ctx: &Context<Self>) {
        if let Some(ref room) = self.room {
            let _ = room.say(self.name.clone(), msg.text);
        }
    }

    #[send_handler]
    async fn handle_deliver(&mut self, msg: Deliver, _ctx: &Context<Self>) {
        tracing::info!("[{}] got: {} says '{}'", self.name, msg.from, msg.text);
    }
}
