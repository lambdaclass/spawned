use crate::message::Message;
use crate::test_wait::wait_for;
use crate::threads::{spawn_listener, Actor, ActorStart, Context, Handler};
use spawned_rt::threads::{self as rt};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::time::Duration;

struct Collector {
    items: Vec<i32>,
    done: Arc<AtomicU64>,
}

struct Push {
    value: i32,
}
impl Message for Push {
    type Result = ();
}

struct GetItems;
impl Message for GetItems {
    type Result = Vec<i32>;
}

impl Actor for Collector {}

impl Handler<Push> for Collector {
    fn handle(
        &mut self,
        msg: Push,
        reply: crate::reply::PendingReply<'_, ()>,
        _ctx: &Context<Self>,
    ) {
        self.items.push(msg.value);
        self.done.fetch_add(1, Ordering::SeqCst);
        reply.reply(());
    }
}

impl Handler<GetItems> for Collector {
    fn handle(
        &mut self,
        _msg: GetItems,
        reply: crate::reply::PendingReply<'_, Vec<i32>>,
        _ctx: &Context<Self>,
    ) {
        reply.reply(self.items.clone());
    }
}

#[test]
fn listener_consumes_iterator() {
    crate::test_wait::bounded(
        concat!(module_path!(), "::listener_consumes_iterator"),
        move || {
            let done = Arc::new(AtomicU64::new(0));
            let actor = Collector {
                items: vec![],
                done: done.clone(),
            }
            .start();

            let ctx = actor.context();
            let items = vec![
                Push { value: 1 },
                Push { value: 2 },
                Push { value: 3 },
                Push { value: 4 },
                Push { value: 5 },
            ];
            let handle = spawn_listener(ctx, items);
            handle.join().unwrap();
            // Give the actor time to process all messages
            while done.load(Ordering::SeqCst) < 5 {
                rt::sleep(Duration::from_millis(10));
            }

            let result = actor.request(GetItems).unwrap();
            assert_eq!(result, vec![1, 2, 3, 4, 5]);
        },
    );
}

#[test]
fn listener_stops_on_cancellation() {
    crate::test_wait::bounded(
        concat!(module_path!(), "::listener_stops_on_cancellation"),
        move || {
            let done = Arc::new(AtomicU64::new(0));
            let actor = Collector {
                items: vec![],
                done: done.clone(),
            }
            .start();

            let ctx = actor.context();

            // An iterator that blocks between items, giving us time to cancel
            let iter = (1..=100).map(move |i| {
                if i > 1 {
                    rt::sleep(Duration::from_millis(50));
                }
                Push { value: i }
            });

            let _handle = spawn_listener(ctx, iter);

            // Let at least one item through, then stop the actor
            let first = wait_for(|| done.load(Ordering::SeqCst), |n| *n > 0);
            assert!(first > 0, "listener never delivered an item");
            actor.context().stop();
            actor.join();

            let processed = done.load(Ordering::SeqCst);
            assert!(
                processed < 100,
                "Expected partial processing, but all 100 items were processed"
            );
            assert!(
                processed > 0,
                "Expected at least some items to be processed"
            );
        },
    );
}

#[test]
fn stopped_actor_does_not_poll_iterator() {
    crate::test_wait::bounded(
        concat!(module_path!(), "::stopped_actor_does_not_poll_iterator"),
        move || {
            let polls = Arc::new(AtomicU64::new(0));
            let actor = Collector {
                items: vec![],
                done: Default::default(),
            }
            .start();
            actor.child_handle().stop();
            actor.join();
            let count = polls.clone();
            let iter = std::iter::from_fn(move || {
                count.fetch_add(1, Ordering::SeqCst);
                None::<Push>
            });
            spawn_listener(actor.context(), iter).join().unwrap();
            assert_eq!(polls.load(Ordering::SeqCst), 0);
        },
    );
}
