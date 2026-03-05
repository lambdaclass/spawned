use std::time::Duration;

use spawned_concurrency::threads::{send_after, Actor, Context, Handler};
use spawned_concurrency::actor;
use spawned_rt::threads::block_on;

use crate::protocols::updater_protocol::Check;
use crate::protocols::UpdaterProtocol;

pub struct UpdaterServer {
    pub url: String,
    pub periodicity: Duration,
}

#[actor(protocol = UpdaterProtocol)]
impl UpdaterServer {
    #[started]
    fn started(&mut self, ctx: &Context<Self>) {
        send_after(self.periodicity, ctx.clone(), Check);
    }

    #[send_handler]
    fn handle_check(&mut self, _msg: Check, ctx: &Context<Self>) {
        send_after(self.periodicity, ctx.clone(), Check);
        let url = self.url.clone();
        tracing::info!("Fetching: {url}");
        let resp = block_on(req(url));
        tracing::info!("Response: {resp:?}");
    }
}

async fn req(url: String) -> Result<String, reqwest::Error> {
    reqwest::get(url).await?.text().await
}
