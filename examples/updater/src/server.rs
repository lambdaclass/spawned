use std::time::Duration;

use spawned_concurrency::tasks::{send_interval, Actor, Context, Handler, TimerHandle};

use crate::messages::Check;

pub struct UpdaterServer {
    pub url: String,
    pub periodicity: Duration,
    pub timer: Option<TimerHandle>,
}

impl UpdaterServer {
    pub fn new(url: String, periodicity: Duration) -> Self {
        UpdaterServer {
            url,
            periodicity,
            timer: None,
        }
    }
}

impl Actor for UpdaterServer {
    async fn started(&mut self, ctx: &Context<Self>) {
        let timer = send_interval(self.periodicity, ctx.clone(), Check);
        self.timer = Some(timer);
    }
}

impl Handler<Check> for UpdaterServer {
    async fn handle(&mut self, _msg: Check, _ctx: &Context<Self>) {
        let url = self.url.clone();
        tracing::info!("Fetching: {url}");
        let resp = req(url).await;
        tracing::info!("Response: {resp:?}");
    }
}

async fn req(url: String) -> Result<String, reqwest::Error> {
    reqwest::get(url).await?.text().await
}
