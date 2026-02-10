//! User actor — knows about `Say`, `SayToRoom`, `Deliver`, and `Recipient<Say>`.
//! Does NOT know about the `ChatRoom` type.

use spawned_concurrency::tasks::{Actor, Context, Handler, Recipient};

use crate::messages::{Deliver, Say, SayToRoom};

pub struct User {
    pub name: String,
    pub room: Recipient<Say>,
}

impl Actor for User {}

impl Handler<SayToRoom> for User {
    async fn handle(&mut self, msg: SayToRoom, _ctx: &Context<Self>) {
        // Forward to the room via Recipient<Say> — no ChatRoom type needed
        let _ = self.room.send(Say {
            from: self.name.clone(),
            text: msg.text,
        });
    }
}

impl Handler<Deliver> for User {
    async fn handle(&mut self, msg: Deliver, _ctx: &Context<Self>) {
        tracing::info!("[{}] got message from {}: {}", self.name, msg.from, msg.text);
    }
}
