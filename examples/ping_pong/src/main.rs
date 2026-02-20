//! Ping-pong example — Approach B (protocol traits + protocol_impl!).
//!
//! Consumer and Producer don't know each other's concrete types.
//! They only depend on the PingReceiver and PongReceiver protocol traits.

mod consumer;
mod messages;
mod producer;
mod protocols;

use consumer::Consumer;
use producer::{Producer, SetConsumer};
use protocols::{AsPingReceiver, AsPongReceiver};
use spawned_concurrency::tasks::ActorStart as _;
use spawned_rt::tasks as rt;
use std::time::Duration;

fn main() {
    rt::run(async {
        let producer = Producer { consumer: None }.start();

        let consumer = Consumer {
            producer: producer.as_pong_receiver(),
        }
        .start();

        producer
            .send(SetConsumer(consumer.as_ping_receiver()))
            .unwrap();

        consumer.send(messages::Ping).unwrap();

        rt::sleep(Duration::from_millis(1)).await;
    })
}
