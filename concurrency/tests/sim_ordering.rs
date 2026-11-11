//! Ordering guarantees from `docs/design/semantics.md` under adversarial
//! schedules: D1 (per-sender FIFO through any handle), R2 (reply identity),
//! M1/L1 (one turn at a time, hook order), D3/D4/L7 (stop discards the queue,
//! releases every reply sender and drops every payload before the exit is
//! published) and X7 (`Exit`/`Down` follow every message the source sent).
//!
//! Every body is a `#[sim_test]` run twice under the simulator, once with PCT
//! and once with bounded DFS over delays; in a normal build each runs once on
//! Tokio. Bodies use only `spawned_rt` primitives and virtual time, so every
//! schedule replays from its tape:
//!
//! ```sh
//! RUSTFLAGS="--cfg spawned_sim --cfg tokio_unstable" cargo test -p spawned-concurrency --test sim_ordering -- --nocapture
//! ```
#![cfg(feature = "async")]

use spawned_concurrency::{
    message::Message,
    tasks::{Actor, ActorRef, ActorStart, Context, Handler, Recipient},
    ActorError, ChildHandle, Down, Exit, ExitReason,
};
use spawned_rt::tasks::{mpsc, oneshot, spawn, timeout, yield_now};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::time::Duration;
use tokio::sync::oneshot::error::TryRecvError;

const LIMIT: Duration = Duration::from_secs(5);

type Log<T> = Arc<Mutex<Vec<T>>>;

fn log<T>() -> Log<T> {
    Arc::new(Mutex::new(Vec::new()))
}

fn push<T>(log: &Log<T>, event: T) {
    log.lock().unwrap_or_else(|p| p.into_inner()).push(event);
}

fn snapshot<T: Clone>(log: &Log<T>) -> Vec<T> {
    log.lock().unwrap_or_else(|p| p.into_inner()).clone()
}

async fn yields(n: usize) {
    for _ in 0..n {
        yield_now().await;
    }
}

// ---------------------------------------------------------------------------
// D1, D2, R2: per-sender FIFO through every handle kind, reply identity.
// ---------------------------------------------------------------------------

const SENDS: usize = 6;

struct Sink(Log<(usize, usize)>);
impl Actor for Sink {}

struct Put(usize, usize);
impl Message for Put {
    type Result = ();
}
struct Echo(usize, usize);
impl Message for Echo {
    type Result = (usize, usize);
}
impl Handler<Put> for Sink {
    async fn handle(&mut self, Put(sender, seq): Put, _: &Context<Self>) {
        push(&self.0, (sender, seq));
    }
}
impl Handler<Echo> for Sink {
    async fn handle(&mut self, Echo(sender, seq): Echo, _: &Context<Self>) -> (usize, usize) {
        push(&self.0, (sender, seq));
        // Hold the turn across an await so a reply could be misrouted only
        // if replies were not bound to their own request.
        yield_now().await;
        (sender, seq)
    }
}

