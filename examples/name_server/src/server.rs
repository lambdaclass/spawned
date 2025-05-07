use std::collections::HashMap;

use spawned_concurrency::{CallResponse, CastResponse, GenServer, GenServerHandle, GenServerInMsg};
use spawned_rt::mpsc::Sender;

use crate::messages::{NameServerInMessage as InMessage, NameServerOutMessage as OutMessage};

type NameServerHandle = GenServerHandle<NameServer>;
type NameServerMessage = GenServerInMsg<NameServer>;
type NameServerState = HashMap<String, String>;

pub struct NameServer {}

impl NameServer {
    pub async fn add(server: &mut NameServerHandle, key: String, value: String) -> OutMessage {
        match server.call(InMessage::Add { key, value }).await {
            Ok(_) => OutMessage::Ok,
            Err(_) => OutMessage::Error,
        }
    }

    pub async fn find(server: &mut NameServerHandle, key: String) -> OutMessage {
        server
            .call(InMessage::Find { key })
            .await
            .unwrap_or(OutMessage::Error)
    }
}

impl GenServer for NameServer {
    type InMsg = InMessage;
    type OutMsg = OutMessage;
    type Error = std::fmt::Error;
    type State = NameServerState;

    fn new() -> Self {
        Self {}
    }

    fn initial_state(&self) -> Self::State {
        HashMap::new()
    }

    async fn handle_call(
        &mut self,
        message: InMessage,
        _tx: &Sender<NameServerMessage>,
        state: &mut Self::State,
    ) -> CallResponse<Self::OutMsg> {
        match message.clone() {
            Self::InMsg::Add { key, value } => {
                state.insert(key, value);
                CallResponse::Reply(Self::OutMsg::Ok)
            }
            Self::InMsg::Find { key } => match state.get(&key) {
                Some(value) => CallResponse::Reply(Self::OutMsg::Found {
                    value: value.to_string(),
                }),
                None => CallResponse::Reply(Self::OutMsg::NotFound),
            },
        }
    }

    async fn handle_cast(
        &mut self,
        _message: InMessage,
        _tx: &Sender<NameServerMessage>,
        _state: &mut Self::State,
    ) -> CastResponse {
        CastResponse::NoReply
    }
}
