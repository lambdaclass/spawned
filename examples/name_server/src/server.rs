use std::collections::HashMap;

use spawned_concurrency::{
    messages::Unused,
    tasks::{Actor, ActorRef, RequestResponse},
};

use crate::messages::{NameServerInMessage as InMessage, NameServerOutMessage as OutMessage};

type NameServerHandle = ActorRef<NameServer>;

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
    pub async fn add(server: &mut NameServerHandle, key: String, value: String) -> OutMessage {
        match server.request(InMessage::Add { key, value }).await {
            Ok(_) => OutMessage::Ok,
            Err(_) => OutMessage::Error,
        }
    }

    pub async fn find(server: &mut NameServerHandle, key: String) -> OutMessage {
        server
            .request(InMessage::Find { key })
            .await
            .unwrap_or(OutMessage::Error)
    }
}

impl Actor for NameServer {
    type Request = InMessage;
    type Message = Unused;
    type Reply = OutMessage;
    type Error = std::fmt::Error;

    async fn handle_request(
        &mut self,
        message: Self::Request,
        _handle: &NameServerHandle,
    ) -> RequestResponse<Self> {
        match message.clone() {
            Self::Request::Add { key, value } => {
                self.inner.insert(key, value);
                RequestResponse::Reply(Self::Reply::Ok)
            }
            Self::Request::Find { key } => match self.inner.get(&key) {
                Some(result) => {
                    let value = result.to_string();
                    RequestResponse::Reply(Self::Reply::Found { value })
                }
                None => RequestResponse::Reply(Self::Reply::NotFound),
            },
        }
    }
}
