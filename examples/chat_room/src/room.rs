//! ChatRoom actor — knows about `Join`, `Say`, and `Recipient<Deliver>`.
//! Does NOT know about the `User` type.

use spawned_concurrency::tasks::{Actor, Context, Handler, Recipient};

use crate::messages::{Deliver, Join, Say};

pub struct ChatRoom {
    members: Vec<(String, Recipient<Deliver>)>,
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
                let _ = inbox.send(Deliver {
                    from: msg.from.clone(),
                    text: msg.text.clone(),
                });
            }
        }
    }
}
