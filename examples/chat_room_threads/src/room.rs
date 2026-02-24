use spawned_concurrency::threads::{Actor, Context, Handler};
use spawned_macros::actor;

use crate::protocols::room_protocol::{AddMember, Members, Say};
use crate::protocols::{RoomProtocol, UserRef};

pub struct ChatRoom {
    members: Vec<(String, UserRef)>,
}

#[actor(protocol = RoomProtocol)]
impl ChatRoom {
    pub fn new() -> Self {
        Self {
            members: Vec::new(),
        }
    }

    #[send_handler]
    fn handle_say(&mut self, msg: Say, _ctx: &Context<Self>) {
        tracing::info!("[room] {} says: {}", msg.from, msg.text);
        for (name, user) in &self.members {
            if *name != msg.from {
                let _ = user.deliver(msg.from.clone(), msg.text.clone());
            }
        }
    }

    #[send_handler]
    fn handle_add_member(&mut self, msg: AddMember, _ctx: &Context<Self>) {
        tracing::info!("[room] {} joined", msg.name);
        self.members.push((msg.name, msg.user));
    }

    #[request_handler]
    fn handle_members(&mut self, _msg: Members, _ctx: &Context<Self>) -> Vec<String> {
        self.members.iter().map(|(name, _)| name.clone()).collect()
    }
}
