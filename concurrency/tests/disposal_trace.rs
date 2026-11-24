//! Records the deadline service's hand-off to the disposal workers from real
//! actors, for TLA+ trace validation against `verification/tla/Disposal.tla`
//! (verification/tla/README.md). Each scenario writes `disposal_<name>.json`
//! to `$LIFECYCLE_TRACE_DIR` (default `target/lifecycle-traces`);
//! `verification/tla/trace_check.py` checks them (schema `disposal-v1`). A
//! negative control recorded from deliberately broken code (the
//! `disposal.batch_serial` failpoint) goes to `controls/` there, and
//! `negative_checks.py` requires it to be rejected.
//!
//! The recording is process-wide (the service and the pool are), so
//! scenarios run one at a time, start with the pool quiescent and wait for
//! their workers before taking the trace.
#![cfg(all(feature = "lifecycle-trace", feature = "async"))]

use spawned_concurrency::{
    lifecycle_trace,
    message::Message,
    tasks::{Actor, ActorRef, ActorStart, Context, Handler, Instant, JobOptions, JobOutcome},
    ActorError, Deadline, PendingReply, ReplyTo,
};
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

/// One recording at a time (held across awaits).
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn trace_dir() -> PathBuf {
    std::env::var_os("LIFECYCLE_TRACE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/lifecycle-traces")
        })
}

/// Let the pool settle (every worker waiting), then record.
async fn record() {
    tokio::time::sleep(Duration::from_millis(100)).await;
    lifecycle_trace::record_disposal();
}

/// Let the workers finish, stop recording and write the trace to `dir`;
/// returns the events as `Action:x`.
async fn write_to(dir: PathBuf, name: &str) -> Vec<String> {
    tokio::time::sleep(Duration::from_millis(100)).await;
    let events = lifecycle_trace::take_disposal();
    let json = format!(
        "{{\"name\":\"disposal_{name}\",\"schema\":\"disposal-v1\",\"events\":[\n{}\n]}}\n",
        events
            .iter()
            .map(|e| e.to_json())
            .collect::<Vec<_>>()
            .join(",\n")
    );
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("disposal_{name}.json")), json).unwrap();
    events
        .iter()
        .map(|e| {
            if e.x.is_empty() {
                e.action.to_string()
            } else {
                format!("{}:{}", e.action, e.x)
            }
        })
        .collect()
}

async fn write(name: &str) -> Vec<String> {
    write_to(trace_dir(), name).await
}

fn count(events: &[String], event: &str) -> usize {
    events.iter().filter(|e| *e == event).count()
}

/// A payload whose destructor is counted.
struct Payload(Arc<AtomicUsize>);
impl Message for Payload {
    type Result = ();
}
impl Drop for Payload {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

struct Done;
impl Message for Done {
    type Result = ();
}

/// Parks requests (deferred, never answered); `stopped()` waits for `gate`.
#[derive(Default)]
struct Owner {
    parked: Vec<ReplyTo<()>>,
    gate: Option<(std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>)>,
}
impl Actor for Owner {
    async fn stopped(&mut self, _: &Context<Self>) {
        if let Some((gate, stopping)) = self.gate.take() {
            let _ = stopping.send(());
            let _ = gate.recv_timeout(Duration::from_secs(10));
        }
    }
}
struct Park;
impl Message for Park {
    type Result = ();
}
impl Handler<Park> for Owner {
    async fn handle(&mut self, _: Park, reply: PendingReply<'_, ()>, _: &Context<Self>) {
        self.parked.push(reply.defer().unwrap());
    }
}
impl Handler<Payload> for Owner {
    async fn handle(&mut self, _: Payload, _: PendingReply<'_, ()>, _: &Context<Self>) {}
}
impl Handler<Done> for Owner {
    async fn handle(&mut self, _: Done, _: PendingReply<'_, ()>, _: &Context<Self>) {}
}

/// A job whose deadline (20 ms) takes its notification while the work runs;
/// its emit scheduled at 40 ms is then discarded on the deadline service.
async fn discard_an_emit(owner: &ActorRef<Owner>, drops: &Arc<AtomicUsize>) {
    let payload = Payload(drops.clone());
    spawned_concurrency::__private::in_turn(owner, move |_, ctx| {
        let opts = JobOptions::new().deadline(Duration::from_millis(20));
        ctx.pipe_blocking(
            opts,
            move |job| {
                let at = Instant::now() + Duration::from_millis(40);
                job.emit_at_blocking(at, payload).unwrap();
                std::thread::sleep(Duration::from_millis(150));
            },
            |_: JobOutcome<()>| Done,
        )
        .unwrap();
    })
    .unwrap()
    .await
    .unwrap();
}

/// Two callers waiting with the same deadline (expired in one visit) and
/// one that never polls.
async fn expire_callers(owner: &ActorRef<Owner>) {
    let deadline = Deadline::after(Duration::from_millis(50));
    let first = owner.send_request(Park, deadline).unwrap();
    let second = owner.send_request(Park, deadline).unwrap();
    let _unpolled = owner.send_request(Park, deadline).unwrap();
    let (a, b) = tokio::join!(first, second);
    assert!(matches!(a, Err(ActorError::RequestTimeout)));
    assert!(matches!(b, Err(ActorError::RequestTimeout)));
}

async fn stop(owner: ActorRef<Owner>) {
    owner.context().stop();
    owner.join().await;
}

/// Expired requests: two waiting callers are woken by workers, a third
/// (never polled) has nothing to wake.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_callers_are_woken_by_workers() {
    let _serial = SERIAL.lock().await;
    let owner = Owner::default().spawn();
    record().await;
    expire_callers(&owner).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    stop(owner).await;
    let events = write("expired_callers").await;
    assert_eq!(count(&events, "Expire:wake"), 2, "{events:?}");
    assert_eq!(count(&events, "Expire:none"), 1, "{events:?}");
    assert!(count(&events, "Run") >= 2, "{events:?}");
    assert_eq!(count(&events, "Run"), count(&events, "Done"), "{events:?}");
}

/// Payloads released on the deadline service are dropped by workers while
/// the owner runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn released_payloads_are_dropped_by_workers() {
    let _serial = SERIAL.lock().await;
    let owner = Owner::default().spawn();
    let drops = Arc::new(AtomicUsize::new(0));
    record().await;
    discard_an_emit(&owner, &drops).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        drops.load(Ordering::SeqCst),
        1,
        "the discarded emit was dropped"
    );
    stop(owner).await;
    let events = write("released_payloads").await;
    assert!(count(&events, "Defer:drop") >= 1, "{events:?}");
    assert_eq!(
        count(&events, "Settle:dropped"),
        count(&events, "Defer:drop"),
        "{events:?}"
    );
    assert_eq!(count(&events, "Settle:held"), 0, "{events:?}");
}