/// Four senders: an `ActorRef` clone, a `Recipient`, one that alternates
/// between both and requests, and one that only requests. Every message
/// carries `(sender, seq)`; the sink must see each sender's sequence in order
/// and exactly once, and every reply must carry its own request's key.
async fn per_sender_fifo_body() {
    let seen = log();
    let sink = Sink(seen.clone()).start();
    let by_ref = sink.clone();
    let by_recipient: Recipient<Put> = sink.recipient();
    let mixed = (sink.clone(), sink.recipient::<Put>());
    let requester = sink.clone();
    let senders = vec![
        spawn(async move {
            for seq in 0..SENDS {
                by_ref.send(Put(0, seq)).unwrap();
                yield_now().await;
            }
        }),
        spawn(async move {
            for seq in 0..SENDS {
                by_recipient.send(Put(1, seq)).unwrap();
                yield_now().await;
            }
        }),
        spawn(async move {
            let (actor, recipient) = mixed;
            for seq in 0..SENDS {
                match seq % 3 {
                    0 => actor.send(Put(2, seq)).unwrap(),
                    1 => recipient.send(Put(2, seq)).unwrap(),
                    _ => {
                        let reply = actor.request_with_timeout(Echo(2, seq), LIMIT).await;
                        assert!(matches!(reply, Ok((2, s)) if s == seq), "R2: {reply:?}");
                    }
                }
                yield_now().await;
            }
        }),
        spawn(async move {
            for seq in 0..SENDS {
                let reply = requester.request_with_timeout(Echo(3, seq), LIMIT).await;
                assert!(matches!(reply, Ok((3, s)) if s == seq), "R2: {reply:?}");
            }
        }),
    ];
    for sender in senders {
        sender.await.unwrap();
    }
    // Fence: everything sent above was enqueued before this request.
    sink.request_with_timeout(Echo(9, 0), LIMIT).await.unwrap();
    let seen = snapshot(&seen);
    for sender in 0..4 {
        let order: Vec<usize> = seen
            .iter()
            .filter(|(s, _)| *s == sender)
            .map(|(_, seq)| *seq)
            .collect();
        assert_eq!(
            order,
            (0..SENDS).collect::<Vec<_>>(),
            "D1/D2: sender {sender} out of order, lost or duplicated: {seen:?}"
        );
    }
    sink.child_handle().stop();
    sink.join().await;
}

#[spawned_macros::sim_test(explore = "pct:d=3", iterations = 1_000)]
async fn per_sender_fifo_and_reply_identity_pct() {
    per_sender_fifo_body().await;
}

#[spawned_macros::sim_test(explore = "dfs:bound=2", iterations = 2_000)]
async fn per_sender_fifo_and_reply_identity_dfs() {
    per_sender_fifo_body().await;
}

// ---------------------------------------------------------------------------
// M1, L1, L7: one turn at a time, started before every handler, nothing after
// stopped() begins, join only after stopped() ends.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
enum Turn {
    StartedBegin,
    StartedEnd,
    Begin(usize),
    End(usize),
    StoppedBegin,
    StoppedEnd,
}

struct Turns(Log<Turn>);
impl Actor for Turns {
    async fn started(&mut self, _: &Context<Self>) {
        push(&self.0, Turn::StartedBegin);
        yields(3).await;
        push(&self.0, Turn::StartedEnd);
    }
    async fn stopped(&mut self, _: &Context<Self>) {
        push(&self.0, Turn::StoppedBegin);
        yields(3).await;
        push(&self.0, Turn::StoppedEnd);
    }
}

struct Work(usize);
impl Message for Work {
    type Result = usize;
}
impl Handler<Work> for Turns {
    async fn handle(&mut self, Work(id): Work, _: &Context<Self>) -> usize {
        push(&self.0, Turn::Begin(id));
        yields(2).await;
        push(&self.0, Turn::End(id));
        id
    }
}

