use std::time::Duration;

use spawned_concurrency::{
    messages::Unused,
    threads::{send_after, Actor, ActorRef, InitResult, MessageResponse},
};
use spawned_rt::threads::block_on;

use crate::messages::{UpdaterInMessage as InMessage, UpdaterOutMessage as OutMessage};

type UpdateServerHandle = ActorRef<UpdaterServer>;

#[derive(Clone)]
pub struct UpdaterServer {
    pub url: String,
    pub periodicity: Duration,
}

impl Actor for UpdaterServer {
    type Request = Unused;
    type Message = InMessage;
    type Reply = OutMessage;
    type Error = std::fmt::Error;

    // Initializing Actor to start periodic checks.
    fn init(self, handle: &ActorRef<Self>) -> Result<InitResult<Self>, Self::Error> {
        send_after(self.periodicity, handle.clone(), InMessage::Check);
        Ok(InitResult::Success(self))
    }

    fn handle_message(
        &mut self,
        message: Self::Message,
        handle: &UpdateServerHandle,
    ) -> MessageResponse {
        match message {
            Self::Message::Check => {
                send_after(self.periodicity, handle.clone(), InMessage::Check);
                let url = self.url.clone();
                tracing::info!("Fetching: {url}");
                let resp = block_on(req(url));

                tracing::info!("Response: {resp:?}");

                MessageResponse::NoReply
            }
        }
    }
}

async fn req(url: String) -> Result<String, reqwest::Error> {
    reqwest::get(url).await?.text().await
}
