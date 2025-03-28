use std::collections::HashMap;

use spawned_concurrency::{GenServerHandler, GenServer, GenServerMsg};
use spawned_rt::Sender;

use crate::messages::{NameServerInMessage as InMessage, NameServerOutMessage as OutMessage};

pub struct NameServer {
    state: HashMap<String, String>,
}

impl NameServer {
    pub async fn add(server: &mut GenServerHandler<GenServerMsg<InMessage, OutMessage>, OutMessage>, key: String, value: String) {
        server.rpc(InMessage::Add { key, value}).await;
    }

    pub async fn find(server: &mut GenServerHandler<GenServerMsg<InMessage, OutMessage>, OutMessage>, key: String) -> OutMessage {
        server.rpc(InMessage::Find { key }).await.unwrap()
    }
}

impl GenServer<InMessage, OutMessage> for NameServer {
    fn init() -> Self {
        Self { state: HashMap::new() }
    }

    async fn handle(
        &mut self,
        message: InMessage,
        _tx: &Sender<GenServerMsg<InMessage, OutMessage>>,
    ) -> OutMessage {
        match message.clone() {
            InMessage::Add { key, value } => {
                self.state.insert(key, value);
                OutMessage::Ok
            }
            InMessage::Find { key } => {
                match self.state.get(&key) {
                    Some(value) => OutMessage::Found { value: value.to_string() },
                    None => OutMessage::NotFound,
                }
            }
        }
    }
}
