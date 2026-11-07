//! A dedicated actor can block its root thread while an async helper progresses.
use spawned_concurrency::{
    message::Message,
    tasks::{Actor, ActorStart, Backend, Context, Handler},
};
use std::{sync::mpsc, thread, time::Duration};

struct Worker;
impl Actor for Worker {}
struct Check;
impl Message for Check {
    type Result = (usize, u64);
}
impl Handler<Check> for Worker {
    async fn handle(&mut self, _: Check, _: &Context<Self>) -> (usize, u64) {
        let root = thread::current().id();
        let workers = tokio::runtime::Handle::current().metrics().num_workers();
        let (tx, rx) = mpsc::channel();
        let helper = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(1)).await;
            assert_ne!(root, thread::current().id());
            tx.send(42).unwrap();
        });
        // This would stall a current-thread executor. The isolated runtime's
        // one async worker can drive the helper independently of this root.
        let answer = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        helper.await.unwrap();
        assert_eq!(root, thread::current().id());
        (workers, answer)
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let actor = Worker.start_with_backend(Backend::Thread);
    let (workers, answer) = actor.request(Check).await.unwrap();
    assert_eq!(workers, 1);
    assert_eq!(answer, 42);
    println!(
        "dedicated actor root + {workers} async helper worker; helper progressed while root waited"
    );
    actor.child_handle().stop();
    actor.join().await;
    // spawn_blocking, filesystem and DNS work can create additional threads.
}
