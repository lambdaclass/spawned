use spawned_concurrency::threads::{Actor, Context, Handler};
use spawned_macros::actor;

use crate::protocols::user_protocol::{Deliver, JoinRoom, Speak};
use crate::protocols::{AsUser, RoomRef};

pub struct User {
    name: String,
    room: Option<RoomRef>,
}

impl Actor for User {}

#[actor]
impl User {
    pub fn new(name: String) -> Self {
        Self { name, room: None }
    }

    #[send_handler]
    fn handle_deliver(&mut self, msg: Deliver, _ctx: &Context<Self>) {
        tracing::info!("[{}] got: {} says '{}'", self.name, msg.from, msg.text);
    }

    #[send_handler]
    fn handle_speak(&mut self, msg: Speak, _ctx: &Context<Self>) {
        if let Some(ref room) = self.room {
            let _ = room.say(self.name.clone(), msg.text);
        }
    }

    #[send_handler]
    fn handle_join_room(&mut self, msg: JoinRoom, ctx: &Context<Self>) {
        let _ = msg
            .room
            .add_member(self.name.clone(), ctx.actor_ref().as_user());
        self.room = Some(msg.room);
    }
}
