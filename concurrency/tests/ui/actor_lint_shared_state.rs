// The shared-state lint (ROADMAP rank 6b) is a warning; `deny` turns each one
// into an error so the snapshot proves every pattern fires.
#![deny(warnings)]
use spawned_concurrency::message::Message;
use spawned_concurrency::tasks::{Actor, Context, Handler};
use spawned_concurrency::actor;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, AtomicUsize};
use std::sync::{Arc, Mutex, RwLock};

pub struct Tick;
impl Message for Tick {
    type Result = ();
}

pub struct Shared {
    a: Arc<Mutex<u64>>,
    b: std::sync::Arc<std::sync::RwLock<u64>>,
    c: Arc<tokio::sync::Mutex<u64>>,
    d: Arc<tokio::sync::RwLock<u64>>,
    e: Arc<AtomicU64>,
    f: Arc<AtomicUsize>,
    g: Arc<RwLock<u64>>,
}

#[actor]
impl Shared {
    pub fn new(
        a: Arc<Mutex<u64>>,
        b: std::sync::Arc<std::sync::RwLock<u64>>,
        c: Arc<tokio::sync::Mutex<u64>>,
        d: Arc<tokio::sync::RwLock<u64>>,
        e: Arc<AtomicU64>,
    ) -> Self {
        Shared { a, b, c, d, e, f: Arc::new(AtomicUsize::new(0)), g: Arc::new(RwLock::new(0)) }
    }

    fn scratch(&self) -> Rc<RefCell<u64>> {
        std::rc::Rc::new(std::cell::RefCell::new(0))
    }

    #[send_handler]
    async fn tick(&mut self, _msg: Tick, _ctx: &Context<Self>) {
        let _ = (&self.a, &self.b, &self.c, &self.d, &self.e, &self.f, &self.g);
        let _ = self.scratch();
    }
}

fn main() {}
