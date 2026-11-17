//! Review P0 (Codex 2): user payload dropped on the process-wide deadline
//! service thread must not end the service. A job's deadline takes its
//! terminal notification while the work runs; the job's scheduled emit is
//! then discarded at its instant on the service thread, and its message's
//! destructor panics. Another actor's request deadline must still fire.
//!
//! The service is process-wide, so the scenario runs in a child process.
#![cfg(all(feature = "async", not(spawned_sim)))] // native jobs are not simulated
use spawned_concurrency::{
    message::Message,
    tasks::{Actor, ActorStart, Context, Handler, Instant, JobOptions, JobOutcome},
    ActorError, Deadline, PendingReply, ReplyTo,
};
use std::time::Duration;
#[path = "support/drop_subprocess.rs"]
mod subprocess;

/// A message whose destructor panics while armed.
struct Bomb(bool);
impl Message for Bomb {
    type Result = ();
}
impl Drop for Bomb {
    fn drop(&mut self) {
        if self.0 {
            panic!("scheduled emit destructor");
        }
    }
}

struct Done;
impl Message for Done {
    type Result = ();
}

/// Starts the job; parks requests (deferred, never answered).
#[derive(Default)]
struct Owner(Vec<ReplyTo<()>>);
impl Actor for Owner {}
impl Handler<Bomb> for Owner {
    async fn handle(&mut self, mut msg: Bomb, _: PendingReply<'_, ()>, _: &Context<Self>) {
        msg.0 = false; // delivered: not the scenario under test
    }
}
impl Handler<Done> for Owner {
    async fn handle(&mut self, _: Done, _: PendingReply<'_, ()>, _: &Context<Self>) {}
}
struct Park;
impl Message for Park {
    type Result = ();
}
impl Handler<Park> for Owner {
    async fn handle(&mut self, _: Park, reply: PendingReply<'_, ()>, _: &Context<Self>) {
        self.0.push(reply.defer().unwrap());
    }
}

#[test]
fn deadline_service_survives_a_panicking_payload_destructor() {
    subprocess::run("deadline_service_child", "deadline service survived");
}

#[test]
#[ignore = "subprocess helper exercised by the enabled parent"]
fn deadline_service_child() {
    if std::env::var_os("SPAWNED_MAILBOX_DROP_CHILD").is_none() {
        return;
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let owner = Owner::default().start();
        spawned_concurrency::__private::in_turn(&owner, |_, ctx| {
            let opts = JobOptions::new().deadline(Duration::from_millis(20));
            ctx.pipe_blocking(
                opts,
                |job| {
                    let at = Instant::now() + Duration::from_millis(40);
                    job.emit_at_blocking(at, Bomb(true)).unwrap();
                    std::thread::sleep(Duration::from_millis(150));
                },
                |_: JobOutcome<()>| Done,
            )
            .unwrap();
        })
        .unwrap()
        .await
        .unwrap();
        // The deadline (20 ms) took the notification; the emit is discarded
        // at 40 ms on the service thread and its destructor panics.
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Another actor's deadlines, serviced by the same thread.
        let other = Owner::default().start();
        let parked = other
            .send_request(Park, Deadline::after(Duration::from_millis(50)))
            .unwrap();
        match tokio::time::timeout(Duration::from_secs(5), parked).await {
            Ok(Err(ActorError::RequestTimeout)) => {}
            Ok(other) => panic!("unexpected outcome {other:?}"),
            Err(_) => panic!("the deadline service stopped: a parked request never expired"),
        }
        // A job deadline still fires too.
        let fired = spawned_concurrency::__private::in_turn(&other, |_, ctx| {
            let (tx, rx) = std::sync::mpsc::channel();
            let opts = JobOptions::new().deadline(Duration::from_millis(20));
            ctx.pipe_blocking(
                opts,
                |_| std::thread::sleep(Duration::from_millis(300)),
                move |outcome: JobOutcome<()>| {
                    let _ = tx.send(outcome.is_err());
                    Done
                },
            )
            .unwrap();
            rx
        })
        .unwrap()
        .await
        .unwrap();
        let deadline =
            tokio::task::spawn_blocking(move || fired.recv_timeout(Duration::from_secs(5)))
                .await
                .unwrap();
        assert_eq!(deadline, Ok(true), "a job deadline did not fire");
        other.context().stop();
        other.join().await;
        owner.context().stop();
        owner.join().await;
    });
    println!("deadline service survived");
}

// ---------------------------------------------------------------------------
// Disposal isolation: user code that expiry releases (a payload destructor, a
// caller's waker) never runs on the shared deadline thread, so a blocking one
// in actor A does not delay actor B's deadlines.
// ---------------------------------------------------------------------------

