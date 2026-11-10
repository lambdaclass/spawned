use spawned_concurrency::{
    message::Message,
    protocol,
    tasks::{Actor, ActorRef, ActorStart, Context, Handler},
    ActorError, Response,
};
use std::{
    mem::{size_of, size_of_val},
    time::Duration,
};
#[protocol]
pub trait ProbeProtocol: Send + Sync {
    fn notify(&self, value: u64) -> Result<(), ActorError>;
    fn query(&self, value: u64) -> Response<u64>;
}
struct Worker;
impl Actor for Worker {}
impl Handler<probe_protocol::Notify> for Worker {
    async fn handle(&mut self, _: probe_protocol::Notify, _: &Context<Self>) {}
}
impl Handler<probe_protocol::Query> for Worker {
    async fn handle(&mut self, q: probe_protocol::Query, _: &Context<Self>) -> u64 {
        q.value
    }
}
async fn owned_send(actor: ActorRef<Worker>) -> Result<(), ActorError> {
    actor.notify(1)
}
async fn owned_request(actor: ActorRef<Worker>) -> Result<u64, ActorError> {
    actor.request(probe_protocol::Query { value: 1 }).await
}
async fn owned_protocol(actor: ActorRef<Worker>) -> Result<u64, ActorError> {
    actor.query(1).await
}
async fn configured_protocol(actor: ActorRef<Worker>) -> Result<u64, ActorError> {
    actor
        .with_request_timeout(Duration::from_secs(1))
        .query(1)
        .await
}
#[derive(Clone)]
struct RpcContext {
    actors: [ActorRef<Worker>; 8],
}
async fn rpc_leaf(ctx: RpcContext) -> Result<u64, ActorError> {
    ctx.actors[0].clone().query(1).await
}
async fn rpc_middle(ctx: RpcContext) -> Result<u64, ActorError> {
    rpc_leaf(ctx.clone()).await
}
async fn rpc_dispatch(ctx: RpcContext) -> Result<u64, ActorError> {
    rpc_middle(ctx.clone()).await
}
fn main() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let actor = Worker.start();
        let context = actor.context();
        let recipient = actor.recipient::<probe_protocol::Query>();
        let rpc = RpcContext {
            actors: std::array::from_fn(|_| actor.clone()),
        };
        let sizes = [
            ("ActorRef", size_of_val(&actor)),
            ("Context", size_of_val(&context)),
            (
                "RequestOptions",
                size_of_val(&actor.with_request_timeout(Duration::from_secs(1))),
            ),
            ("send_result", size_of::<Result<(), ActorError>>()),
            (
                "request_borrowed",
                size_of_val(&actor.request(probe_protocol::Query { value: 1 })),
            ),
            (
                "request_timeout",
                size_of_val(&actor.request_with_timeout(
                    probe_protocol::Query { value: 1 },
                    Duration::from_secs(1),
                )),
            ),
            (
                "context_request",
                size_of_val(&context.request(probe_protocol::Query { value: 1 })),
            ),
            (
                "recipient_request",
                size_of_val(&spawned_concurrency::tasks::request(
                    &*recipient,
                    probe_protocol::Query { value: 1 },
                    Duration::from_secs(1),
                )),
            ),
            ("protocol_response", size_of_val(&actor.query(1))),
            ("owned_send", size_of_val(&owned_send(actor.clone()))),
            ("owned_request", size_of_val(&owned_request(actor.clone()))),
            (
                "owned_protocol",
                size_of_val(&owned_protocol(actor.clone())),
            ),
            (
                "configured_protocol",
                size_of_val(&configured_protocol(actor.clone())),
            ),
            ("rpc_context", size_of_val(&rpc)),
            ("rpc_dispatch", size_of_val(&rpc_dispatch(rpc))),
            ("response_type", size_of::<Response<u64>>()),
        ];
        for (name, size) in sizes {
            println!("{name},{size}");
        }
        actor.context().stop();
        actor.join().await;
    });
}
