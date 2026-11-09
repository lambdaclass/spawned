use spawned_concurrency::{actor, protocol, Response};
use std::fmt::Debug;
use std::time::Duration;

#[protocol]
trait Probe: Send + Sync + Debug {
    fn value(&self) -> Response<u64>;
}
trait Endpoint: Send + Sync { fn kind(&self) -> &'static str; }
impl<A: spawned_concurrency::tasks::Actor> Endpoint for spawned_concurrency::tasks::ActorRef<A> {
    fn kind(&self) -> &'static str { "async" }
}
impl<A: spawned_concurrency::threads::Actor> Endpoint for spawned_concurrency::threads::ActorRef<A> {
    fn kind(&self) -> &'static str { "native" }
}
#[protocol]
trait Custom: Endpoint {
    fn other(&self) -> Response<u64>;
}
// A custom supertrait implemented only on ActorRef must not make the whole
// protocol fail just because a configured wrapper does not implement it.
mod native {
use super::*;
use spawned_concurrency::threads::{Actor, Context, Handler};
pub struct Native;
#[actor(protocol(Probe, Custom))]
impl Native {
    #[request_handler]
    fn value(&mut self, _: probe::Value, _: &spawned_concurrency::threads::Context<Self>) -> u64 { 42 }
    #[request_handler]
    fn other(&mut self, _: custom::Other, _: &spawned_concurrency::threads::Context<Self>) -> u64 { 7 }
}
}
mod asynchronous {
use super::*;
use spawned_concurrency::tasks::{Actor, Context, Handler};
pub struct Async;
#[actor(protocol(Probe, Custom))]
impl Async {
    #[request_handler]
    async fn value(&mut self, _: probe::Value, _: &spawned_concurrency::tasks::Context<Self>) -> u64 { 42 }
    #[request_handler]
    async fn other(&mut self, _: custom::Other, _: &spawned_concurrency::tasks::Context<Self>) -> u64 { 7 }
}
}
#[derive(Debug)]
struct Fake;
impl Probe for Fake { fn value(&self) -> Response<u64> { Response::ready(Ok(8)) } }
fn assert_debug<T: Debug>(_: &T) {}
#[tokio::main]
async fn main() {
    use spawned_concurrency::tasks::ActorStart as _;
    use spawned_concurrency::threads::ActorStart as _;
    let native = native::Native.start();
    let configured = native.with_request_timeout(Duration::from_secs(5));
    assert_debug(&configured);
    assert_eq!(configured.to_probe_ref().value().unwrap(), 42);
    assert_eq!(native.kind(), "native");
    assert_eq!(native.to_custom_ref().other().unwrap(), 7);
    native.context().stop(); native.join();
    let actor = asynchronous::Async.start();
    assert_eq!(actor.with_request_timeout(Duration::from_secs(5)).to_probe_ref().value().await.unwrap(),42);
    assert_eq!(actor.kind(), "async");
    assert_eq!(actor.to_custom_ref().other().await.unwrap(),7);
    actor.context().stop(); actor.join().await;
    assert_eq!(Fake.value().unwrap(),8);
}
