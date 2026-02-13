use spawned_concurrency::actor_api;
use spawned_concurrency::send_messages;
use spawned_concurrency::threads::{Actor, ActorRef, Context, Handler};
use spawned_macros::actor;

use crate::room::{ChatRoom, ChatRoomApi, Deliver};

// -- Messages --

send_messages! {
    SayToRoom { text: String };
    JoinRoom { room: ActorRef<ChatRoom> }
}

// -- API --

actor_api! {
    pub UserApi for ActorRef<User> {
        send fn say(text: String) => SayToRoom;
        send fn join_room(room: ActorRef<ChatRoom>) => JoinRoom;
    }
}

// -- Actor --

pub struct User {
    pub name: String,
    room: Option<ActorRef<ChatRoom>>,
}

impl Actor for User {}

#[actor]
impl User {
    pub fn new(name: String) -> Self {
        Self { name, room: None }
    }

    #[send_handler]
    fn handle_say_to_room(&mut self, msg: SayToRoom, _ctx: &Context<Self>) {
        if let Some(ref room) = self.room {
            let _ = room.say(self.name.clone(), msg.text);
        }
    }

    #[send_handler]
    fn handle_join_room(&mut self, msg: JoinRoom, ctx: &Context<Self>) {
        let _ = msg.room.add_member(self.name.clone(), ctx.recipient::<Deliver>());
        self.room = Some(msg.room);
    }

    #[send_handler]
    fn handle_deliver(&mut self, msg: Deliver, _ctx: &Context<Self>) {
        tracing::info!("[{}] got: {} says '{}'", self.name, msg.from, msg.text);
    }
}
