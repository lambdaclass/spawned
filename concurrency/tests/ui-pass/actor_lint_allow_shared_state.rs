// `#[actor(allow_shared_state, allow_panic_abort)]` silences the shared-state lint (ROADMAP
// rank 6b) for deliberate sharing; `deny(warnings)` proves nothing is left.
// `allow_panic_abort` parses alongside it (the abort warning itself needs the
// `panic-abort` profile; see docs/TESTING.md).
#![deny(warnings)]
use spawned_concurrency::message::Message;
use spawned_concurrency::tasks::{Actor, ActorStart, Context, Handler};
use spawned_concurrency::actor;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

pub struct Tick;
impl Message for Tick {
    type Result = u64;
}

pub struct Shared {
    seen: Arc<AtomicU64>,
    log: Arc<Mutex<Vec<u64>>>,
}

#[actor(allow_shared_state, allow_panic_abort)]
impl Shared {
    pub fn new(seen: Arc<AtomicU64>) -> Self {
        Shared { seen, log: Arc::new(Mutex::new(Vec::new())) }
    }

    #[request_handler]
    async fn tick(&mut self, _msg: Tick, _ctx: &Context<Self>) -> u64 {
        let n = self.seen.fetch_add(1, Ordering::SeqCst);
        self.log.lock().unwrap_or_else(|p| p.into_inner()).push(n);
        n
    }
}

fn main() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let seen = Arc::new(AtomicU64::new(0));
        let actor = Shared::new(seen.clone()).start();
        assert_eq!(actor.request(Tick).await.unwrap(), 0);
        assert_eq!(seen.load(Ordering::SeqCst), 1);
    });
}
