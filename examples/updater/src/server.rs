use std::time::Duration;

use spawned_concurrency::{
    messages::Unused,
    tasks::{
        send_interval, MessageResult, Actor, ActorRef,
        InitResult::{self, Success},
    },
};
use spawned_rt::tasks::CancellationToken;

use crate::messages::{UpdaterInMessage as InMessage, UpdaterOutMessage as OutMessage};

type UpdateServerHandle = ActorRef<UpdaterServer>;

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

impl Actor for UpdaterServer {
    type Request = Unused;
    type Message = InMessage;
    type Reply = OutMessage;
    type Error = std::fmt::Error;

    // Initializing Actor to start periodic checks.
    async fn init(
        mut self,
        handle: &ActorRef<Self>,
    ) -> Result<InitResult<Self>, Self::Error> {
        let timer = send_interval(self.periodicity, handle.clone(), InMessage::Check);
        self.timer_token = Some(timer.cancellation_token);
        Ok(Success(self))
    }

    async fn handle_message(
        &mut self,
        message: Self::Message,
        _handle: &UpdateServerHandle,
    ) -> MessageResult {
        match message {
            Self::Message::Check => {
                let url = self.url.clone();
                tracing::info!("Fetching: {url}");
                let resp = req(url).await;
                tracing::info!("Response: {resp:?}");
                MessageResult::NoReply
            }
        }
    }
}

async fn req(url: String) -> Result<String, reqwest::Error> {
    reqwest::get(url).await?.text().await
}
