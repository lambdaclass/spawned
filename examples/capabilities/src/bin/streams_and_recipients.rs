//! Type erasure, listener cancellation, and a blocking iterator's limit.
use spawned_concurrency::{message::Message, ActorError};
use std::time::Duration;
const LIMIT: Duration = Duration::from_secs(30);
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
        async fn handle(
            &mut self,
            msg: Item,
            reply: spawned_concurrency::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            self.0 += N * msg.0;
            reply.reply(());
        }
    }
    impl<const N: u64> Handler<Total> for Counter<N> {
        async fn handle(
            &mut self,
            _: Total,
            reply: spawned_concurrency::PendingReply<'_, u64>,
            _: &Context<Self>,
        ) {
            reply.reply(self.0);
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
        let (queued, progress) = tokio::sync::oneshot::channel();
        let source = stream::iter((0..5).map(|_| Item(1))).chain(stream::once(async move {
            queued.send(()).unwrap();
            std::future::pending::<Item>().await
        }));
        let listener = tasks::spawn_listener(first.context(), source);
        tokio::time::timeout(LIMIT, progress)
            .await
            .expect("async listener did not queue its finite input")
            .expect("async listener dropped its progress signal");
        assert_eq!(first.request(Total).await.unwrap(), 6);
        assert!(!listener.is_finished());
        first.context().stop();
        tokio::time::timeout(LIMIT, first.join())
            .await
            .expect("async counter did not stop");
        tokio::time::timeout(LIMIT, listener)
            .await
            .expect("async listener did not observe cancellation")
            .expect("async listener panicked");
        assert!(matches!(
            recipients[0].send(Item(1)),
            Err(ActorError::NoProc)
        ));
        recipients[1].send(Item(1)).unwrap();
        assert_eq!(second.request(Total).await.unwrap(), 4);
        second.context().stop();
        tokio::time::timeout(LIMIT, second.join())
            .await
            .expect("second async counter did not stop");
    }
}
mod synchronous {
    use super::*;
    use spawned_concurrency::threads::{self, Actor, ActorStart, Context, Handler, Recipient};
    impl<const N: u64> Actor for Counter<N> {}
    impl<const N: u64> Handler<Item> for Counter<N> {
        fn handle(
            &mut self,
            msg: Item,
            reply: spawned_concurrency::PendingReply<'_, ()>,
            _: &Context<Self>,
        ) {
            self.0 += N * msg.0;
            reply.reply(());
        }
    }
    impl<const N: u64> Handler<Total> for Counter<N> {
        fn handle(
            &mut self,
            _: Total,
            reply: spawned_concurrency::PendingReply<'_, u64>,
            _: &Context<Self>,
        ) {
            reply.reply(self.0);
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
            self.release
                .recv_timeout(LIMIT)
                .expect("native iterator release gate was not opened");
            None
        }
    }
    pub async fn run() {
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
        waiting
            .recv_timeout(LIMIT)
            .expect("native listener did not enter its blocking next()");
        assert_eq!(first.request(Total).unwrap(), 6);
        first.context().stop();
        // Cancellation cannot interrupt a next() that already blocks.
        // Release it before awaiting actor cleanup; cleanup does not race the gate deadline.
        assert!(!listener.is_finished());
        release.send(()).unwrap();
        tokio::time::timeout(LIMIT, async {
            while !listener.is_finished() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("native listener did not finish after its iterator was released");
        listener.join().expect("native listener panicked");
        tokio::time::timeout(LIMIT, first.child_handle().wait_exit_async())
            .await
            .expect("native counter did not stop");
        assert!(matches!(
            recipients[0].send(Item(1)),
            Err(ActorError::NoProc)
        ));
        recipients[1].send(Item(1)).unwrap();
        assert_eq!(second.request(Total).unwrap(), 4);
        second.context().stop();
        tokio::time::timeout(LIMIT, second.child_handle().wait_exit_async())
            .await
            .expect("second native counter did not stop");
    }
}
#[tokio::main(flavor = "current_thread")]
async fn main() {
    asynchronous::run().await;
    synchronous::run().await;
    println!(
        "Recipients hide actor types; listeners stop cooperatively. Mailboxes remain unbounded."
    );
}
