//! Ping-pong example — Approach A (Recipient<M> for type erasure).
//!
//! Consumer holds Recipient<Pong>, Producer holds Recipient<Ping>.
//! No protocol traits needed.

mod consumer;
mod messages;
mod producer;

use consumer::Consumer;
use producer::{Producer, SetConsumer};
use spawned_concurrency::tasks::ActorStart as _;
use spawned_rt::tasks as rt;
use std::time::Duration;

fn main() {
    rt::run(async {
        let producer = Producer { consumer: None }.start();

        let consumer = Consumer {
            producer: producer.recipient(),
        }
        .start();

        producer
            .send(SetConsumer(consumer.recipient()))
            .unwrap();

        consumer.send(messages::Ping).unwrap();

        rt::sleep(Duration::from_millis(1)).await;
    })
}
