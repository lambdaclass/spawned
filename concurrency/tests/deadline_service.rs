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
        owner
            .__in_turn(|_, ctx| {
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
            .request_raw_until(Park, Deadline::after(Duration::from_millis(50)))
            .unwrap();
        match tokio::time::timeout(Duration::from_secs(5), parked).await {
            Ok(Err(ActorError::RequestTimeout)) => {}
            Ok(other) => panic!("unexpected outcome {other:?}"),
            Err(_) => panic!("the deadline service stopped: a parked request never expired"),
        }
        // A job deadline still fires too.
        let fired = other
            .__in_turn(|_, ctx| {
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
