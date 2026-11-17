//! Records the deadline service's hand-off to the disposal workers from real
//! actors, for TLA+ trace validation against `verification/tla/Disposal.tla`
//! (verification/tla/README.md). Each scenario writes `disposal_<name>.json`
//! to `$LIFECYCLE_TRACE_DIR` (default `target/lifecycle-traces`);
//! `verification/tla/trace_check.py` checks them (schema `disposal-v1`).
//!
//! The recording is process-wide (the service is), so scenarios run one at a
//! time and each waits for its workers before taking the trace.
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

/// Stop recording and write the trace; returns the events as `Action:x`.
fn write(name: &str) -> Vec<String> {
    let events = lifecycle_trace::take_disposal();
    let json = format!(
        "{{\"name\":\"disposal_{name}\",\"schema\":\"disposal-v1\",\"events\":[\n{}\n]}}\n",
        events
            .iter()
            .map(|e| e.to_json())
            .collect::<Vec<_>>()
            .join(",\n")
    );
    let dir = trace_dir();
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

/// Expired requests: two waiting callers are woken by workers, a third
/// (never polled) has nothing to wake.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_callers_are_woken_by_workers() {
    let _serial = SERIAL.lock().await;
    let owner = Owner::default().start();
    lifecycle_trace::record_disposal();
    let deadline = Deadline::after(Duration::from_millis(50));
    let first = owner.send_request(Park, deadline).unwrap();
    let second = owner.send_request(Park, deadline).unwrap();
    let _unpolled = owner.send_request(Park, deadline).unwrap();
    let (a, b) = tokio::join!(first, second);
    assert!(matches!(a, Err(ActorError::RequestTimeout)));
    assert!(matches!(b, Err(ActorError::RequestTimeout)));
    tokio::time::sleep(Duration::from_millis(100)).await;
    owner.context().stop();
    owner.join().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let events = write("expired_callers");
    assert_eq!(count(&events, "Expire:wake"), 2, "{events:?}");
    assert_eq!(count(&events, "Expire:none"), 1, "{events:?}");
    assert_eq!(count(&events, "Wake"), 2, "{events:?}");
    assert!(count(&events, "Submit") >= 1, "{events:?}");
}

/// Payloads released on the deadline service are dropped by workers while
/// the owner runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn released_payloads_are_dropped_by_workers() {
    let _serial = SERIAL.lock().await;
    let owner = Owner::default().start();
    let drops = Arc::new(AtomicUsize::new(0));
    lifecycle_trace::record_disposal();
    discard_an_emit(&owner, &drops).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        drops.load(Ordering::SeqCst),
        1,
        "the discarded emit was dropped"
    );
    owner.context().stop();
    owner.join().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let events = write("released_payloads");
    assert!(count(&events, "Release") >= 1, "{events:?}");
    assert_eq!(
        count(&events, "Drop:dropped"),
        count(&events, "Release"),
        "{events:?}"
    );
    assert_eq!(count(&events, "Drop:held"), 0, "{events:?}");
}

/// A payload handed off before the owner's exit, reaching its worker after
/// the exit began (the worker is delayed at `disposal.item`), is held and
/// dropped only at the death point.
#[cfg(feature = "failpoints")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_payload_handed_off_before_exit_waits_for_the_death_point() {
    use spawned_concurrency::failpoints::{self, Action, FailPoint, FailScenario};
    let _serial = SERIAL.lock().await;
    let _scenario = FailScenario::setup();
    let (release, gate) = std::sync::mpsc::channel();
    let (stopping_tx, stopping) = std::sync::mpsc::channel();
    let owner = Owner {
        gate: Some((gate, stopping_tx)),
        ..Owner::default()
    }
    .start();
    let drops = Arc::new(AtomicUsize::new(0));
    failpoints::configure(
        "disposal.item",
        FailPoint::new(Action::Delay(Duration::from_millis(400))),
    )
    .unwrap();
    lifecycle_trace::record_disposal();
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
    tokio::time::sleep(Duration::from_millis(50)).await;
    let events = write("held_until_death");
    assert_eq!(
        before_death, 0,
        "a payload handed off before the exit was dropped before the death point"
    );
    assert_eq!(
        drops.load(Ordering::SeqCst),
        1,
        "dropped at the death point"
    );
    assert!(count(&events, "Drop:held") >= 1, "{events:?}");
    let held = events.iter().position(|e| e == "Drop:held").unwrap();
    let exit = events.iter().position(|e| e == "Exit").unwrap();
    let death = events.iter().position(|e| e == "Death").unwrap();
    assert!(exit < held && held < death, "{events:?}");
}
