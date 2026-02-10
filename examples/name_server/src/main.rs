//! Name server example using the new Handler<M> API.
//!
//! Based on Joe's Armstrong book: Programming Erlang, Second edition
//! Section 22.1 - The Road to the Generic Server

mod messages;
mod server;

use messages::*;
use server::NameServer;
use spawned_concurrency::tasks::ActorStart as _;
use spawned_rt::tasks as rt;

fn main() {
    rt::run(async {
        let ns = NameServer::new().start();

        ns.send_request(Add { key: "Joe".into(), value: "At Home".into() }).await.unwrap();

        let result = ns.send_request(Find { key: "Joe".into() }).await.unwrap();
        tracing::info!("Retrieving value result: {result:?}");
        assert_eq!(
            result,
            FindResult::Found { value: "At Home".to_string() }
        );

        let result = ns.send_request(Find { key: "Bob".into() }).await.unwrap();
        tracing::info!("Retrieving value result: {result:?}");
        assert_eq!(result, FindResult::NotFound);
    })
}
