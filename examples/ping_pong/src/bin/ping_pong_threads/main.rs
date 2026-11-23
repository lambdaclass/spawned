mod consumer;
mod producer;
mod protocols;

use std::{thread, time::Duration};

use consumer::Consumer;
use producer::{Producer, SetConsumer};
use protocols::{PingReceiver, ToPingReceiverRef, ToPongReceiverRef};
use spawned_concurrency::threads::ActorStart as _;
use spawned_rt::threads as rt;

fn main() {
    rt::run(|| {
        let producer = Producer { consumer: None }.start();

        let consumer = Consumer {
            producer: producer.to_pong_receiver_ref(),
        }
        .start();

        producer
            .send(SetConsumer(consumer.to_ping_receiver_ref()))
            .unwrap();

        consumer.ping().unwrap();

        thread::sleep(Duration::from_millis(1));
    })
}
