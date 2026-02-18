use spawned_concurrency::error::ActorError;
use spawned_concurrency::tasks::{Actor, ActorRef, Context, Handler, Recipient};
use spawned_macros::actor;

// -- ChatRoom messages --

spawned_concurrency::send_messages! {
    Say { from: String, text: String };
    Deliver { from: String, text: String };
    Join { name: String, inbox: Recipient<Deliver> }
}

spawned_concurrency::request_messages! {
    Members -> Vec<String>
}

// -- ChatRoom actor --

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

pub trait ChatRoomApi {
    fn say(&self, from: String, text: String) -> Result<(), ActorError>;
    fn add_member(&self, name: String, inbox: Recipient<Deliver>) -> Result<(), ActorError>;
    async fn members(&self) -> Result<Vec<String>, ActorError>;
}

impl ChatRoomApi for ActorRef<ChatRoom> {
    fn say(&self, from: String, text: String) -> Result<(), ActorError> {
        self.send(Say { from, text })
    }

    fn add_member(&self, name: String, inbox: Recipient<Deliver>) -> Result<(), ActorError> {
        self.send(Join { name, inbox })
    }

    async fn members(&self) -> Result<Vec<String>, ActorError> {
        self.request(Members).await
    }
}

impl Actor for ChatRoom {}

#[actor]
impl ChatRoom {
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
