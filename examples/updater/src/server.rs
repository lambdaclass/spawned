use std::time::Duration;

use spawned_concurrency::tasks::{send_interval, Actor, Context, Handler};
use spawned_macros::actor;
use spawned_rt::tasks::CancellationToken;

use crate::protocols::updater_protocol::Check;
use crate::protocols::UpdaterProtocol;

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

#[actor(protocol = UpdaterProtocol)]
impl UpdaterServer {
    #[started]
    async fn started(&mut self, ctx: &Context<Self>) {
        let timer = send_interval(self.periodicity, ctx.clone(), Check);
        self.timer_token = Some(timer.cancellation_token);
    }

    #[send_handler]
    async fn handle_check(&mut self, _msg: Check, _ctx: &Context<Self>) {
        let url = self.url.clone();
        tracing::info!("Fetching: {url}");
        let resp = req(url).await;
        tracing::info!("Response: {resp:?}");
    }
}

async fn req(url: String) -> Result<String, reqwest::Error> {
    reqwest::get(url).await?.text().await
}
