mod consumer;
mod producer;
mod protocols;

use consumer::Consumer;
use producer::{Producer, SetConsumer};
use protocols::{PingReceiver, ToPingReceiverRef, ToPongReceiverRef};
use spawned_concurrency::tasks::ActorStart as _;
use spawned_rt::tasks as rt;
use std::time::Duration;

fn main() {
    rt::run(async {
        let producer = Producer { consumer: None }.start();

        let consumer = Consumer {
            producer: producer.to_pong_receiver_ref(),
        }
        .start();

        producer
            .send(SetConsumer(consumer.to_ping_receiver_ref()))
            .unwrap();

        consumer.ping().unwrap();

        rt::sleep(Duration::from_millis(1)).await;
    })
}
