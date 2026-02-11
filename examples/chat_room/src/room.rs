use spawned_concurrency::tasks::{Actor, Context, Handler, Recipient};
use spawned_macros::actor;

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

#[actor]
impl ChatRoom {
    #[handler]
    async fn on_say(&mut self, msg: Say, _ctx: &Context<Self>) {
        for (name, inbox) in &self.members {
            if *name != msg.from {
                let _ = inbox.send(Deliver {
                    from: msg.from.clone(),
                    text: msg.text.clone(),
                });
            }
        }
    }

    #[handler]
    async fn on_join(&mut self, msg: Join, _ctx: &Context<Self>) {
        tracing::info!("[room] {} joined", msg.name);
        self.members.push((msg.name, msg.inbox));
    }
}
