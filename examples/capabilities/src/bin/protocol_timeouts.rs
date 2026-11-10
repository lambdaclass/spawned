//! Protocol/method defaults and a cloned-handle override in both modes.
use spawned_concurrency::{protocol, Response};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
#[protocol(timeout = Duration::from_millis(10))]
trait LookupProtocol: Send + Sync {
    fn value(&self) -> Response<u64>;
    #[request_timeout(Duration::ZERO)]
    fn quick(&self) -> Response<u64>;
}
mod asynchronous {
    use super::*;
    use spawned_concurrency::tasks::{Actor, ActorStart, Context, Handler};
    struct Worker(tokio::sync::oneshot::Receiver<()>, Arc<AtomicUsize>);
    impl Actor for Worker {
        async fn started(&mut self, _: &Context<Self>) {
            let _ = (&mut self.0).await;
        }
    }
    impl Handler<lookup_protocol::Value> for Worker {
        async fn handle(
            &mut self,
            _: lookup_protocol::Value,
            reply: spawned_concurrency::reply::PendingReply<'_, u64>,
            _: &Context<Self>,
        ) {
            let __reply_value: u64 = {
                self.1.fetch_add(1, Ordering::SeqCst);
                42
            };
            let _ = reply.reply(__reply_value);
        }
    }
    impl Handler<lookup_protocol::Quick> for Worker {
        async fn handle(
            &mut self,
            _: lookup_protocol::Quick,
            reply: spawned_concurrency::reply::PendingReply<'_, u64>,
            _: &Context<Self>,
        ) {
            let __reply_value: u64 = {
                self.1.fetch_add(1, Ordering::SeqCst);
                42
            };
            let _ = reply.reply(__reply_value);
        }
    }
    pub async fn run() {
        let (release, gate) = tokio::sync::oneshot::channel();
        let calls = Arc::new(AtomicUsize::new(0));
        let actor = Worker(gate, calls.clone()).start();
        // Startup is held, so neither timed-out request can complete early.
        assert!(actor.value().await.is_err());
        assert!(actor.quick().await.is_err());
        let configured = actor
            .with_request_timeout(Duration::from_secs(1))
            .to_lookup_ref();
        let response = configured.quick();
        release.send(()).unwrap();
        assert_eq!(response.await.unwrap(), 42);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        // Earlier requests remain ordinary queued work, despite caller timeout.
        actor.context().stop();
        actor.join().await;
    }
}
mod synchronous {
    use super::*;
    use spawned_concurrency::threads::{Actor, ActorStart, Context, Handler};
    struct Worker(std::sync::mpsc::Receiver<()>, Arc<AtomicUsize>);
    impl Actor for Worker {
        fn started(&mut self, _: &Context<Self>) {
            self.0.recv_timeout(Duration::from_secs(2)).unwrap();
        }
    }
    impl Handler<lookup_protocol::Value> for Worker {
        fn handle(
            &mut self,
            _: lookup_protocol::Value,
            reply: spawned_concurrency::reply::PendingReply<'_, u64>,
            _: &Context<Self>,
        ) {
            let __reply_value: u64 = {
                self.1.fetch_add(1, Ordering::SeqCst);
                42
            };
            let _ = reply.reply(__reply_value);
        }
    }
    impl Handler<lookup_protocol::Quick> for Worker {
        fn handle(
            &mut self,
            _: lookup_protocol::Quick,
            reply: spawned_concurrency::reply::PendingReply<'_, u64>,
            _: &Context<Self>,
        ) {
            let __reply_value: u64 = {
                self.1.fetch_add(1, Ordering::SeqCst);
                42
            };
            let _ = reply.reply(__reply_value);
        }
    }
    pub fn run() {
        let (release, gate) = std::sync::mpsc::channel();
        let calls = Arc::new(AtomicUsize::new(0));
        let actor = Worker(gate, calls.clone()).start();
        assert!(actor.value().is_err());
        assert!(actor.quick().is_err());
        release.send(()).unwrap();
        let configured = actor
            .with_request_timeout(Duration::from_secs(1))
            .to_lookup_ref();
        assert_eq!(configured.quick().unwrap(), 42);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        actor.context().stop();
        actor.join();
    }
}
#[tokio::main(flavor = "current_thread")]
async fn main() {
    asynchronous::run().await;
    synchronous::run();
    println!("Timeout precedence: handle override > method > protocol > five-second default.");
}
