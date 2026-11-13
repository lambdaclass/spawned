#![deny(warnings)]
use spawned_concurrency::{actor, message::Message, tasks::{Actor, Context, Handler}};
struct Allow;
impl Message for Allow { type Result = u64; }
struct Expect;
impl Message for Expect { type Result = u64; }
#[deprecated]
fn old_value() -> u64 { 17 }
struct Constrained;
impl Message for Constrained { type Result = u64; }
struct Worker;
#[actor]
impl Worker {
    #[request_handler]
    async fn constrained(&mut self, _: Constrained, _: &Context<Self>) -> u64
    where Self: Send { 23 }
    #[request_handler]
    #[allow(deprecated)]
    async fn allowed(&mut self, _: Allow, _: &Context<Self>) -> u64 { old_value() }
    #[request_handler]
    #[expect(deprecated)]
    async fn expected(&mut self, _: Expect, _: &Context<Self>) -> u64 { old_value() }
}
fn main() { let _ = (Worker, Allow, Expect, Constrained); }
