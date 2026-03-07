use crate::messages;
use crate::threads::{
    spawn_listener, Actor, ActorStart, Context, Handler,
};
use spawned_rt::threads::{self as rt, CancellationToken};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::time::Duration;

struct Collector {
    items: Vec<i32>,
    done: Arc<AtomicU64>,
}

messages! {
    Push { value: i32 } -> ();
    GetItems -> Vec<i32>
}

impl Actor for Collector {}

impl Handler<Push> for Collector {
    fn handle(&mut self, msg: Push, _ctx: &Context<Self>) {
        self.items.push(msg.value);
        self.done.fetch_add(1, Ordering::SeqCst);
    }
}

impl Handler<GetItems> for Collector {
    fn handle(&mut self, _msg: GetItems, _ctx: &Context<Self>) -> Vec<i32> {
        self.items.clone()
    }
}

#[test]
fn listener_consumes_iterator() {
    let done = Arc::new(AtomicU64::new(0));
    let actor = Collector {
        items: vec![],
        done: done.clone(),
    }
    .start();

    let ctx = Context::from_ref(&actor);
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
}

#[test]
fn listener_stops_on_cancellation() {
    let done = Arc::new(AtomicU64::new(0));
    let actor = Collector {
        items: vec![],
        done: done.clone(),
    }
    .start();

    let ctx = Context::from_ref(&actor);
    let ct = CancellationToken::new();
    let ct_clone = ct.clone();

    // An iterator that blocks between items, giving us time to cancel
    let iter = (1..=100).map(move |i| {
        if i > 1 {
            rt::sleep(Duration::from_millis(50));
        }
        Push { value: i }
    });

    let _handle = spawn_listener(ctx, iter);

    // Let a few items through then cancel
    rt::sleep(Duration::from_millis(200));
    ct_clone.cancel();
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
}
