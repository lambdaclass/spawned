// No shared-state warning (ROADMAP rank 6b) for owned state, owned locks, or
// `Arc` of immutable data; `deny(warnings)` proves the lint stays quiet.
#![deny(warnings)]
use spawned_concurrency::message::Message;
use spawned_concurrency::tasks::{Actor, ActorStart, Context, Handler};
use spawned_concurrency::actor;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub struct Get;
impl Message for Get {
    type Result = usize;
}

pub struct Owned {
    count: u64,
    names: HashMap<String, String>,
    config: Arc<String>,
    table: Arc<Vec<u8>>,
    label: Arc<str>,
    callback: Arc<dyn Fn(u64) -> u64 + Send + Sync>,
    // A lock the actor owns outright is not shared.
    owned_lock: Mutex<u64>,
}

#[actor]
impl Owned {
    pub fn new(config: Arc<String>, table: std::sync::Arc<Vec<u8>>) -> Self {
        Owned {
            count: 0,
            names: HashMap::new(),
            config,
            table,
            label: Arc::from("owned"),
            callback: Arc::new(|n| n + 1),
            owned_lock: Mutex::new(0),
        }
    }

    #[request_handler]
    async fn get(&mut self, _msg: Get, _ctx: &Context<Self>) -> usize {
        self.count = (self.callback)(self.count);
        *self.owned_lock.get_mut().unwrap_or_else(|p| p.into_inner()) += 1;
        self.names.len() + self.config.len() + self.table.len() + self.label.len()
    }
}

fn main() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let actor = Owned::new(Arc::new("cfg".into()), Arc::new(vec![1, 2])).start();
        assert_eq!(actor.request(Get).await.unwrap(), 10);
    });
}
