use std::collections::HashMap;

use spawned_concurrency::tasks::{Actor, Context, Handler};
use spawned_concurrency::actor;

use crate::protocols::name_server_protocol::{Add, Find};
use crate::protocols::{FindResult, NameServerProtocol};

pub struct NameServer {
    inner: HashMap<String, String>,
}

#[actor(protocol = NameServerProtocol)]
impl NameServer {
    pub fn new() -> Self {
        NameServer {
            inner: HashMap::new(),
        }
    }

    #[request_handler]
    async fn handle_add(&mut self, msg: Add, _ctx: &Context<Self>) {
        self.inner.insert(msg.key, msg.value);
    }

    #[request_handler]
    async fn handle_find(&mut self, msg: Find, _ctx: &Context<Self>) -> FindResult {
        match self.inner.get(&msg.key) {
            Some(value) => FindResult::Found { value: value.clone() },
            None => FindResult::NotFound,
        }
    }
}
