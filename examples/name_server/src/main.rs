mod protocols;
mod server;

use protocols::{FindResult, NameServerProtocol};
use server::NameServer;
use spawned_concurrency::tasks::ActorStart as _;
use spawned_rt::tasks as rt;

fn main() {
    rt::run(async {
        let ns = NameServer::new().start();

        ns.add("Joe".into(), "At Home".into()).await.unwrap();

        let result = ns.find("Joe".into()).await.unwrap();
        tracing::info!("Retrieving value result: {result:?}");
        assert_eq!(
            result,
            FindResult::Found { value: "At Home".to_string() }
        );

        let result = ns.find("Bob".into()).await.unwrap();
        tracing::info!("Retrieving value result: {result:?}");
        assert_eq!(result, FindResult::NotFound);
    })
}
