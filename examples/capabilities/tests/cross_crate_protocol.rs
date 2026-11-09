//! Protocol definitions come from another crate, so fake coherence is checked
//! across a real Cargo dependency boundary rather than inside one macro module.
use spawned_concurrency::Response;
use spawned_protocol_contract::SharedProtocol;
use std::{fmt::Debug, sync::Arc, time::Duration};

#[derive(Debug)]
struct Fake;
impl SharedProtocol for Fake {
    fn value(&self) -> Response<u64> {
        Response::ready(Ok(7))
    }
}
#[derive(Debug)]
struct GenericFake<T>(T);
impl<T: Debug + Send + Sync> SharedProtocol for GenericFake<T> {
    fn value(&self) -> Response<u64> {
        Response::ready(Ok(8))
    }
}
mod native {
    use spawned_concurrency::{
        actor,
        threads::{Actor, Context, Handler},
    };
    use spawned_protocol_contract::{
        shared_protocol, where_protocol, SharedProtocol, WhereProtocol,
    };
    pub struct Worker;
    #[actor(protocol(SharedProtocol, WhereProtocol))]
    impl Worker {
        #[request_handler]
        fn value(&mut self, _: shared_protocol::Value, _: &Context<Self>) -> u64 {
            42
        }
        #[request_handler]
        fn version(&mut self, _: where_protocol::Version, _: &Context<Self>) -> u64 {
            1
        }
    }
}
mod asynchronous {
    use spawned_concurrency::{
        actor,
        tasks::{Actor, Context, Handler},
    };
    use spawned_protocol_contract::{
        shared_protocol, where_protocol, SharedProtocol, WhereProtocol,
    };
    pub struct Worker;
    #[actor(protocol(SharedProtocol, WhereProtocol))]
    impl Worker {
        #[request_handler]
        async fn value(&mut self, _: shared_protocol::Value, _: &Context<Self>) -> u64 {
            42
        }
        #[request_handler]
        async fn version(&mut self, _: where_protocol::Version, _: &Context<Self>) -> u64 {
            1
        }
    }
}
#[tokio::test]
async fn exported_protocols_support_actors_configured_refs_and_downstream_fakes() {
    use spawned_concurrency::{tasks::ActorStart as _, threads::ActorStart as _};
    use spawned_protocol_contract::{ToSharedRef, ToWhereRef};
    let fake: Arc<dyn SharedProtocol> = Arc::new(Fake);
    assert_eq!(fake.value().unwrap(), 7);
    assert_eq!(GenericFake("state").value().unwrap(), 8);
    let native = native::Worker.start();
    assert_eq!(native.value().unwrap(), 42);
    let configured = native.with_request_timeout(Duration::from_secs(5));
    assert_eq!(configured.to_shared_ref().value().unwrap(), 42);
    assert_eq!(configured.to_where_ref().version().unwrap(), 1);
    native.context().stop();
    native.join();
    let actor = asynchronous::Worker.start();
    assert_eq!(actor.value().await.unwrap(), 42);
    let configured = actor.with_request_timeout(Duration::from_secs(5));
    assert_eq!(configured.to_shared_ref().value().await.unwrap(), 42);
    assert_eq!(configured.to_where_ref().version().await.unwrap(), 1);
    actor.context().stop();
    actor.join().await;
}
