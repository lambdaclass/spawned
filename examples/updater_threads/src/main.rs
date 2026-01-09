//! Example to test a recurrent gen_server.
//!
//! Just activates periodically and performs an http request
//!

mod messages;
mod server;

use std::{thread, time::Duration};

use server::UpdaterServer;
use spawned_concurrency::threads::Actor as _;
use spawned_rt::threads as rt;

fn main() {
    rt::run(|| {
        UpdaterServer {
            url: "https://httpbin.org/ip".to_string(),
            periodicity: Duration::from_millis(1000),
        }
        .start();

        // giving it some time before ending
        thread::sleep(Duration::from_secs(10));
    })
}
