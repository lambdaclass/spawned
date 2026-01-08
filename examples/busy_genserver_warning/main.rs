use spawned_rt::tasks as rt;
use std::time::Duration;
use std::{process::exit, thread};
use tracing::info;

use spawned_concurrency::{Backend, CallResponse, CastResponse, GenServer, GenServerHandle};

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

impl GenServer for BusyWorker {
    type CallMsg = InMessage;
    type CastMsg = ();
    type OutMsg = ();
    type Error = ();

    async fn handle_call(
        &mut self,
        _: Self::CallMsg,
        _: &GenServerHandle<Self>,
    ) -> CallResponse<Self> {
        CallResponse::Stop(())
    }

    async fn handle_cast(
        &mut self,
        _: Self::CastMsg,
        handle: &GenServerHandle<Self>,
    ) -> CastResponse {
        info!(taskid = ?rt::task_id(), "sleeping");
        thread::sleep(Duration::from_millis(542));
        handle.clone().cast(()).await.unwrap();
        // This sleep is needed to yield control to the runtime.
        // If not, the future never returns and the warning isn't emitted.
        rt::sleep(Duration::from_millis(0)).await;
        CastResponse::NoReply
    }
}

/// Example of a program with a semi-blocking [`GenServer`].
/// As mentioned in the `blocking_genserver` example, tasks that block can block
/// the entire runtime in cooperative multitasking models. This is easy to find
/// in practice, since it appears as if the whole world stopped. However, most
/// of the time, tasks simply take longer than expected, which can lead to
/// service degradation and increased latency. To tackle this, we print a warning
/// whenever we detect tasks that take too long to run.
pub fn main() {
    rt::run(async move {
        // If we change BusyWorker to Backend::Blocking instead, it won't print the warning
        let mut badboy = BusyWorker::new().start(Backend::Async);
        let _ = badboy.cast(()).await;

        rt::sleep(Duration::from_secs(5)).await;
        exit(0);
    })
}