/// A one-shot latch with bounded waits.
#[derive(Default)]
struct Latch(std::sync::Mutex<bool>, std::sync::Condvar);
impl Latch {
    fn set(&self) {
        *self.0.lock().unwrap() = true;
        self.1.notify_all();
    }
    fn wait(&self, limit: Duration) -> bool {
        let guard = self.0.lock().unwrap();
        let (guard, _) = self
            .1
            .wait_timeout_while(guard, limit, |set| !*set)
            .unwrap();
        *guard
    }
}

/// Blocking user code: signals `entered`, then waits (bounded) for `release`.
#[derive(Default)]
struct Blocker {
    entered: Latch,
    release: Latch,
}
impl Blocker {
    fn block(&self) {
        self.entered.set();
        self.release.wait(Duration::from_secs(10));
    }
}

/// A message whose destructor blocks while armed.
struct Stall(Option<std::sync::Arc<Blocker>>);
impl Message for Stall {
    type Result = ();
}
impl Drop for Stall {
    fn drop(&mut self) {
        if let Some(blocker) = self.0.take() {
            blocker.block();
        }
    }
}
impl Handler<Stall> for Owner {
    async fn handle(&mut self, mut msg: Stall, _: PendingReply<'_, ()>, _: &Context<Self>) {
        msg.0 = None; // delivered: not the scenario under test
    }
}

/// A waker whose `wake` blocks.
struct BlockingWake(std::sync::Arc<Blocker>);
impl std::task::Wake for BlockingWake {
    fn wake(self: std::sync::Arc<Self>) {
        self.0.block();
    }
}

#[test]
fn deadline_service_isolates_a_blocking_payload_destructor() {
    subprocess::run(
        "deadline_service_blocking_destructor_child",
        "blocking destructor isolated",
    );
}

#[test]
fn deadline_service_isolates_a_blocking_waker() {
    subprocess::run(
        "deadline_service_blocking_waker_child",
        "blocking waker isolated",
    );
}

#[test]
fn deadline_service_isolates_a_blocking_destructor_threads() {
    subprocess::run(
        "deadline_service_blocking_destructor_threads_child",
        "threads blocking destructor isolated",
    );
}

/// Another actor's raw request and job deadlines, serviced by the heap.
async fn other_actor_deadlines_fire(blocker: &Blocker) -> Result<(), String> {
    let other = Owner::default().start();
    let parked = other
        .send_request(Park, Deadline::after(Duration::from_millis(50)))
        .unwrap();
    let result = match tokio::time::timeout(Duration::from_secs(3), parked).await {
        Ok(Err(ActorError::RequestTimeout)) => Ok(()),
        Ok(other) => Err(format!("unexpected outcome {other:?}")),
        Err(_) => Err("a blocked disposal delayed another actor's request deadline".into()),
    };
    let fired = spawned_concurrency::__private::in_turn(&other, |_, ctx| {
        let (tx, rx) = std::sync::mpsc::channel();
        let opts = JobOptions::new().deadline(Duration::from_millis(20));
        ctx.pipe_blocking(
            opts,
            |_| std::thread::sleep(Duration::from_millis(300)),
            move |outcome: JobOutcome<()>| {
                let _ = tx.send(outcome.is_err());
                Done
            },
        )
        .unwrap();
        rx
    })
    .unwrap()
    .await
    .unwrap();
    let job = tokio::task::spawn_blocking(move || fired.recv_timeout(Duration::from_secs(3)))
        .await
        .unwrap();
    // Release the blocked disposal before reporting, so the child exits.
    blocker.release.set();
    let result = result.and(match job {
        Ok(true) => Ok(()),
        other => Err(format!(
            "a blocked disposal delayed another actor's job deadline: {other:?}"
        )),
    });
    other.context().stop();
    other.join().await;
    result
}

fn tokio_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
}

#[test]
#[ignore = "subprocess helper exercised by the enabled parent"]
fn deadline_service_blocking_destructor_child() {
    if std::env::var_os("SPAWNED_MAILBOX_DROP_CHILD").is_none() {
        return;
    }
    let blocker = std::sync::Arc::new(Blocker::default());
    tokio_runtime().block_on(async {
        let owner = Owner::default().start();
        let armed = blocker.clone();
        spawned_concurrency::__private::in_turn(&owner, move |_, ctx| {
            let opts = JobOptions::new().deadline(Duration::from_millis(20));
            ctx.pipe_blocking(
                opts,
                move |job| {
                    // Discarded at 40 ms (the deadline took the notification
                    // at 20 ms): its destructor blocks.
                    let at = Instant::now() + Duration::from_millis(40);
                    job.emit_at_blocking(at, Stall(Some(armed))).unwrap();
                    std::thread::sleep(Duration::from_millis(150));
                },
                |_: JobOutcome<()>| Done,
            )
            .unwrap();
        })
        .unwrap()
        .await
        .unwrap();
        assert!(
            blocker.entered.wait(Duration::from_secs(5)),
            "the scheduled emit was never discarded"
        );
        let result = other_actor_deadlines_fire(&blocker).await;
        owner.context().stop();
        owner.join().await;
        result.unwrap();
    });
    println!("blocking destructor isolated");
}

