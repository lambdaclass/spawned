use std::time::Duration;

use spawned_concurrency::{
    CallResponse, CastResponse, GenServer, GenServerHandle, GenServerInMsg, send_after,
};
use spawned_rt::mpsc::Sender;

use crate::messages::{UpdaterInMessage as InMessage, UpdaterOutMessage as OutMessage};

type UpdateServerHandle = GenServerHandle<UpdaterServer>;
type UpdateServerMessage = GenServerInMsg<UpdaterServer>;

#[derive(Clone)]
pub struct UpdateServerState {
    pub url: String,
    pub periodicity: Duration,
}
pub struct UpdaterServer {}

impl UpdaterServer {
    pub async fn check(server: &mut UpdateServerHandle) -> OutMessage {
        match server.cast(InMessage::Check).await {
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
        state: &mut Self::State,
    ) -> CastResponse {
        match message {
            Self::InMsg::Check => {
                send_after(state.periodicity, tx.clone(), InMessage::Check);
                let url = state.url.clone();
                tracing::info!("Fetching: {url}");
                let resp = req(url).await;

                tracing::info!("Response: {resp:?}");

                CastResponse::NoReply
            }
        }
    }
}

async fn req(url: String) -> Result<String, reqwest::Error> {
    reqwest::get(url).await?.text().await
}
