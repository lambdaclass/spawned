//! Simple example to test concurrency/Process abstraction.
//!
//! Based on Joe's Armstrong book: Programming Erlang, Second edition
//! Section 22.1 - The Road to the Generic Server
//!
//! Erlang usage example:
//! 1> server1:start(name_server, name_server).
//! true
//! 2> name_server:add(joe, "at home").
//! ok
//! 3> name_server:find(joe).
//! {ok,"at home"}

mod messages;
mod server;

use messages::NameServerOutMessage;
use server::NameServer;
use spawned_concurrency::{Backend, Actor as _};
use spawned_rt::tasks as rt;

fn main() {
    rt::run(async {
        let mut name_server = NameServer::new().start(Backend::Async);

        let result =
            NameServer::add(&mut name_server, "Joe".to_string(), "At Home".to_string()).await;
        tracing::info!("Storing value result: {result:?}");
        assert_eq!(result, NameServerOutMessage::Ok);

        let result = NameServer::find(&mut name_server, "Joe".to_string()).await;
        tracing::info!("Retrieving value result: {result:?}");
        assert_eq!(
            result,
            NameServerOutMessage::Found {
                value: "At Home".to_string()
            }
        );

        let result = NameServer::find(&mut name_server, "Bob".to_string()).await;
        tracing::info!("Retrieving value result: {result:?}");
        assert_eq!(result, NameServerOutMessage::NotFound);
    })
}
