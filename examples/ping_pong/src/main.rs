mod consumer;
mod producer;
mod protocols;

use consumer::Consumer;
use producer::{Producer, SetConsumer};
use protocols::{AsPingReceiver, AsPongReceiver, PingReceiver};
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

        consumer.ping().unwrap();

        rt::sleep(Duration::from_millis(1)).await;
    })
}
