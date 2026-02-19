//! ChatRoom actor — knows about protocol traits, not User (sync/threads version).

use spawned_concurrency::protocol_impl;
use spawned_concurrency::request_messages;
use spawned_concurrency::send_messages;
use spawned_concurrency::threads::{Actor, ActorRef, Context, Handler};
use spawned_macros::actor;
use std::sync::Arc;

use crate::protocols::{AsBroadcaster, BroadcasterRef, ChatBroadcaster, ParticipantRef};

// -- Internal messages --

send_messages! {
    Say { from: String, text: String };
    Join { name: String, participant: ParticipantRef }
}

request_messages! {
    Members -> Vec<String>
}

// -- Protocol bridge --

protocol_impl! {
    ChatBroadcaster for ActorRef<ChatRoom> {
        send fn say(from: String, text: String) => Say;
        send fn add_member(name: String, participant: ParticipantRef) => Join;
        request sync fn members() -> Vec<String> => Members;
    }
}

impl AsBroadcaster for ActorRef<ChatRoom> {
    fn as_broadcaster(&self) -> BroadcasterRef {
        Arc::new(self.clone())
    }
}

// -- Actor --

pub struct ChatRoom {
    members: Vec<(String, ParticipantRef)>,
}

impl Actor for ChatRoom {}

#[actor]
impl ChatRoom {
    pub fn new() -> Self {
        Self {
            members: Vec::new(),
        }
    }

    #[send_handler]
    fn handle_join(&mut self, msg: Join, _ctx: &Context<Self>) {
        tracing::info!("[room] {} joined", msg.name);
        self.members.push((msg.name, msg.participant));
    }

    #[send_handler]
    fn handle_say(&mut self, msg: Say, _ctx: &Context<Self>) {
        for (name, participant) in &self.members {
            if *name != msg.from {
                let _ = participant.deliver(msg.from.clone(), msg.text.clone());
            }
        }
    }

    #[request_handler]
    fn handle_members(&mut self, _msg: Members, _ctx: &Context<Self>) -> Vec<String> {
        self.members.iter().map(|(name, _)| name.clone()).collect()
    }
}
