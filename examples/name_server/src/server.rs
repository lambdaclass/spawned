use std::collections::HashMap;

use spawned_concurrency::tasks::{Actor, ActorRef, Context, Handler};

use crate::messages::{Add, Find, NameServerOutMessage as OutMessage};

pub struct NameServer {
    inner: HashMap<String, String>,
}

impl NameServer {
    pub fn new() -> Self {
        NameServer {
            inner: HashMap::new(),
        }
    }
}

impl NameServer {
    pub async fn add(server: &ActorRef<NameServer>, key: String, value: String) -> OutMessage {
        match server.send_request(Add { key, value }).await {
            Ok(_) => OutMessage::Ok,
            Err(_) => OutMessage::Error,
        }
    }

    pub async fn find(server: &ActorRef<NameServer>, key: String) -> OutMessage {
        server
            .send_request(Find { key })
            .await
            .unwrap_or(OutMessage::Error)
    }
}

impl Actor for NameServer {}

impl Handler<Add> for NameServer {
    async fn handle(&mut self, msg: Add, _ctx: &Context<Self>) -> OutMessage {
        self.inner.insert(msg.key, msg.value);
        OutMessage::Ok
    }
}

impl Handler<Find> for NameServer {
    async fn handle(&mut self, msg: Find, _ctx: &Context<Self>) -> OutMessage {
        match self.inner.get(&msg.key) {
            Some(result) => OutMessage::Found {
                value: result.to_string(),
            },
            None => OutMessage::NotFound,
        }
    }
}
