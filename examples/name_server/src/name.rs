use std::collections::HashMap;

use spawned_concurrency::{GenServerHandle, GenServer, GenServerMsg};
use spawned_rt::mpsc::Sender;

use crate::messages::{NameServerInMessage as InMessage, NameServerOutMessage as OutMessage};

type NameServerHandle = GenServerHandle<InMessage, OutMessage>;
type NameServerMessage = GenServerMsg<InMessage, OutMessage>;
pub struct NameServer {
    state: HashMap<String, String>,
}

impl NameServer {
    pub async fn add(server: &mut NameServerHandle, key: String, value: String) {
        server.rpc(InMessage::Add { key, value}).await;
    }

    pub async fn find(server: &mut NameServerHandle, key: String) -> OutMessage {
        server.rpc(InMessage::Find { key }).await.unwrap()
    }
}

impl GenServer for NameServer {
    type InMsg = InMessage;
    type OutMsg = OutMessage;

    fn init() -> Self {
        Self { state: HashMap::new() }
    }

    async fn handle(
        &mut self,
        message: InMessage,
        _tx: &Sender<NameServerMessage>,
    ) -> Self::OutMsg {
        match message.clone() {
            Self::InMsg::Add { key, value } => {
                self.state.insert(key, value);
                Self::OutMsg::Ok
            }
            Self::InMsg::Find { key } => {
                match self.state.get(&key) {
                    Some(value) => Self::OutMsg::Found { value: value.to_string() },
                    None => Self::OutMsg::NotFound,
                }
            }
        }
    }
}
