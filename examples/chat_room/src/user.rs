use spawned_concurrency::error::ActorError;
use spawned_concurrency::tasks::{Actor, ActorRef, Context, Handler};
use spawned_macros::actor;

use crate::room::{ChatRoom, ChatRoomApi, Deliver};

// -- User messages --

spawned_concurrency::send_messages! {
    SayToRoom { text: String };
    JoinRoom { room: ActorRef<ChatRoom> }
}

pub struct User {
    pub name: String,
    room: Option<ActorRef<ChatRoom>>,
}

impl User {
    pub fn new(name: String) -> Self {
        Self { name, room: None }
    }
}

pub trait UserApi {
    fn say(&self, text: String) -> Result<(), ActorError>;
    fn join_room(&self, room: &ActorRef<ChatRoom>) -> Result<(), ActorError>;
}

impl UserApi for ActorRef<User> {
    fn say(&self, text: String) -> Result<(), ActorError> {
        self.send(SayToRoom { text })
    }

    fn join_room(&self, room: &ActorRef<ChatRoom>) -> Result<(), ActorError> {
        self.send(JoinRoom { room: room.clone() })
    }
}

impl Actor for User {}

#[actor]
impl User {
    #[send_handler]
    async fn handle_say_to_room(&mut self, msg: SayToRoom, _ctx: &Context<Self>) {
        if let Some(ref room) = self.room {
            let _ = room.say(self.name.clone(), msg.text);
        }
    }

    #[send_handler]
    async fn handle_join_room(&mut self, msg: JoinRoom, ctx: &Context<Self>) {
        let _ = msg.room.add_member(self.name.clone(), ctx.recipient::<Deliver>());
        self.room = Some(msg.room);
    }

    #[send_handler]
    async fn handle_deliver(&mut self, msg: Deliver, _ctx: &Context<Self>) {
        tracing::info!("[{}] got: {} says '{}'", self.name, msg.from, msg.text);
    }
}
