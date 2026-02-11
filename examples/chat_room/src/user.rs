use spawned_concurrency::tasks::{Actor, Context, Handler, Recipient};
use spawned_macros::actor;

use crate::messages::{Deliver, Say, SayToRoom};

pub struct User {
    pub name: String,
    pub room: Recipient<Say>,
}

impl Actor for User {}

#[actor]
impl User {
    #[handler]
    async fn on_say_to_room(&mut self, msg: SayToRoom, _ctx: &Context<Self>) {
        let _ = self.room.send(Say {
            from: self.name.clone(),
            text: msg.text,
        });
    }

    #[handler]
    async fn on_deliver(&mut self, msg: Deliver, _ctx: &Context<Self>) {
        tracing::info!("[{}] got: {} says '{}'", self.name, msg.from, msg.text);
    }
}
