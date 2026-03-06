use spawned_concurrency::threads::{Actor, Context, Handler};
use spawned_concurrency::actor;

use crate::protocols::user_protocol::{Deliver, JoinRoom, Say};
use crate::protocols::{RoomRef, ToUserRef, UserProtocol};

pub struct User {
    name: String,
    room: Option<RoomRef>,
}

#[actor(protocol = UserProtocol)]
impl User {
    pub fn new(name: String) -> Self {
        Self { name, room: None }
    }

    #[send_handler]
    fn handle_deliver(&mut self, msg: Deliver, _ctx: &Context<Self>) {
        tracing::info!("[{}] got: {} says '{}'", self.name, msg.from, msg.text);
    }

    #[send_handler]
    fn handle_say(&mut self, msg: Say, _ctx: &Context<Self>) {
        if let Some(ref room) = self.room {
            let _ = room.say(self.name.clone(), msg.text);
        }
    }

    #[send_handler]
    fn handle_join_room(&mut self, msg: JoinRoom, ctx: &Context<Self>) {
        let _ = msg
            .room
            .add_member(self.name.clone(), ctx.actor_ref().to_user_ref());
        self.room = Some(msg.room);
    }
}
