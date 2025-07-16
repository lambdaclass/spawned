use std::time::Duration;

use spawned_concurrency::{
    messages::Unused,
    threads::{send_after, CastResponse, GenServer, GenServerHandle},
};
use spawned_rt::threads::block_on;

use crate::messages::{UpdaterInMessage as InMessage, UpdaterOutMessage as OutMessage};

type UpdateServerHandle = GenServerHandle<UpdaterServer>;

#[derive(Clone)]
pub struct UpdateServerState {
    pub url: String,
    pub periodicity: Duration,
}
#[derive(Default)]
pub struct UpdaterServer {}

impl GenServer for UpdaterServer {
    type CallMsg = Unused;
    type CastMsg = InMessage;
    type OutMsg = OutMessage;
    type Error = std::fmt::Error;
    type State = UpdateServerState;

    // Initializing GenServer to start periodic checks.
    fn init(
        &mut self,
        handle: &GenServerHandle<Self>,
        state: Self::State,
    ) -> Result<Self::State, Self::Error> {
        send_after(state.periodicity, handle.clone(), InMessage::Check);
        Ok(state)
    }

    fn handle_cast(
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
                let resp = block_on(req(url));

                tracing::info!("Response: {resp:?}");

                CastResponse::NoReply(state)
            }
        }
    }
}

async fn req(url: String) -> Result<String, reqwest::Error> {
    reqwest::get(url).await?.text().await
}
