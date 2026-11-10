//! Stack-shape budgets for handles and request futures. Owned handles are
//! repeated in application dispatch futures (a borrowed request alone does not
//! detect that regression), and debug builds give every move of such a future
//! its own stack slot, so each handle word is paid many times over in callers'
//! frames. `ActorRef` and `Context` are one pointer: spawned 0.5 handles were
//! 4 words and the 0.7.1 ones 10-11, which cost ethrex's debug RPC dispatch
//! ~136 KiB of extra thread stack (`RpcApiContext` holds three handles).
#![cfg(all(feature = "async", target_pointer_width = "64"))]
use spawned_concurrency::{
    protocol,
    tasks::{Actor, ActorRef, ActorStart, Context, Handler},
    ActorError, Response,
};
use std::{
    mem::{size_of, size_of_val},
    time::Duration,
};
#[protocol]
trait ProbeProtocol: Send + Sync {
    fn notify(&self, value: u64) -> Result<(), ActorError>;
    fn query(&self, value: u64) -> Response<u64>;
}
struct Worker;
impl Actor for Worker {}
impl Handler<probe_protocol::Notify> for Worker {
    async fn handle(&mut self, _: probe_protocol::Notify, _: &Context<Self>) {}
}
impl Handler<probe_protocol::Query> for Worker {
    async fn handle(&mut self, query: probe_protocol::Query, _: &Context<Self>) -> u64 {
        query.value
    }
}
const BASELINE_WORDS: usize = 1;
#[derive(Clone)]
struct Baseline([usize; BASELINE_WORDS]);
// Match the nontrivial drop semantics of ActorRef in the generic future shapes.
impl Drop for Baseline {
    fn drop(&mut self) {}
}
impl ProbeProtocol for Baseline {
    fn notify(&self, _: u64) -> Result<(), ActorError> {
        Ok(())
    }
    fn query(&self, value: u64) -> Response<u64> {
        Response::ready(Ok(value + self.0[0] as u64))
    }
}
async fn send<H: ProbeProtocol>(actor: H) -> Result<(), ActorError> {
    actor.notify(1)
}
async fn protocol_call<H: ProbeProtocol>(actor: H) -> Result<u64, ActorError> {
    actor.query(1).await
}
#[derive(Clone)]
struct RpcContext<H: Clone> {
    actors: [H; 8],
}
async fn rpc_leaf<H: ProbeProtocol + Clone>(ctx: RpcContext<H>) -> Result<u64, ActorError> {
    ctx.actors[0].clone().query(1).await
}
async fn rpc_middle<H: ProbeProtocol + Clone>(ctx: RpcContext<H>) -> Result<u64, ActorError> {
    rpc_leaf(ctx.clone()).await
}
async fn rpc_dispatch<H: ProbeProtocol + Clone>(ctx: RpcContext<H>) -> Result<u64, ActorError> {
    rpc_middle(ctx.clone()).await
}
async fn owned_request(actor: ActorRef<Worker>) -> Result<u64, ActorError> {
    actor.request(probe_protocol::Query { value: 1 }).await
}
async fn configured_protocol(actor: ActorRef<Worker>) -> Result<u64, ActorError> {
    actor
        .with_request_timeout(Duration::from_secs(1))
        .query(1)
        .await
}
#[tokio::test]
async fn owned_send_request_and_protocol_futures_fit_the_pre_guard_layout() {
    let actor = Worker.start();
    let baseline = Baseline([0; BASELINE_WORDS]);
    assert_eq!(
        size_of_val(&actor),
        size_of::<usize>(),
        "ActorRef must stay one pointer"
    );
    assert_eq!(size_of::<Context<Worker>>(), size_of::<usize>());
    assert_eq!(size_of::<Option<ActorRef<Worker>>>(), size_of::<usize>());
    assert!(size_of_val(&send(actor.clone())) <= size_of_val(&send(baseline.clone())));
    assert!(
        size_of_val(&protocol_call(actor.clone())) <= size_of_val(&protocol_call(baseline.clone()))
    );
    let real = RpcContext {
        actors: std::array::from_fn(|_| actor.clone()),
    };
    let reference = RpcContext {
        actors: std::array::from_fn(|_| baseline.clone()),
    };
    assert!(size_of_val(&rpc_dispatch(real)) <= size_of_val(&rpc_dispatch(reference)));
    println!(
        "owned_request_bytes={} configured_protocol_bytes={}",
        size_of_val(&owned_request(actor.clone())),
        size_of_val(&configured_protocol(actor.clone()))
    );
    // Concrete timer-containing budgets measured on Linux x86_64, Rust 1.93.
    // Other 64-bit targets still check the relative owned-handle shapes above.
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        let metrics = cfg!(feature = "metrics");
        assert!(
            size_of_val(&actor.request(probe_protocol::Query { value: 1 }))
                <= if metrics { 224 } else { 200 }
        );
        // 0.7.1 (10-11 word handles): 368/408 and 288/336.
        assert!(size_of_val(&owned_request(actor.clone())) <= if metrics { 248 } else { 224 });
        assert!(size_of_val(&configured_protocol(actor.clone())) <= if metrics { 96 } else { 72 });
    }
    actor.context().stop();
    actor.join().await;
}
