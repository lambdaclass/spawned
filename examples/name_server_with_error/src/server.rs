use std::collections::HashMap;

use spawned_concurrency::tasks::{
    CallResponse, CastResponse, GenServer, GenServerError, GenServerHandle,
};

use crate::messages::{NameServerInMessage as InMessage, NameServerOutMessage as OutMessage};

type NameServerHandle = GenServerHandle<NameServer>;
type NameServerState = HashMap<String, String>;

pub struct NameServer {}

impl NameServer {
    pub async fn add(server: &mut NameServerHandle, key: String, value: String) -> OutMessage {
        match server.call(InMessage::Add { key, value }).await {
            Ok(_) => OutMessage::Ok,
            Err(GenServerError::Callback) => OutMessage::CallbackError,
            Err(_) => OutMessage::ServerError,
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
    type CallMsg = InMessage;
    type CastMsg = ();
    type OutMsg = OutMessage;
    type Error = std::fmt::Error;
    type State = NameServerState;

    fn new() -> Self {
        NameServer {}
    }

    async fn handle_call(
        &mut self,
        message: Self::CallMsg,
        _handle: &NameServerHandle,
        mut state: Self::State,
    ) -> CallResponse<Self> {
        match message.clone() {
            Self::CallMsg::Add { key, value } => {
                state.insert(key.clone(), value);
                if key == "error" {
                    panic!("error!")
                } else {
                    CallResponse::Reply(state, Self::OutMsg::Ok)
                }
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

    async fn handle_cast(
        &mut self,
        _message: Self::CastMsg,
        _handle: &NameServerHandle,
        state: Self::State,
    ) -> CastResponse<Self> {
        CastResponse::NoReply(state)
    }
}
