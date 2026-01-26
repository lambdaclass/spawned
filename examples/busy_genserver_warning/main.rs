use spawned_rt::tasks as rt;
use std::time::Duration;
use std::{process::exit, thread};
use tracing::info;

use spawned_concurrency::tasks::{Actor, ActorRef, MessageResponse, RequestResponse};

// We test a scenario with a badly behaved task
struct BusyWorker;

impl BusyWorker {
    pub fn new() -> Self {
        BusyWorker
    }
}

#[derive(Clone)]
pub enum InMessage {
    GetCount,
    Stop,
}

#[derive(Clone)]
pub enum OutMsg {
    Count(u64),
}

impl Actor for BusyWorker {
    type Request = InMessage;
    type Message = ();
    type Reply = ();
    type Error = ();

    async fn handle_request(
        &mut self,
        _: Self::Request,
        _: &ActorRef<Self>,
    ) -> RequestResponse<Self> {
        RequestResponse::Stop(())
    }

    async fn handle_message(
        &mut self,
        _: Self::Message,
        handle: &ActorRef<Self>,
    ) -> MessageResponse {
        info!(taskid = ?rt::task_id(), "sleeping");
        thread::sleep(Duration::from_millis(542));
        handle.clone().send(()).await.unwrap();
        // This sleep is needed to yield control to the runtime.
        // If not, the future never returns and the warning isn't emitted.
        rt::sleep(Duration::from_millis(0)).await;
        MessageResponse::NoReply
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
        let mut badboy = BusyWorker::new().start();
        let _ = badboy.send(()).await;

        rt::sleep(Duration::from_secs(5)).await;
        exit(0);
    })
}
