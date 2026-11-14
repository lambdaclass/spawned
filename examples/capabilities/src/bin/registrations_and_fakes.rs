//! Protocol fakes, managed name cleanup, and replacement-safe manual guards.
use spawned_concurrency::{
    actor, protocol, registry,
    supervision::{Backoff, ChildSpec, SupervisorConfig},
    system::ActorSystem,
    tasks::{Actor, Context, Handler},
    Response,
};
use std::{sync::Arc, time::Duration};
#[protocol]
trait PriceProtocol: Send + Sync {
    fn price(&self) -> Response<u64>;
}
struct FixedPrice;
impl PriceProtocol for FixedPrice {
    fn price(&self) -> Response<u64> {
        Response::ready(Ok(7))
    }
}
async fn invoice(client: &PriceRef, units: u64) -> u64 {
    client.price().await.unwrap() * units
}
struct Service;
#[actor(protocol = PriceProtocol)]
impl Service {
    #[started]
    async fn started(&mut self, ctx: &Context<Self>) {
        ctx.register_name("example-managed-price").unwrap();
    }
    #[request_handler]
    async fn price(&mut self, _: price_protocol::Price, _: &Context<Self>) -> u64 {
        7
    }
}
struct Crash;
impl spawned_concurrency::message::Message for Crash {
    type Result = ();
}
impl Handler<Crash> for Service {
    async fn handle(
        &mut self,
        _: Crash,
        _reply: spawned_concurrency::PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
        {
            panic!("intentional named-service replacement");
        }
    }
}
#[tokio::main(flavor = "current_thread")]
async fn main() {
    // Test business behavior with a protocol fake: no actor/runtime work needed.
    let fake: PriceRef = Arc::new(FixedPrice);
    assert_eq!(invoice(&fake, 3).await, 21);
    let system = ActorSystem::new().unwrap();
    let group = system.async_group("named-service", 1).unwrap();
    let mut tree = system
        .supervisor(
            SupervisorConfig::default().backoff(
                Backoff::default()
                    .initial(Duration::ZERO)
                    .maximum(Duration::ZERO)
                    .jitter(false),
            ),
        )
        .unwrap();
    let service = tree
        .add(ChildSpec::new("prices", &group, |_| async {
            Ok::<_, String>(Service)
        }))
        .unwrap();
    let supervisor = tree.start().await.unwrap();
    let old =
        registry::whereis::<spawned_concurrency::tasks::ActorRef<Service>>("example-managed-price")
            .unwrap();
    assert_eq!(invoice(&old.to_price_ref(), 3).await, 21);
    assert!(old.request(Crash).await.is_err());
    let new = tokio::time::timeout(Duration::from_secs(2), service.wait_for_generation(1))
        .await
        .unwrap()
        .unwrap();
    assert_ne!(old.id(), new.id());
    assert_eq!(
        registry::whereis::<spawned_concurrency::tasks::ActorRef<Service>>("example-managed-price")
            .unwrap()
            .id(),
        new.id()
    );
    assert!(supervisor.shutdown(Duration::from_secs(2)).await.complete);
    assert!(system.shutdown(Duration::from_secs(2)).await.complete);
    assert!(
        registry::whereis::<spawned_concurrency::tasks::ActorRef<Service>>("example-managed-price")
            .is_none()
    );

    // Manual registrations require explicit ownership/cleanup. An old guard
    // cannot remove a newer registration, even with the same actor identity.
    use spawned_concurrency::threads::ActorStart as _;
    struct Native;
    impl spawned_concurrency::threads::Actor for Native {}
    let native = Native.start();
    let old = registry::register_owned("example-manual", native.id(), 1u64).unwrap();
    registry::unregister("example-manual");
    let new = registry::register_owned("example-manual", native.id(), 2u64).unwrap();
    drop(old);
    assert_eq!(registry::whereis::<u64>("example-manual"), Some(2));
    native.context().stop();
    native.join();
    drop(new);
    assert!(registry::whereis::<u64>("example-manual").is_none());
    println!("Fake protocols isolate business tests; owned names follow cleanup and replacement.");
}
