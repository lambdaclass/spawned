use std::time::Duration;

use spawned_concurrency::tasks::{
    send_after, CallResponse, CastResponse, GenServer, GenServerHandle,
};

use crate::messages::{UpdaterInMessage as InMessage, UpdaterOutMessage as OutMessage};

type UpdateServerHandle = GenServerHandle<UpdaterServer>;

#[derive(Clone)]
pub struct UpdateServerState {
    pub url: String,
    pub periodicity: Duration,
}
pub struct UpdaterServer {}

impl GenServer for UpdaterServer {
    type CallMsg = ();
    type CastMsg = InMessage;
    type OutMsg = OutMessage;
    type Error = std::fmt::Error;
    type State = UpdateServerState;

    fn new() -> Self {
        Self {}
    }

    // Initializing GenServer to start periodic checks
    async fn init(
        &mut self,
        handle: &GenServerHandle<Self>,
        state: Self::State,
    ) -> Result<Self::State, Self::Error> {
        send_after(state.periodicity, handle.clone(), InMessage::Check);
        Ok(state)
    }

    async fn handle_call(
        &mut self,
        _message: Self::CallMsg,
        _handle: &UpdateServerHandle,
        state: Self::State,
    ) -> CallResponse<Self> {
        CallResponse::Reply(state, OutMessage::Ok)
    }

    async fn handle_cast(
        &mut self,
        message: Self::CastMsg,
        handle: &UpdateServerHandle,
        state: Self::State,
    ) -> CastResponse<Self> {
        match message {
            Self::CastMsg::Check => {
                send_after(state.periodicity, handle.clone(), InMessage::Check);
                let url = state.url.clone();
                tracing::info!("Fetching: {url}");
                let resp = req(url).await;

                tracing::info!("Response: {resp:?}");

                CastResponse::NoReply(state)
            }
        }
    }
}

async fn req(url: String) -> Result<String, reqwest::Error> {
    reqwest::get(url).await?.text().await
}
