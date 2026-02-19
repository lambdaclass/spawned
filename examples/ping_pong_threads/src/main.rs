mod consumer;
mod messages;
mod producer;
mod protocols;

use std::{thread, time::Duration};

use consumer::Consumer;
use producer::{Producer, SetConsumer};
use spawned_concurrency::threads::ActorStart as _;
use spawned_rt::threads as rt;
use std::sync::Arc;

fn main() {
    rt::run(|| {
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
        thread::sleep(Duration::from_millis(1));
    })
}
