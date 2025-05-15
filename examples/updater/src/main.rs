//! Example to test a recurrent gen_server.
//!
//! Just activates periodically and performs an http request
<<<<<<< HEAD
//! 
=======
//!
>>>>>>> async_handlers

mod messages;
mod server;

use std::{thread, time::Duration};

use messages::UpdaterOutMessage;
<<<<<<< HEAD
use server::UpdaterServer;
=======
use server::{UpdateServerState, UpdaterServer};
>>>>>>> async_handlers
use spawned_concurrency::GenServer as _;
use spawned_rt as rt;

fn main() {
    rt::run(async {
<<<<<<< HEAD
        let mut update_server = UpdaterServer::start();

        let result = UpdaterServer::check(&mut update_server, "ea".to_string()).await;
=======
        let mut update_server = UpdaterServer::start(UpdateServerState {
            url: "https://httpbin.org/ip".to_string(),
            periodicity: Duration::from_millis(1000),
        });

        let result = UpdaterServer::check(&mut update_server).await;
>>>>>>> async_handlers
        tracing::info!("Update check done: {result:?}");
        assert_eq!(result, UpdaterOutMessage::Ok);

        // giving it some time before ending
        thread::sleep(Duration::from_secs(10));
    })
}