/// Senders start before `started()` has finished and keep going while
/// another task stops the actor, so handlers race both hooks.
async fn turns_body() {
    let events = log();
    let actor = Turns(events.clone()).start();
    let mut tasks = Vec::new();
    for sender in 0..2 {
        let actor = actor.clone();
        tasks.push(spawn(async move {
            let mut outcomes = Vec::new();
            for k in 0..4 {
                let id = sender * 10 + k;
                if k % 2 == 0 {
                    outcomes.push((id, actor.send(Work(id)).map(|_| None)));
                } else {
                    let reply = actor.request_with_timeout(Work(id), LIMIT).await;
                    outcomes.push((id, reply.map(Some)));
                }
                yield_now().await;
            }
            outcomes
        }));
    }
    let stopper = {
        let handle = actor.child_handle();
        spawn(async move {
            yields(4).await;
            handle.stop();
        })
    };
    let mut outcomes = Vec::new();
    for task in tasks {
        outcomes.extend(task.await.unwrap());
    }
    stopper.await.unwrap();
    actor.join().await;

    // L7: join() returned, so stopped() has completed and new work is refused.
    let events = snapshot(&events);
    assert_eq!(events.last(), Some(&Turn::StoppedEnd), "L7: {events:?}");
    assert!(matches!(actor.send(Work(99)), Err(ActorError::ActorStopped)));
    assert!(matches!(
        actor.request_with_timeout(Work(99), LIMIT).await,
        Err(ActorError::ActorStopped)
    ));

    // L1/M1: started, then whole non-overlapping turns, then stopped.
    assert_eq!(events[..2], [Turn::StartedBegin, Turn::StartedEnd], "L1: {events:?}");
    let body = &events[2..events.len() - 2];
    assert_eq!(
        events[events.len() - 2..],
        [Turn::StoppedBegin, Turn::StoppedEnd],
        "L1: {events:?}"
    );
    assert_eq!(body.len() % 2, 0, "M1: a turn was split: {events:?}");
    let mut handled = Vec::new();
    for pair in body.chunks(2) {
        match pair {
            [Turn::Begin(a), Turn::End(b)] if a == b => handled.push(*a),
            _ => panic!("M1/L1: interleaved or misplaced turn {pair:?} in {events:?}"),
        }
    }
    // D2: at most once; R1: an Ok reply means its handler ran.
    let mut unique = handled.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), handled.len(), "D2: duplicated: {events:?}");
    for (id, outcome) in outcomes {
        match outcome {
            Ok(Some(reply)) => {
                assert_eq!(reply, id);
                assert!(handled.contains(&id), "R1: Ok without a turn for {id}");
            }
            Ok(None) => {}
            Err(ActorError::ActorStopped) => {
                assert!(!handled.contains(&id), "R1: ran but caller got ActorStopped");
            }
            Err(other) => panic!("R1: unexpected outcome {other:?} for {id}"),
        }
    }
}

#[spawned_macros::sim_test(explore = "pct:d=3", iterations = 1_000)]
async fn turns_and_hooks_never_overlap_pct() {
    turns_body().await;
}

#[spawned_macros::sim_test(explore = "dfs:bound=2", iterations = 2_000)]
async fn turns_and_hooks_never_overlap_dfs() {
    turns_body().await;
}

// ---------------------------------------------------------------------------
// D3, D4, L7: a stop while a handler runs discards everything queued, even
// messages sent after the stop request; join() returns only after every
// queued payload was dropped and every queued reply sender released.
// ---------------------------------------------------------------------------

