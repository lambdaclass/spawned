//! Existing behavior: a timed-out caller does not roll back an actor's effect.
use spawned_concurrency::{
    message::Message,
    tasks::{Actor, ActorStart, Context, Handler},
    ActorError, Deadline, Response,
};
use std::time::Duration;
use tokio::sync::oneshot;

struct Counter(usize);
impl Actor for Counter {}
struct Apply {
    entered: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
}
impl Message for Apply {
    type Result = ();
}
impl Handler<Apply> for Counter {
    async fn handle(
        &mut self,
        msg: Apply,
        reply: spawned_concurrency::PendingReply<'_, ()>,
        _: &Context<Self>,
    ) {
        {
            self.0 += 1; // Effect happens before the reply.
            msg.entered.send(()).unwrap();
            let _ = msg.release.await;
        }
        reply.reply(());
    }
}
struct Read;
impl Message for Read {
    type Result = usize;
}
impl Handler<Read> for Counter {
    async fn handle(
        &mut self,
        _: Read,
        reply: spawned_concurrency::PendingReply<'_, usize>,
        _: &Context<Self>,
    ) {
        reply.reply(self.0);
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let counter = Counter(0).start();
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    // The deadline is fixed at send (OTP's call/3): already due here. An
    // expired request is still dispatched, as in OTP; its late reply is dropped.
    let reply = counter.send_request(
        Apply {
            entered: entered_tx,
            release: release_rx,
        },
        Deadline::after(Duration::ZERO),
    );
    entered_rx.await.unwrap();
    // A gate keeps the reply pending; no race-dependent sleep is needed.
    let result = Response::from(reply).await;
    assert!(matches!(result, Err(ActorError::RequestTimeout)));
    release_tx.send(()).unwrap();
    assert_eq!(counter.request(Read).await.unwrap(), 1);
    println!("caller timed out, but the effect happened once; retry needs an application idempotency key");
    counter.child_handle().stop();
    counter.join().await;
}
