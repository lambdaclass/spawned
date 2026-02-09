//! Example to test a recurrent Actor.
//!
//! Just activates periodically and performs an http request
//!

mod messages;
mod server;

use std::{thread, time::Duration};

use server::UpdaterServer;
use spawned_concurrency::tasks::ActorStart as _;
use spawned_rt::tasks as rt;

fn main() {
    rt::run(async {
        tracing::info!("Starting Updater");
        UpdaterServer::new(
            "https://httpbin.org/ip".to_string(),
            Duration::from_millis(1000),
        )
        .start();

        // giving it some time before ending
        thread::sleep(Duration::from_secs(10));
        tracing::info!("Updater stopped");
    })
}
