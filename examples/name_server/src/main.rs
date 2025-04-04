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

use spawned_concurrency::GenServer as _;
use spawned_rt as rt;
use server::NameServer;


fn main() {
    rt::run(async {
        let mut name_server = NameServer::start().await;
        
        tracing::info!("Storing value");

        let result = NameServer::add(&mut name_server, "Joe".to_string(), "At Home".to_string()).await;

        tracing::info!("Storing value result {result:?}");

        let result = NameServer::find(&mut name_server, "Joe".to_string()).await;

        tracing::info!("Retrieving value result {result:?}");

        let result = NameServer::find(&mut name_server, "Bob".to_string()).await;

        tracing::info!("Retrieving value result {result:?}");
    })
}
