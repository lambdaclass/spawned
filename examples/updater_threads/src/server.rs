use std::time::Duration;

use spawned_concurrency::threads::{
    send_after, CallResponse, CastResponse, GenServer, GenServerHandle,
};
use spawned_rt::threads::block_on;

use crate::messages::{UpdaterInMessage as InMessage, UpdaterOutMessage as OutMessage};

type UpdateServerHandle = GenServerHandle<UpdaterServer>;

#[derive(Clone)]
pub struct UpdateServerState {
    pub url: String,
    pub periodicity: Duration,
}
pub struct UpdaterServer {}

impl UpdaterServer {
    pub fn check(server: &mut UpdateServerHandle) -> OutMessage {
        match server.cast(InMessage::Check) {
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

    fn handle_call(
        &mut self,
        _message: InMessage,
        _handle: &UpdateServerHandle,
        _state: &mut Self::State,
    ) -> CallResponse<Self::OutMsg> {
        CallResponse::Reply(OutMessage::Ok)
    }

    fn handle_cast(
        &mut self,
        message: InMessage,
        handle: &UpdateServerHandle,
        state: &mut Self::State,
    ) -> CastResponse {
        match message {
            Self::InMsg::Check => {
                send_after(state.periodicity, handle.clone(), InMessage::Check);
                let url = state.url.clone();
                tracing::info!("Fetching: {url}");
                let resp = block_on(req(url));

                tracing::info!("Response: {resp:?}");

                CastResponse::NoReply
            }
        }
    }
}

async fn req(url: String) -> Result<String, reqwest::Error> {
    reqwest::get(url).await?.text().await
}
