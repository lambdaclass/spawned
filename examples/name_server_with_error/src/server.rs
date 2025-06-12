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
        state: &mut Self::State,
    ) -> CallResponse<Self::OutMsg> {
        match message.clone() {
            Self::CallMsg::Add { key, value } => {
                state.insert(key.clone(), value);
                if key == "error" {
                    panic!("error!")
                } else {
                    CallResponse::Reply(Self::OutMsg::Ok)
                }
            }
            Self::CallMsg::Find { key } => match state.get(&key) {
                Some(value) => CallResponse::Reply(Self::OutMsg::Found {
                    value: value.to_string(),
                }),
                None => CallResponse::Reply(Self::OutMsg::NotFound),
            },
        }
    }

    async fn handle_cast(
        &mut self,
        _message: Self::CastMsg,
        _handle: &NameServerHandle,
        _state: &mut Self::State,
    ) -> CastResponse {
        CastResponse::NoReply
    }
}
