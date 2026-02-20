mod consumer;
mod messages;
mod producer;

use std::{thread, time::Duration};

use consumer::Consumer;
use producer::{Producer, SetConsumer};
use spawned_concurrency::threads::ActorStart as _;
use spawned_rt::threads as rt;

fn main() {
    rt::run(|| {
        let producer = Producer { consumer: None }.start();

        let consumer = Consumer {
            producer: producer.recipient(),
        }
        .start();

        producer
            .send(SetConsumer(consumer.recipient()))
            .unwrap();

        consumer.send(messages::Ping).unwrap();

        thread::sleep(Duration::from_millis(1));
    })
}
