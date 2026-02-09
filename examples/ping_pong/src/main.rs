//! Ping-pong example demonstrating bidirectional communication
//! between actors using Recipient<M> for type-erased messaging.
//!
//! This solves the circular dependency problem: Consumer and Producer
//! don't need to know each other's concrete types — they only know
//! about the message types they exchange (Ping and Pong).

mod consumer;
mod messages;
mod producer;

use consumer::Consumer;
use messages::Ping;
use producer::Producer;
use spawned_concurrency::tasks::ActorStart as _;
use spawned_rt::tasks as rt;
use std::time::Duration;

fn main() {
    rt::run(async {
        // Start the producer first
        let producer = Producer { consumer: None }.start();

        // Start the consumer with a Recipient<Pong> pointing to the producer
        let consumer = Consumer {
            producer: producer.recipient(),
        }
        .start();

        // Wire up the producer with the consumer's Recipient<Ping>
        producer.send(messages::SetConsumer(consumer.recipient())).unwrap();

        // Kick off the ping-pong loop
        consumer.send(Ping).unwrap();

        // Let them ping-pong for a bit
        rt::sleep(Duration::from_millis(1)).await;
    })
}
