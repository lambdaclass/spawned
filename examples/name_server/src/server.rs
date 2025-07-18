use std::collections::HashMap;

use spawned_concurrency::{
    messages::Unused,
    tasks::{CallResponse, GenServer, GenServerHandle},
};

use crate::messages::{NameServerInMessage as InMessage, NameServerOutMessage as OutMessage};

type NameServerHandle = GenServerHandle<NameServer>;
type NameServerState = HashMap<String, String>;

#[derive(Default)]
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
    type CallMsg = InMessage;
    type CastMsg = Unused;
    type OutMsg = OutMessage;
    type Error = std::fmt::Error;
    type State = NameServerState;

    async fn handle_call(
        &mut self,
        message: Self::CallMsg,
        _handle: &NameServerHandle,
        mut state: Self::State,
    ) -> CallResponse<Self> {
        match message.clone() {
            Self::CallMsg::Add { key, value } => {
                state.insert(key, value);
                CallResponse::Reply(state, Self::OutMsg::Ok)
            }
            Self::CallMsg::Find { key } => match state.get(&key) {
                Some(result) => {
                    let value = result.to_string();
                    CallResponse::Reply(state, Self::OutMsg::Found { value })
                }
                None => CallResponse::Reply(state, Self::OutMsg::NotFound),
            },
        }
    }
}
