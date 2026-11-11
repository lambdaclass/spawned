// `#[actor(allow_shared_state)]` silences the shared-state lint (ROADMAP
// rank 6b) for deliberate sharing; `deny(warnings)` proves nothing is left.
#![deny(warnings)]
use spawned_concurrency::message::Message;
use spawned_concurrency::tasks::{Actor, ActorStart, Context, Handler};
use spawned_concurrency::actor;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

pub struct Tick;
impl Message for Tick {
    type Result = ();
}

pub struct Shared {
    seen: Arc<AtomicU64>,
    log: Arc<Mutex<Vec<u64>>>,
}

#[actor(allow_shared_state)]
impl Shared {
    pub fn new(seen: Arc<AtomicU64>) -> Self {
        Shared { seen, log: Arc::new(Mutex::new(Vec::new())) }
    }

    #[send_handler]
    async fn tick(&mut self, _msg: Tick, _ctx: &Context<Self>) {
        let n = self.seen.fetch_add(1, Ordering::SeqCst);
        self.log.lock().unwrap_or_else(|p| p.into_inner()).push(n);
    }
}

fn main() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let seen = Arc::new(AtomicU64::new(0));
        let actor = Shared::new(seen.clone()).start();
        actor.send(Tick).unwrap();
        actor.context().stop();
        actor.join().await;
        assert_eq!(seen.load(Ordering::SeqCst), 1);
    });
}
