//! Real synchronous-only dependency graph, including protocols and lifecycle.
use spawned_concurrency::{
    actor, protocol,
    threads::{Actor, ActorStart, Context, Handler},
    Down, ExitReason, Response,
};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
#[protocol]
pub trait CounterProtocol: Send + Sync {
    #[request_timeout(Duration::from_millis(500))]
    fn next(&self) -> Response<usize>;
}
struct Counter(usize);
#[actor(protocol = CounterProtocol)]
impl Counter {
    #[request_handler]
    fn next_value(&mut self, _: counter_protocol::Next, _: &Context<Self>) -> usize {
        self.0 += 1;
        self.0
    }
}
struct Tick;
impl spawned_concurrency::message::Message for Tick {
    type Result = ();
}
struct Observer(Arc<AtomicUsize>);
impl Actor for Observer {}
impl Handler<Down> for Observer {
    fn handle(&mut self, _: Down, reply: spawned_concurrency::reply::PendingReply<'_, ()>, _: &Context<Self>) {
        
        self.0.fetch_add(1, Ordering::SeqCst);
reply.reply(());
    }
}
impl Handler<Tick> for Observer {
    fn handle(&mut self, _: Tick, reply: spawned_concurrency::reply::PendingReply<'_, ()>, _: &Context<Self>) {
        
        self.0.fetch_add(1, Ordering::SeqCst);
reply.reply(());
    }
}
fn main() {
    let counter = Counter(0).start();
    assert_eq!(counter.next().unwrap(), 1);
    assert_eq!(
        counter
            .with_request_timeout(Duration::from_secs(1))
            .to_counter_ref()
            .next()
            .unwrap(),
        2
    );
    assert_eq!(spawned_rt::threads::block_on(counter.next()).unwrap(), 3);
    spawned_concurrency::registry::register("thread-only-counter", counter.clone()).unwrap();
    let named = spawned_concurrency::registry::whereis::<
        spawned_concurrency::threads::ActorRef<Counter>,
    >("thread-only-counter")
    .unwrap();
    assert_eq!(named.next().unwrap(), 4);
    let events = Arc::new(AtomicUsize::new(0));
    let observer = Observer(events.clone()).start();
    observer.context().monitor(&counter.child_handle());
    spawned_concurrency::threads::send_after(Duration::from_millis(5), observer.context(), Tick);
    counter.child_handle().shutdown();
    assert_eq!(counter.wait_exit(), ExitReason::Shutdown);
    assert_eq!(
        spawned_rt::threads::block_on(counter.child_handle().wait_exit_async()),
        ExitReason::Shutdown
    );
    let limit = std::time::Instant::now() + Duration::from_secs(3);
    while events.load(Ordering::SeqCst) < 2 {
        assert!(
            std::time::Instant::now() < limit,
            "monitor/timer did not complete"
        );
        std::thread::yield_now();
    }
    observer.context().stop();
    assert_eq!(observer.wait_exit(), ExitReason::Normal);
    spawned_concurrency::registry::unregister("thread-only-counter");
    // Links retain the same exit semantics without an async runtime.
    let first = Counter(0).start();
    let second = Counter(0).start();
    first.context().link(&second.child_handle());
    second.child_handle().shutdown();
    assert_eq!(first.wait_exit(), ExitReason::Shutdown);
    assert_eq!(second.wait_exit(), ExitReason::Shutdown);
    #[cfg(feature = "metrics")]
    {
        let metrics = spawned_concurrency::metrics::snapshot();
        assert_eq!(metrics.actors_active, 0);
        assert_eq!(metrics.mailbox_depth, 0);
        assert_eq!(
            metrics.messages_enqueued_total,
            metrics.messages_handled_total + metrics.messages_discarded_total
        );
    }
    println!(
        "Tokio-free protocols, requests, completion, timers, monitors, links and metrics passed."
    );
}
