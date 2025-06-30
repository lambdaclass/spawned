use std::time::Duration;

use spawned_concurrency::{
    messages::Unused,
    tasks::{send_interval, CastResponse, GenServer, GenServerHandle},
};
use spawned_rt::tasks::CancellationToken;

use crate::messages::{UpdaterInMessage as InMessage, UpdaterOutMessage as OutMessage};

type UpdateServerHandle = GenServerHandle<UpdaterServer>;

#[derive(Clone)]
pub struct UpdateServerState {
    pub url: String,
    pub periodicity: Duration,
    pub timer_token: Option<CancellationToken>,
}
pub struct UpdaterServer {}

impl GenServer for UpdaterServer {
    type CallMsg = Unused;
    type CastMsg = InMessage;
    type OutMsg = OutMessage;
    type Error = std::fmt::Error;
    type State = UpdateServerState;

    fn new() -> Self {
        Self {}
    }

    // Initializing GenServer to start periodic checks.
    async fn init(
        &mut self,
        handle: &GenServerHandle<Self>,
        mut state: Self::State,
    ) -> Result<Self::State, Self::Error> {
        let timer = send_interval(state.periodicity, handle.clone(), InMessage::Check);
        state.timer_token = Some(timer.cancellation_token);
        Ok(state)
    }

    async fn handle_cast(
        &mut self,
        message: Self::CastMsg,
        _handle: &UpdateServerHandle,
        state: Self::State,
    ) -> CastResponse<Self> {
        match message {
            Self::CastMsg::Check => {
                //send_after(state.periodicity, handle.clone(), InMessage::Check);
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
