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
        let producer = Producer { consumer: None }.start();

        let consumer = Consumer {
            producer: Arc::new(producer.clone()),
        }
        .start();

        producer
            .send(SetConsumer(Arc::new(consumer.clone())))
            .unwrap();

        consumer.send(messages::Ping).unwrap();

        thread::sleep(Duration::from_millis(1));
    })
}
