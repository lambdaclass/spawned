use std::time::Duration;

use spawned_concurrency::{
    messages::Unused,
    tasks::{send_interval, CastResponse, GenServer, GenServerHandle},
};
use spawned_rt::tasks::CancellationToken;

use crate::messages::{UpdaterInMessage as InMessage, UpdaterOutMessage as OutMessage};

type UpdateServerHandle = GenServerHandle<UpdaterServer>;

#[derive(Clone)]
pub struct UpdaterServer {
    pub url: String,
    pub periodicity: Duration,
    pub timer_token: Option<CancellationToken>,
}

impl UpdaterServer {
    pub fn new(url: String, periodicity: Duration) -> Self {
        UpdaterServer {
            url,
            periodicity,
            timer_token: None,
        }
    }
}

impl GenServer for UpdaterServer {
    type CallMsg = Unused;
    type CastMsg = InMessage;
    type OutMsg = OutMessage;
    type Error = std::fmt::Error;

    // Initializing GenServer to start periodic checks.
    async fn init(mut self, handle: &GenServerHandle<Self>) -> Result<Self, Self::Error> {
        let timer = send_interval(self.periodicity, handle.clone(), InMessage::Check);
        self.timer_token = Some(timer.cancellation_token);
        Ok(self)
    }

    async fn handle_cast(
        self,
        message: Self::CastMsg,
        _handle: &UpdateServerHandle,
    ) -> CastResponse<Self> {
        match message {
            Self::CastMsg::Check => {
                //send_after(state.periodicity, handle.clone(), InMessage::Check);
                let url = self.url.clone();
                tracing::info!("Fetching: {url}");
                let resp = req(url).await;
                tracing::info!("Response: {resp:?}");
                CastResponse::NoReply(self)
            }
        }
    }
}

async fn req(url: String) -> Result<String, reqwest::Error> {
    reqwest::get(url).await?.text().await
}
