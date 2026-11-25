// A `get_state` callback runs on the actor: it must be `Send`.
use spawned_concurrency::tasks::{Actor, ActorRef};
use std::rc::Rc;
pub struct Counter;
impl Actor for Counter {}
fn read(actor: ActorRef<Counter>) {
    let local = Rc::new(1);
    let _ = actor.get_state(move |_| *local);
}
fn main() {}