#[test]
#[ignore = "subprocess helper exercised by the enabled parent"]
fn deadline_service_blocking_waker_child() {
    if std::env::var_os("SPAWNED_MAILBOX_DROP_CHILD").is_none() {
        return;
    }
    let blocker = std::sync::Arc::new(Blocker::default());
    tokio_runtime().block_on(async {
        let owner = Owner::default().start();
        // A raw receiver polled once with a waker that blocks: its deadline
        // goes to the owner's heap, whose expiry wakes it.
        let receiver = owner
            .send_request(Park, Deadline::after(Duration::from_millis(30)))
            .unwrap();
        let mut receiver = Box::pin(std::future::IntoFuture::into_future(receiver));
        let waker = std::task::Waker::from(std::sync::Arc::new(BlockingWake(blocker.clone())));
        let mut cx = std::task::Context::from_waker(&waker);
        assert!(std::future::Future::poll(receiver.as_mut(), &mut cx).is_pending());
        assert!(
            blocker.entered.wait(Duration::from_secs(5)),
            "the receiver's waker never ran"
        );
        let result = other_actor_deadlines_fire(&blocker).await;
        drop(receiver);
        owner.context().stop();
        owner.join().await;
        result.unwrap();
    });
    println!("blocking waker isolated");
}

mod threads_backend {
    use super::*;
    use spawned_concurrency::threads::{
        Actor, ActorStart, Context, Handler, JobOptions, JobOutcome,
    };

    pub struct Owner;
    impl Actor for Owner {}
    impl Handler<Stall> for Owner {
        fn handle(&mut self, mut msg: Stall, _: PendingReply<'_, ()>, _: &Context<Self>) {
            msg.0 = None;
        }
    }
    impl Handler<Done> for Owner {
        fn handle(&mut self, _: Done, _: PendingReply<'_, ()>, _: &Context<Self>) {}
    }

    pub fn run(blocker: &std::sync::Arc<Blocker>) -> Result<(), String> {
        let owner = Owner.start();
        let armed = blocker.clone();
        spawned_concurrency::__private::in_turn_threads(&owner, move |_, ctx| {
            let opts = JobOptions::new().deadline(Duration::from_millis(20));
            ctx.pipe_blocking(
                opts,
                move |job| {
                    let at = Instant::now() + Duration::from_millis(40);
                    job.emit_at_blocking(at, Stall(Some(armed))).unwrap();
                    std::thread::sleep(Duration::from_millis(150));
                },
                |_: JobOutcome<()>| Done,
            )
            .unwrap();
        })
        .unwrap()
        .recv()
        .unwrap();
        assert!(
            blocker.entered.wait(Duration::from_secs(5)),
            "the scheduled emit was never discarded"
        );
        // Another actor's job deadline, serviced by the same deadline service.
        let other = Owner.start();
        let fired = spawned_concurrency::__private::in_turn_threads(&other, |_, ctx| {
            let (tx, rx) = std::sync::mpsc::channel();
            let opts = JobOptions::new().deadline(Duration::from_millis(20));
            ctx.pipe_blocking(
                opts,
                |_| std::thread::sleep(Duration::from_millis(300)),
                move |outcome: JobOutcome<()>| {
                    let _ = tx.send(outcome.is_err());
                    Done
                },
            )
            .unwrap();
            rx
        })
        .unwrap()
        .recv()
        .unwrap();
        let job = fired.recv_timeout(Duration::from_secs(3));
        blocker.release.set();
        let result = match job {
            Ok(true) => Ok(()),
            other => Err(format!(
                "a blocked disposal delayed another actor's job deadline: {other:?}"
            )),
        };
        other.context().stop();
        other.join();
        owner.context().stop();
        owner.join();
        result
    }
}

#[test]
#[ignore = "subprocess helper exercised by the enabled parent"]
fn deadline_service_blocking_destructor_threads_child() {
    if std::env::var_os("SPAWNED_MAILBOX_DROP_CHILD").is_none() {
        return;
    }
    let blocker = std::sync::Arc::new(Blocker::default());
    threads_backend::run(&blocker).unwrap();
    println!("threads blocking destructor isolated");
}
