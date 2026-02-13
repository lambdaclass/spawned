use spawned_concurrency::actor_api;
use spawned_concurrency::send_messages;
use spawned_concurrency::request_messages;
use spawned_concurrency::tasks::{Actor, ActorRef, Context, Handler, Recipient};
use spawned_macros::actor;

// -- Messages --

send_messages! {
    Say { from: String, text: String };
    Deliver { from: String, text: String };
    Join { name: String, inbox: Recipient<Deliver> }
}

request_messages! {
    Members -> Vec<String>
}

// -- API --

actor_api! {
    pub ChatRoomApi for ActorRef<ChatRoom> {
        send fn say(from: String, text: String) => Say;
        send fn add_member(name: String, inbox: Recipient<Deliver>) => Join;
        request async fn members() -> Vec<String> => Members;
    }
}

// -- Actor --

pub struct ChatRoom {
    members: Vec<(String, Recipient<Deliver>)>,
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
    async fn handle_say(&mut self, msg: Say, _ctx: &Context<Self>) {
        for (name, inbox) in &self.members {
            if *name != msg.from {
                let _ = inbox.send(Deliver {
                    from: msg.from.clone(),
                    text: msg.text.clone(),
                });
            }
        }
    }

    #[send_handler]
    async fn handle_join(&mut self, msg: Join, _ctx: &Context<Self>) {
        tracing::info!("[room] {} joined", msg.name);
        self.members.push((msg.name, msg.inbox));
    }

    #[request_handler]
    async fn handle_members(&mut self, _msg: Members, _ctx: &Context<Self>) -> Vec<String> {
        self.members.iter().map(|(name, _)| name.clone()).collect()
    }
}
