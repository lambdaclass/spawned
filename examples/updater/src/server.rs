use std::{collections::HashMap, time::Duration};

use spawned_concurrency::{
    CallResponse, CastResponse, GenServer, GenServerHandle, GenServerInMsg, send_after,
};
use spawned_rt::mpsc::Sender;

use crate::messages::{UpdaterInMessage as InMessage, UpdaterOutMessage as OutMessage};

type UpdateServerHandle = GenServerHandle<UpdaterServer>;
type UpdateServerMessage = GenServerInMsg<UpdaterServer>;
type UpdateServerState = HashMap<String, String>;

pub struct UpdaterServer {}

impl UpdaterServer {
    pub async fn check(server: &mut UpdateServerHandle, url: String) -> OutMessage {
        match server.cast(InMessage::Check(url)).await {
            Ok(_) => OutMessage::Ok,
            Err(_) => OutMessage::Error,
        }
    }
}

impl GenServer for UpdaterServer {
    type InMsg = InMessage;
    type OutMsg = OutMessage;
    type Error = std::fmt::Error;
    type State = UpdateServerState;

    fn new() -> Self {
        Self {}
    }

    fn initial_state(&self) -> Self::State {
        HashMap::new()
    }

    async fn handle_call(
        &mut self,
        _message: InMessage,
        _tx: &Sender<UpdateServerMessage>,
        _state: &mut Self::State,
    ) -> CallResponse<Self::OutMsg> {
        CallResponse::Reply(OutMessage::Ok)
    }

    async fn handle_cast(
        &mut self,
        message: InMessage,
        tx: &Sender<UpdateServerMessage>,
        _state: &mut Self::State,
    ) -> CastResponse {
        match message {
            Self::InMsg::Check(url) => {
                send_after(
                    Duration::from_millis(1000),
                    tx.clone(),
                    InMessage::Check("url".to_string()),
                );
                tracing::info!("Fetching: {url:?}");
                let resp = req().await;

                tracing::info!("Response: {resp:?}");

                CastResponse::NoReply
            }
        }
    }
}

async fn req() -> Result<String, reqwest::Error> {
    reqwest::get("https://httpbin.org/ip")
        .await?
        .text()
        .await
}
