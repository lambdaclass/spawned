use std::time::Duration;

use spawned_concurrency::{
    messages::Unused,
    threads::{send_after, CastResponse, GenServer, GenServerHandle},
};
use spawned_rt::threads::block_on;

use crate::messages::{UpdaterInMessage as InMessage, UpdaterOutMessage as OutMessage};

type UpdateServerHandle = GenServerHandle<UpdaterServer>;

#[derive(Default, Clone)]
pub struct UpdaterServer {
    pub url: String,
    pub periodicity: Duration,
}

impl GenServer for UpdaterServer {
    type CallMsg = Unused;
    type CastMsg = InMessage;
    type OutMsg = OutMessage;
    type Error = std::fmt::Error;

    // Initializing GenServer to start periodic checks.
    fn init(self, handle: &GenServerHandle<Self>) -> Result<Self, Self::Error> {
        send_after(self.periodicity, handle.clone(), InMessage::Check);
        Ok(self)
    }

    fn handle_cast(
        self,
        message: Self::CastMsg,
        handle: &UpdateServerHandle,
    ) -> CastResponse<Self> {
        match message {
            Self::CastMsg::Check => {
                send_after(self.periodicity, handle.clone(), InMessage::Check);
                let url = self.url.clone();
                tracing::info!("Fetching: {url}");
                let resp = block_on(req(url));

                tracing::info!("Response: {resp:?}");

                CastResponse::NoReply(self)
            }
        }
    }
}

async fn req(url: String) -> Result<String, reqwest::Error> {
    reqwest::get(url).await?.text().await
}
