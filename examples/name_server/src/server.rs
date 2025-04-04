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
    type Error = std::fmt::Error;
    type State = HashMap<String, String>;

    fn init() -> Self {
        Self { state: HashMap::new() }
    }

    fn state(&self) -> Self::State {
        self.state.clone()
    }

    fn set_state(&mut self, state: Self::State) {
        self.state = state;
    }

    async fn handle(
        &mut self,
        message: InMessage,
        _tx: &Sender<NameServerMessage>,
    ) -> Result<Self::OutMsg, Self::Error> {
        match message.clone() {
            Self::InMsg::Add { key, value } => {
                self.state.insert(key, value);
                Ok::<Self::OutMsg, Self::Error>(Self::OutMsg::Ok)
            }
            Self::InMsg::Find { key } => {
                Ok(match self.state.get(&key) {
                    Some(value) => Self::OutMsg::Found { value: value.to_string() },
                    None => Self::OutMsg::NotFound,
                })
            }
        }
    }
}