/// Counts its own destruction.
struct Payload(Arc<AtomicUsize>);
impl Drop for Payload {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

struct Queued(#[allow(dead_code)] Payload);
impl Message for Queued {
    type Result = ();
}
struct Asked(#[allow(dead_code)] Payload);
impl Message for Asked {
    type Result = u64;
}
struct Hold {
    entered: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
}
impl Message for Hold {
    type Result = ();
}

#[derive(Debug, Clone, PartialEq)]
enum Seen {
    Hold,
    Queued,
    Asked,
    Stopped,
}

struct Holder(Log<Seen>);
impl Actor for Holder {
    async fn stopped(&mut self, _: &Context<Self>) {
        push(&self.0, Seen::Stopped);
    }
}
impl Handler<Hold> for Holder {
    async fn handle(&mut self, hold: Hold, _: &Context<Self>) {
        push(&self.0, Seen::Hold);
        let _ = hold.entered.send(());
        let _ = hold.release.await;
    }
}
impl Handler<Queued> for Holder {
    async fn handle(&mut self, _: Queued, _: &Context<Self>) {
        push(&self.0, Seen::Queued);
    }
}
impl Handler<Asked> for Holder {
    async fn handle(&mut self, _: Asked, _: &Context<Self>) -> u64 {
        push(&self.0, Seen::Asked);
        1
    }
}

const QUEUERS: usize = 3;
const PER_QUEUER: usize = 4;

async fn stop_discards_body() {
    let seen = log();
    let drops = Arc::new(AtomicUsize::new(0));
    let actor = Holder(seen.clone()).start();
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    actor
        .send(Hold {
            entered: entered_tx,
            release: release_rx,
        })
        .unwrap();
    entered_rx.await.unwrap();

    // Queue sends and requests from several tasks while another task stops
    // the actor; the handler is still running, so the mailbox stays open.
    let mut queuers = Vec::new();
    for _ in 0..QUEUERS {
        let actor = actor.clone();
        let drops = drops.clone();
        queuers.push(spawn(async move {
            let mut replies = Vec::new();
            for k in 0..PER_QUEUER {
                if k % 2 == 0 {
                    // D3: accepted even after the stop request.
                    actor.send(Queued(Payload(drops.clone()))).unwrap();
                } else {
                    replies.push(actor.request_raw(Asked(Payload(drops.clone()))).unwrap());
                }
                yield_now().await;
            }
            replies
        }));
    }
    let stopper = {
        let handle = actor.child_handle();
        spawn(async move {
            yields(3).await;
            handle.stop();
        })
    };
    let mut replies = Vec::new();
    for queuer in queuers {
        replies.extend(queuer.await.unwrap());
    }
    stopper.await.unwrap();
    // The stop was requested while Hold was running: nothing queued may run.
    release_tx.send(()).unwrap();
    actor.join().await;

    // L7: by the time join() returns every queued payload has been dropped
    // and every queued reply sender released.
    assert_eq!(
        drops.load(Ordering::SeqCst),
        QUEUERS * PER_QUEUER,
        "D4/L7: queued payloads not all dropped before the exit was published"
    );
    for mut reply in replies {
        assert!(
            matches!(reply.try_recv(), Err(TryRecvError::Closed)),
            "D4/L7: a queued reply sender outlived the exit"
        );
    }
    assert_eq!(
        snapshot(&seen),
        vec![Seen::Hold, Seen::Stopped],
        "D4: a queued message was dispatched after the stop"
    );
    assert_eq!(actor.wait_exit().await, ExitReason::Normal);
    assert!(
        matches!(
            actor.send(Queued(Payload(drops.clone()))),
            Err(ActorError::ActorStopped)
        ),
        "L7: send after exit"
    );
}

#[spawned_macros::sim_test(explore = "pct:d=3", iterations = 1_000)]
async fn stop_discards_queue_before_publishing_exit_pct() {
    stop_discards_body().await;
}

#[spawned_macros::sim_test(explore = "dfs:bound=2", iterations = 2_000)]
async fn stop_discards_queue_before_publishing_exit_dfs() {
    stop_discards_body().await;
}

// ---------------------------------------------------------------------------
// X7: trapped Exit and Down follow every message the source sent earlier,
// even when the stop races the source's sends and noise floods the observer.
// ---------------------------------------------------------------------------

const PINGS: usize = 5;

#[derive(Debug, Clone, PartialEq)]
enum Notice {
    Ping(usize),
    Noise,
    Exit(ExitReason),
    Down(ExitReason),
}

/// Logs every notice; signals `notify` on `Exit` and `Down`.
struct Observer(Log<Notice>, mpsc::Sender<()>);
impl Actor for Observer {
    async fn exit_received(&mut self, exit: Exit, _: &Context<Self>) {
        push(&self.0, Notice::Exit(exit.reason));
        let _ = self.1.send(());
    }
}
struct Watch(ChildHandle);
impl Message for Watch {
    type Result = ();
}
impl Handler<Watch> for Observer {
    async fn handle(&mut self, Watch(target): Watch, ctx: &Context<Self>) {
        ctx.trap_exit(true);
        ctx.link(&target);
        ctx.monitor(&target);
    }
}
struct Ping(usize);
impl Message for Ping {
    type Result = ();
}
impl Handler<Ping> for Observer {
    async fn handle(&mut self, Ping(i): Ping, _: &Context<Self>) {
        push(&self.0, Notice::Ping(i));
    }
}
struct Noise;
impl Message for Noise {
    type Result = ();
}
impl Handler<Noise> for Observer {
    async fn handle(&mut self, _: Noise, _: &Context<Self>) {
        push(&self.0, Notice::Noise);
    }
}
impl Handler<Down> for Observer {
    async fn handle(&mut self, down: Down, _: &Context<Self>) {
        push(&self.0, Notice::Down(down.reason));
        let _ = self.1.send(());
    }
}

#[derive(Clone, Copy, Debug)]
enum End {
    /// The source stops itself after its last ping.
    SelfStop,
    /// The source panics after its last ping.
    Panic,
    /// Another task stops the source while it is still sending.
    External,
}

struct Source;
impl Actor for Source {}
struct Burst {
    to: Recipient<Ping>,
    end: End,
    entered: oneshot::Sender<()>,
}
impl Message for Burst {
    type Result = ();
}
impl Handler<Burst> for Source {
    async fn handle(&mut self, burst: Burst, ctx: &Context<Self>) {
        let _ = burst.entered.send(());
        for i in 0..PINGS {
            burst.to.send(Ping(i)).unwrap();
            yield_now().await;
        }
        match burst.end {
            End::SelfStop => ctx.stop_with_reason(ExitReason::Shutdown),
            End::Panic => panic!("source panicked"),
            End::External => {}
        }
    }
}

async fn exit_after_last_message_body(end: End) {
    let notices = log();
    let (notify, mut notified) = mpsc::channel();
    let observer: ActorRef<Observer> = Observer(notices.clone(), notify).start();
    let source = Source.start();
    observer
        .request_with_timeout(Watch(source.child_handle()), LIMIT)
        .await
        .unwrap();
    let (entered_tx, entered_rx) = oneshot::channel();
    source
        .send(Burst {
            to: observer.recipient(),
            end,
            entered: entered_tx,
        })
        .unwrap();
    let noise = {
        let observer = observer.clone();
        spawn(async move {
            for _ in 0..3 {
                let _ = observer.send(Noise);
                yield_now().await;
            }
        })
    };
    if let End::External = end {
        // Stop once the burst is running (a stop before it is dequeued would
        // discard it, D4), racing the remaining sends.
        entered_rx.await.unwrap();
        yield_now().await;
        source.child_handle().stop();
    }
    noise.await.unwrap();
    let reason = source.wait_exit().await;
    for _ in 0..2 {
        timeout(LIMIT, notified.recv())
            .await
            .expect("Exit and Down at the observer")
            .unwrap();
    }
    let notices = snapshot(&notices);
    let pings: Vec<usize> = notices
        .iter()
        .filter_map(|e| match e {
            Notice::Ping(i) => Some(*i),
            _ => None,
        })
        .collect();
    assert_eq!(pings, (0..PINGS).collect::<Vec<_>>(), "D1: {notices:?}");
    let last_ping = notices
        .iter()
        .rposition(|e| matches!(e, Notice::Ping(_)))
        .unwrap();
    let exit = notices
        .iter()
        .position(|e| matches!(e, Notice::Exit(_)))
        .unwrap();
    let down = notices
        .iter()
        .position(|e| matches!(e, Notice::Down(_)))
        .unwrap();
    assert!(exit > last_ping, "X7: Exit overtook a ping: {notices:?}");
    assert!(down > last_ping, "X7: Down overtook a ping: {notices:?}");
    assert_eq!(notices[exit], Notice::Exit(reason.clone()), "{notices:?}");
    assert_eq!(notices[down], Notice::Down(reason.clone()), "{notices:?}");
    match end {
        End::SelfStop => assert_eq!(reason, ExitReason::Shutdown),
        End::Panic => assert!(matches!(reason, ExitReason::Panic(_)), "{reason:?}"),
        End::External => assert_eq!(reason, ExitReason::Normal),
    }
    observer.child_handle().stop();
    observer.join().await;
}

async fn exit_after_last_message_all() {
    for end in [End::SelfStop, End::Panic, End::External] {
        exit_after_last_message_body(end).await;
    }
}

#[spawned_macros::sim_test(explore = "pct:d=3", iterations = 1_000)]
async fn exit_and_down_follow_the_last_message_pct() {
    exit_after_last_message_all().await;
}

#[spawned_macros::sim_test(explore = "dfs:bound=2", iterations = 2_000)]
async fn exit_and_down_follow_the_last_message_dfs() {
    exit_after_last_message_all().await;
}
