//! Type erasure, listener cancellation, and a blocking iterator's limit.
use spawned_concurrency::{message::Message, ActorError};
use std::time::Duration;
#[derive(Clone)]
struct Item(u64);
impl Message for Item {
    type Result = ();
}
struct Total;
impl Message for Total {
    type Result = u64;
}
// Distinct Counter<1> and Counter<2> types share only their message interface.
struct Counter<const SCALE: u64>(u64);
mod asynchronous {
    use super::*;
    use futures::{stream, StreamExt};
    use spawned_concurrency::tasks::{self, Actor, ActorStart, Context, Handler, Recipient};
    impl<const N: u64> Actor for Counter<N> {}
    impl<const N: u64> Handler<Item> for Counter<N> {
        async fn handle(&mut self, msg: Item, _: &Context<Self>) {
            self.0 += N * msg.0;
        }
    }
    impl<const N: u64> Handler<Total> for Counter<N> {
        async fn handle(&mut self, _: Total, _: &Context<Self>) -> u64 {
            self.0
        }
    }
    pub async fn run() {
        let first = Counter::<1>(0).start();
        let second = Counter::<2>(0).start();
        let recipients: Vec<Recipient<Item>> = vec![first.recipient(), second.recipient()];
        for member in &recipients {
            member.send(Item(1)).unwrap();
        }
        assert_eq!(second.request(Total).await.unwrap(), 2);
        let source = stream::iter((0..5).map(|_| Item(1))).chain(stream::pending());
        let mut listener = tasks::spawn_listener(first.context(), source);
        tokio::time::timeout(Duration::from_secs(2), async {
            while first.request(Total).await.unwrap() != 6 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(tokio::time::timeout(Duration::ZERO, &mut listener)
            .await
            .is_err());
        first.context().stop();
        first.join().await;
        tokio::time::timeout(Duration::from_secs(2), listener)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            recipients[0].send(Item(1)),
            Err(ActorError::ActorStopped)
        ));
        recipients[1].send(Item(1)).unwrap();
        assert_eq!(second.request(Total).await.unwrap(), 4);
        second.context().stop();
        second.join().await;
    }
}
mod synchronous {
    use super::*;
    use spawned_concurrency::threads::{self, Actor, ActorStart, Context, Handler, Recipient};
    impl<const N: u64> Actor for Counter<N> {}
    impl<const N: u64> Handler<Item> for Counter<N> {
        fn handle(&mut self, msg: Item, _: &Context<Self>) {
            self.0 += N * msg.0;
        }
    }
    impl<const N: u64> Handler<Total> for Counter<N> {
        fn handle(&mut self, _: Total, _: &Context<Self>) -> u64 {
            self.0
        }
    }
    struct Source {
        left: usize,
        entered: std::sync::mpsc::Sender<()>,
        release: std::sync::mpsc::Receiver<()>,
    }
    impl Iterator for Source {
        type Item = Item;
        fn next(&mut self) -> Option<Item> {
            if self.left > 0 {
                self.left -= 1;
                return Some(Item(1));
            }
            self.entered.send(()).unwrap();
            self.release.recv_timeout(Duration::from_secs(2)).unwrap();
            None
        }
    }
    pub fn run() {
        let first = Counter::<1>(0).start();
        let second = Counter::<2>(0).start();
        let recipients: Vec<Recipient<Item>> = vec![first.recipient(), second.recipient()];
        for member in &recipients {
            member.send(Item(1)).unwrap();
        }
        let (entered, waiting) = std::sync::mpsc::channel();
        let (release, gate) = std::sync::mpsc::channel();
        let listener = threads::spawn_listener(
            first.context(),
            Source {
                left: 5,
                entered,
                release: gate,
            },
        );
        waiting.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(first.request(Total).unwrap(), 6);
        first.context().stop();
        first.join();
        // Cancellation cannot interrupt a next() that already blocks.
        assert!(!listener.is_finished());
        release.send(()).unwrap();
        listener.join().unwrap();
        assert!(matches!(
            recipients[0].send(Item(1)),
            Err(ActorError::ActorStopped)
        ));
        recipients[1].send(Item(1)).unwrap();
        assert_eq!(second.request(Total).unwrap(), 4);
        second.context().stop();
        second.join();
    }
}
#[tokio::main(flavor = "current_thread")]
async fn main() {
    asynchronous::run().await;
    synchronous::run();
    println!(
        "Recipients hide actor types; listeners stop cooperatively. Mailboxes remain unbounded."
    );
}