#[cfg(feature = "failpoints")]
mod with_failpoints {
    use super::*;
    use spawned_concurrency::failpoints::{self, Action, FailPoint, FailScenario};

    /// A payload handed off before the owner's exit, reaching its worker
    /// after the exit began (the worker is delayed at `disposal.item`), is
    /// held and dropped only at the death point.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_payload_handed_off_before_exit_waits_for_the_death_point() {
        let _serial = SERIAL.lock().await;
        let _scenario = FailScenario::setup();
        let (release, gate) = std::sync::mpsc::channel();
        let (stopping_tx, stopping) = std::sync::mpsc::channel();
        let owner = Owner {
            gate: Some((gate, stopping_tx)),
            ..Owner::default()
        }
        .spawn();
        let drops = Arc::new(AtomicUsize::new(0));
        record().await;
        failpoints::configure(
            "disposal.item",
            FailPoint::new(Action::Delay(Duration::from_millis(400))),
        )
        .unwrap();
        discard_an_emit(&owner, &drops).await;
        // The emit is discarded at 40 ms; its worker waits until ~440 ms.
        tokio::time::sleep(Duration::from_millis(100)).await;
        owner.context().stop();
        tokio::task::spawn_blocking(move || stopping.recv_timeout(Duration::from_secs(5)))
            .await
            .unwrap()
            .expect("stopped() began");
        // The worker settles the payload while stopped() still runs.
        tokio::time::sleep(Duration::from_millis(600)).await;
        let before_death = drops.load(Ordering::SeqCst);
        release.send(()).unwrap();
        owner.join().await;
        failpoints::remove("disposal.item");
        let events = write("held_until_death").await;
        assert_eq!(
            before_death, 0,
            "a payload handed off before the exit was dropped before the death point"
        );
        assert_eq!(
            drops.load(Ordering::SeqCst),
            1,
            "dropped at the death point"
        );
        let held = events.iter().position(|e| e == "Settle:held");
        let exit = events.iter().position(|e| e == "Exit");
        let death = events.iter().position(|e| e == "Death");
        assert!(exit < held && held < death && held.is_some(), "{events:?}");
    }

    /// Idle workers retire (keep-alive shortened) and later work starts new
    /// ones.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn idle_workers_retire_and_new_ones_start() {
        let _serial = SERIAL.lock().await;
        let _scenario = FailScenario::setup();
        failpoints::configure("disposal.keep_alive", FailPoint::new(Action::Error)).unwrap();
        // Let the workers left by earlier scenarios retire first.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let owner = Owner::default().spawn();
        record().await;
        expire_callers(&owner).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        expire_callers(&owner).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        stop(owner).await;
        let events = write("workers_retire").await;
        failpoints::remove("disposal.keep_alive");
        assert!(count(&events, "Retire") >= 1, "{events:?}");
        assert!(count(&events, "Begin") >= 2, "{events:?}");
    }

    /// Negative control recorded from broken code: with
    /// `disposal.batch_serial` the batch runs serially on one worker. The
    /// callers are still woken (nothing blocks here), but the trace must be
    /// rejected: a worker runs an item it never took.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn batch_serial_control() {
        let _serial = SERIAL.lock().await;
        let _scenario = FailScenario::setup();
        let owner = Owner::default().spawn();
        record().await;
        failpoints::configure("disposal.batch_serial", FailPoint::new(Action::Error)).unwrap();
        expire_callers(&owner).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        failpoints::remove("disposal.batch_serial");
        stop(owner).await;
        let events = write_to(trace_dir().join("controls"), "batch_serial").await;
        assert_eq!(count(&events, "Expire:wake"), 2, "{events:?}");
        assert!(count(&events, "Run") > count(&events, "Take"), "{events:?}");
    }
}
