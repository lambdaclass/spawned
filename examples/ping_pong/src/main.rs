//! Ping-pong example demonstrating bidirectional communication
//! between actors using protocol traits for type-erased messaging.
//!
//! This solves the circular dependency problem: Consumer and Producer
//! don't need to know each other's concrete types — they only know
//! about the protocol traits they implement (PingReceiver and PongReceiver).

mod consumer;
mod messages;
mod producer;
mod protocols;

use consumer::Consumer;
use producer::{Producer, SetConsumer};
use spawned_concurrency::tasks::ActorStart as _;
use spawned_rt::tasks as rt;
use std::sync::Arc;
use std::time::Duration;

fn main() {
    rt::run(async {
        // Start the producer first
        let producer = Producer { consumer: None }.start();

        // Start the consumer with an Arc<dyn PongReceiver> pointing to the producer
        let consumer = Consumer {
            producer: Arc::new(producer.clone()),
        }
        .start();

        // Wire up the producer with the consumer's Arc<dyn PingReceiver>
        producer
            .send(SetConsumer(Arc::new(consumer.clone())))
            .unwrap();

        // Kick off the ping-pong loop
        consumer.send(messages::Ping).unwrap();

        // Let them ping-pong for a bit
        rt::sleep(Duration::from_millis(1)).await;
    })
}
