mod consumer;
mod messages;
mod producer;
mod protocols;

use std::{thread, time::Duration};

use consumer::Consumer;
use producer::{Producer, SetConsumer};
use protocols::{AsPingReceiver, AsPongReceiver};
use spawned_concurrency::threads::ActorStart as _;
use spawned_rt::threads as rt;

fn main() {
    rt::run(|| {
        let producer = Producer { consumer: None }.start();

        let consumer = Consumer {
            producer: producer.as_pong_receiver(),
        }
        .start();

        producer
            .send(SetConsumer(consumer.as_ping_receiver()))
            .unwrap();

        consumer.send(messages::Ping).unwrap();

        thread::sleep(Duration::from_millis(1));
    })
}
