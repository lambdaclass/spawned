//! Application adapter: reserve payload bytes BEFORE allocation, across two actors.
use spawned_concurrency::{
    message::Message,
    tasks::{Actor, ActorRef, ActorStart, Context, Handler},
};
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

const PAYLOAD: usize = 1024;
const BUDGET: usize = 4 * PAYLOAD;
struct Packet {
    bytes: Vec<u8>,
    credit: OwnedSemaphorePermit,
}
impl Message for Packet {
    type Result = usize;
}
struct Sink(usize);
impl Actor for Sink {}
impl Handler<Packet> for Sink {
    async fn handle(
        &mut self,
        packet: Packet,
        reply: spawned_concurrency::reply::PendingReply<'_, usize>,
        _: &Context<Self>,
    ) {
        let __reply_value: usize = {
            let Packet { bytes, credit } = packet;
            let size = bytes.len();
            self.0 += size;
            drop(bytes);
            drop(credit); // Capacity returns after the payload is released.
            size
        };
        let _ = reply.reply(__reply_value);
    }
}
struct Total;
impl Message for Total {
    type Result = usize;
}
impl Handler<Total> for Sink {
    async fn handle(
        &mut self,
        _: Total,
        reply: spawned_concurrency::reply::PendingReply<'_, usize>,
        _: &Context<Self>,
    ) {
        let __reply_value: usize = { self.0 };
        let _ = reply.reply(__reply_value);
    }
}
struct Forwarder(ActorRef<Sink>);
impl Actor for Forwarder {}
impl Handler<Packet> for Forwarder {
    async fn handle(
        &mut self,
        packet: Packet,
        reply: spawned_concurrency::reply::PendingReply<'_, usize>,
        _: &Context<Self>,
    ) {
        let __reply_value: usize = { self.0.request(packet).await.unwrap() };
        let _ = reply.reply(__reply_value);
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let budget = Arc::new(Semaphore::new(BUDGET));
    let sink = Sink(0).start();
    let forwarder = Forwarder(sink.clone()).start();
    let mut producers = Vec::new();
    for _ in 0..8 {
        let budget = budget.clone();
        let forwarder = forwarder.clone();
        producers.push(tokio::spawn(async move {
            for _ in 0..8 {
                let credit = budget
                    .clone()
                    .acquire_many_owned(PAYLOAD as u32)
                    .await
                    .unwrap();
                let packet = Packet {
                    bytes: vec![7; PAYLOAD],
                    credit,
                };
                assert_eq!(forwarder.request(packet).await.unwrap(), PAYLOAD);
            }
        }));
    }
    for producer in producers {
        producer.await.unwrap();
    }
    assert_eq!(sink.request(Total).await.unwrap(), 64 * PAYLOAD);
    assert_eq!(budget.available_permits(), BUDGET);
    println!(
        "64 payloads crossed two actors; payload budget = {BUDGET} bytes; all credits returned"
    );
    forwarder.child_handle().stop();
    forwarder.join().await;
    sink.child_handle().stop();
    sink.join().await;
}
