// `#[send_handler]` in a crate that renames `spawned-concurrency`: the
// local module shadows the original name, so a hard-coded
// `spawned_concurrency::` path in the expansion fails to resolve.
extern crate spawned_concurrency as my_spawned;

#[allow(dead_code)]
mod spawned_concurrency {}

use my_spawned::message::Message;
use my_spawned::send_handler;
use my_spawned::tasks::{Actor, Context, Handler};

struct Poller;
impl Actor for Poller {}

struct Tick;
impl Message for Tick {
    type Result = ();
}
struct Tock;
impl Message for Tock {
    type Result = ();
}

// A qualified `Handler` path names the reply type through its module.
#[send_handler]
impl my_spawned::tasks::Handler<Tick> for Poller {
    async fn handle(&mut self, _msg: Tick, _ctx: &Context<Self>) {}
}

// A bare `Handler` takes the renamed crate explicitly.
#[send_handler(crate = my_spawned)]
impl Handler<Tock> for Poller {
    async fn handle(&mut self, _msg: Tock, _ctx: &Context<Self>) {}
}

fn main() {}
