use std::collections::HashMap;

use spawned_concurrency::{
    CallResponse, CastResponse, GenServer, GenServerError, GenServerHandle, GenServerInMsg,
};
use spawned_rt::mpsc::Sender;

use crate::messages::{NameServerInMessage as InMessage, NameServerOutMessage as OutMessage};

type NameServerHandle = GenServerHandle<InMessage, OutMessage>;
type NameServerMessage = GenServerInMsg<InMessage, OutMessage>;
type NameServerState = HashMap<String, String>;

pub struct NameServer {
    state: NameServerState,
}

impl NameServer {
    pub async fn add(server: &mut NameServerHandle, key: String, value: String) -> OutMessage {
        match server.call(InMessage::Add { key, value }).await {
            Ok(_) => OutMessage::Ok,
            Err(GenServerError::ServerError) => OutMessage::ServerError,
            Err(GenServerError::CallbackError) => OutMessage::CallbackError,
        }
    }

    pub async fn find(server: &mut NameServerHandle, key: String) -> OutMessage {
        server
            .call(InMessage::Find { key })
            .await
            .unwrap_or(OutMessage::ServerError)
    }
}

impl GenServer for NameServer {
    type InMsg = InMessage;
    type OutMsg = OutMessage;
    type Error = std::fmt::Error;
    type State = NameServerState;

    fn init() -> Self {
        Self {
            state: HashMap::new(),
        }
    }

    fn state(&self) -> Self::State {
        self.state.clone()
    }

    fn set_state(&mut self, state: Self::State) {
        self.state = state;
    }

    fn handle_call(
        &mut self,
        message: InMessage,
        _tx: &Sender<NameServerMessage>,
    ) -> CallResponse<Self::OutMsg> {
        match message.clone() {
            Self::InMsg::Add { key, value } => {
                self.state.insert(key.clone(), value);
                if key == "error" {
                    panic!("error!")
                } else {
                    CallResponse::Reply(Self::OutMsg::Ok)
                }
            }
            Self::InMsg::Find { key } => match self.state.get(&key) {
                Some(value) => CallResponse::Reply(Self::OutMsg::Found {
                    value: value.to_string(),
                }),
                None => CallResponse::Reply(Self::OutMsg::NotFound),
            },
        }
    }

    fn handle_cast(
        &mut self,
        _message: InMessage,
        _tx: &Sender<NameServerMessage>,
    ) -> CastResponse {
        CastResponse::NoReply
    }
}
