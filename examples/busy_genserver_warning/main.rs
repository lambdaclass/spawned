use spawned_concurrency::message::Message;
use spawned_concurrency::tasks::{Actor, ActorStart as _, Context, Handler};
use spawned_rt::tasks as rt;
use std::time::Duration;
use std::{process::exit, thread};
use tracing::info;

// We test a scenario with a badly behaved task
struct BusyWorker;

#[derive(Debug)]
pub struct DoWork;
impl Message for DoWork {
    type Result = ();
}

impl Actor for BusyWorker {}

impl Handler<DoWork> for BusyWorker {
    async fn handle(&mut self, _msg: DoWork, ctx: &Context<Self>) {
        info!(taskid = ?rt::task_id(), "sleeping");
        thread::sleep(Duration::from_millis(542));
        let _ = ctx.send(DoWork);
        // This sleep is needed to yield control to the runtime.
        // If not, the future never returns and the warning isn't emitted.
        rt::sleep(Duration::from_millis(0)).await;
    }
}

/// Example of a program with a semi-blocking [`Actor`].
/// As mentioned in the `blocking_genserver` example, tasks that block can block
/// the entire runtime in cooperative multitasking models. This is easy to find
/// in practice, since it appears as if the whole world stopped. However, most
/// of the time, tasks simply take longer than expected, which can lead to
/// service degradation and increased latency. To tackle this, we print a warning
/// whenever we detect tasks that take too long to run.
pub fn main() {
    rt::run(async move {
        // If we change BusyWorker to Backend::Blocking instead, it won't print the warning
        let badboy = BusyWorker.start();
        let _ = badboy.send(DoWork);

        rt::sleep(Duration::from_secs(5)).await;
        exit(0);
    })
}
