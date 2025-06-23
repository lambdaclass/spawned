//! Example to test a recurrent gen_server.
//!
//! Just activates periodically and performs an http request
//!

mod messages;
mod server;

use std::{thread, time::Duration};

use server::{UpdateServerState, UpdaterServer};
use spawned_concurrency::threads::GenServer as _;
use spawned_rt::threads as rt;

fn main() {
    rt::run(|| {
        UpdaterServer::start(UpdateServerState {
            url: "https://httpbin.org/ip".to_string(),
            periodicity: Duration::from_millis(1000),
        });

        // giving it some time before ending
        thread::sleep(Duration::from_secs(10));
    })
}
