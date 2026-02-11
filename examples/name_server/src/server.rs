use std::collections::HashMap;

use spawned_concurrency::tasks::{Actor, Context, Handler};

use crate::messages::*;

pub struct NameServer {
    inner: HashMap<String, String>,
}

impl NameServer {
    pub fn new() -> Self {
        NameServer {
            inner: HashMap::new(),
        }
    }
}

impl Actor for NameServer {}

impl Handler<Add> for NameServer {
    async fn handle(&mut self, msg: Add, _ctx: &Context<Self>) {
        self.inner.insert(msg.key, msg.value);
    }
}

impl Handler<Find> for NameServer {
    async fn handle(&mut self, msg: Find, _ctx: &Context<Self>) -> FindResult {
        match self.inner.get(&msg.key) {
            Some(value) => FindResult::Found { value: value.clone() },
            None => FindResult::NotFound,
        }
    }
}
