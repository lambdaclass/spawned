//! Example to test a recurrent gen_server.
//!
//! Just activates periodically and performs an http request
//!

mod messages;
mod server;

use std::{thread, time::Duration};

use messages::UpdaterOutMessage;
use server::{UpdateServerState, UpdaterServer};
use spawned_concurrency::threads::GenServer as _;
use spawned_rt::threads as rt;

fn main() {
    rt::run(|| {
        let mut update_server = UpdaterServer::start(UpdateServerState {
            url: "https://httpbin.org/ip".to_string(),
            periodicity: Duration::from_millis(1000),
        });

        let result = UpdaterServer::check(&mut update_server);
        tracing::info!("Update check done: {result:?}");
        assert_eq!(result, UpdaterOutMessage::Ok);

        // giving it some time before ending
        thread::sleep(Duration::from_secs(10));
    })
}
