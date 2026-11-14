//! Application recipe: finish known producers, then fence the accepted work.
use spawned_concurrency::{
    message::Message,
    tasks::{Actor, ActorStart, Context, Handler},
};
struct Worker(usize);
impl Actor for Worker {}
struct Add;
impl Message for Add {
    type Result = ();
}
impl Handler<Add> for Worker {
    async fn handle(
        &mut self,
        _: Add,
        reply: spawned_concurrency::PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
        self.0 += 1;
        reply.reply(());
    }
}
struct Fence;
impl Message for Fence {
    type Result = usize;
}
impl Handler<Fence> for Worker {
    async fn handle(
        &mut self,
        _: Fence,
        reply: spawned_concurrency::PendingReply<'_, usize>,
        _: &Context<Self>,
    ) {
        reply.reply(self.0);
    }
}
#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let worker = Worker(0).start();
    let mut producers = Vec::new();
    for _ in 0..4 {
        let actor = worker.clone();
        producers.push(tokio::spawn(async move {
            for _ in 0..32 {
                actor.send(Add).unwrap();
            }
        }));
    }
    // These finite, owned producers are the only sources of work. Joining them
    // closes application admission; an open service needs its own admission gate.
    for producer in producers {
        producer.await.unwrap();
    }
    let completed = worker.request(Fence).await.unwrap();
    assert_eq!(completed, 128);
    worker.child_handle().stop();
    worker.join().await;
    println!("all {completed} accepted operations completed before stop");
    // ctx.stop()/ChildHandle::stop alone do not drain a queued backlog.
    // A fence does not wait for detached helpers or external durable commits.
}
