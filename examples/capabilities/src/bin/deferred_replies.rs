//! Deferred replies and pipe-to-self (rank 11), asserted:
//!
//! 1. a router delegates a slow request with `pipe_reply` and keeps serving
//!    its mailbox meanwhile;
//! 2. CPU work offloaded with `pipe_blocking_with` and a job deadline;
//! 3. the ethrex `outgoing_request` pattern: request ids in a bounded
//!    `PendingReplies`, answered when the "peer" responds, late responses
//!    ignored;
//! 4. the `get_best_peer` waiter queue: callers wait (no polling) until
//!    capacity frees, and `Overloaded` at the bound;
//! 5. owner stop with stored tokens: `stopped()` answers, death closes the
//!    rest with `ActorStopped`.
use spawned_concurrency::message::Message;
use spawned_concurrency::tasks::{
    Actor, ActorRef, ActorStart, Context, Deadline, Handler, InsertError, JobError, JobOptions,
    JobOutcome, PendingReplies, PendingReply, ReplyTo,
};
use spawned_concurrency::{actor, protocol, ActorError, DeferError, Response};
use std::collections::VecDeque;
use std::time::Duration;

// ---------------------------------------------------------------------------
// 1. Router delegating with pipe_reply
// ---------------------------------------------------------------------------

#[protocol]
pub trait BackendProtocol: Send + Sync {
    fn lookup(&self, millis: u64) -> Response<u64>;
}

pub struct Backend;
#[actor(protocol = BackendProtocol)]
impl Backend {
    #[request_handler]
    async fn lookup(&mut self, msg: backend_protocol::Lookup, _: &Context<Self>) -> u64 {
        spawned_rt::tasks::sleep(Duration::from_millis(msg.millis)).await;
        msg.millis
    }
}

#[protocol]
pub trait RouterProtocol: Send + Sync {
    fn route(&self, millis: u64) -> Response<u64>;
    fn ping(&self) -> Response<&'static str>;
}

pub struct Router {
    backend: ActorRef<Backend>,
}
struct Delegated {
    res: JobOutcome<Result<u64, ActorError>>,
    token: ReplyTo<u64>,
}
impl Message for Delegated {
    type Result = ();
}

#[actor(protocol = RouterProtocol)]
impl Router {
    #[deferred_handler]
    async fn route(
        &mut self,
        msg: router_protocol::Route,
        reply: PendingReply<'_, u64>,
        ctx: &Context<Self>,
    ) {
        let Ok(token) = reply.defer() else { return };
        // The downstream request inherits the caller's deadline.
        let deadline = token.deadline().map_or(Deadline::Never, Deadline::At);
        let downstream = self
            .backend
            .request_raw_until(backend_protocol::Lookup { millis: msg.millis }, deadline);
        let work = async move { downstream?.await };
        if let Err(e) = ctx.pipe_reply(token, work, |res, token| Delegated { res, token }) {
            e.into_token().overloaded();
        }
    }

    #[request_handler]
    async fn ping(&mut self, _: router_protocol::Ping, _: &Context<Self>) -> &'static str {
        "pong"
    }

    #[send_handler]
    async fn delegated(&mut self, msg: Delegated, _: &Context<Self>) {
        match msg.res {
            JobOutcome::Ok(Ok(v)) => {
                msg.token.reply(v);
            }
            _ => msg.token.overloaded(),
        }
    }
}

// ---------------------------------------------------------------------------
// 2. CPU offload with a job deadline
// ---------------------------------------------------------------------------

pub struct Verifier {
    verdicts: Vec<String>,
}
struct Verify {
    id: u32,
    spin: Duration,
}
impl Message for Verify {
    type Result = ();
}
struct Verdict {
    id: u32,
    res: JobOutcome<bool>,
}
impl Message for Verdict {
    type Result = ();
}
struct Verdicts;
impl Message for Verdicts {
    type Result = Vec<String>;
}

#[actor]
impl Verifier {
    #[send_handler]
    async fn verify(&mut self, msg: Verify, ctx: &Context<Self>) {
        let opts = JobOptions {
            deadline: Some(spawned_rt::time::Instant::now() + Duration::from_millis(100)),
            ..JobOptions::default()
        };
        let (id, spin) = (msg.id, msg.spin);
        let started = ctx.pipe_blocking_with(
            opts,
            move |_job| {
                std::thread::sleep(spin); // stands in for a CPU-bound check
                true
            },
            move |res| Verdict { id, res },
        );
        if let Err(e) = started {
            self.verdicts.push(format!("{id}: rejected ({e})"));
        }
    }

    #[send_handler]
    async fn verdict(&mut self, msg: Verdict, _: &Context<Self>) {
        self.verdicts.push(match msg.res {
            JobOutcome::Ok(valid) => format!("{}: valid={valid}", msg.id),
            JobOutcome::Err(JobError::DeadlineExceeded) => format!("{}: timed out", msg.id),
            JobOutcome::Err(JobError::Panicked(m)) => format!("{}: panicked {m}", msg.id),
        });
    }

    #[request_handler]
    async fn verdicts(&mut self, _: Verdicts, _: &Context<Self>) -> Vec<String> {
        std::mem::take(&mut self.verdicts)
    }
}

// ---------------------------------------------------------------------------
// 3. Outgoing requests by id (ethrex `outgoing_request`)
// ---------------------------------------------------------------------------

pub struct Connection {
    current_requests: PendingReplies<u64, String>,
}
struct Outgoing {
    id: u64,
}
impl Message for Outgoing {
    type Result = String;
}
struct PeerResponse {
    id: u64,
    body: String,
}
impl Message for PeerResponse {
    type Result = bool;
}

#[actor]
impl Connection {
    #[deferred_handler]
    async fn outgoing(
        &mut self,
        msg: Outgoing,
        reply: PendingReply<'_, String>,
        _: &Context<Self>,
    ) {
        let Ok(token) = reply.defer() else { return };
        match self.current_requests.insert(msg.id, token) {
            Ok(()) => {} // the request goes out on the socket here
            Err(InsertError::Duplicate(token)) => drop(token), // ReplyDropped; the first caller is kept
            Err(InsertError::Full(token)) => token.overloaded(),
        }
    }

    /// Returns whether the response matched a pending request.
    #[request_handler]
    async fn peer_response(&mut self, msg: PeerResponse, _: &Context<Self>) -> bool {
        match self.current_requests.take(&msg.id) {
            Some(token) => {
                token.reply(msg.body);
                true
            }
            None => false, // late or unknown: expired, abandoned or purged
        }
    }
}

// ---------------------------------------------------------------------------
// 4. Waiting for a free peer (ethrex `get_best_peer`)
// ---------------------------------------------------------------------------

const MAX_WAITERS: usize = 2;

pub struct PeerTable {
    free: u32,
    waiters: VecDeque<ReplyTo<u32>>,
}
struct WaitForPeer;
impl Message for WaitForPeer {
    type Result = u32;
}
struct Release(u32);
impl Message for Release {
    type Result = ();
}

#[actor]
impl PeerTable {
    #[deferred_handler]
    async fn wait_for_peer(
        &mut self,
        _: WaitForPeer,
        reply: PendingReply<'_, u32>,
        _: &Context<Self>,
    ) {
        if self.free > 0 {
            self.free -= 1;
            reply.reply(self.free);
            return;
        }
        self.waiters.retain(|t| !t.is_closed());
        if self.waiters.len() >= MAX_WAITERS {
            return reply.overloaded();
        }
        match reply.defer() {
            Ok(token) => self.waiters.push_back(token),
            Err(DeferError::Overloaded | DeferError::Closed | DeferError::NoCaller) => {}
        }
    }

    #[send_handler]
    async fn release(&mut self, msg: Release, _: &Context<Self>) {
        self.free += msg.0;
        while self.free > 0 {
            let Some(token) = self.waiters.pop_front() else {
                break;
            };
            if token.is_closed() {
                continue;
            }
            self.free -= 1;
            token.reply(self.free);
        }
    }
}

// ---------------------------------------------------------------------------
// 5. Owner stop with stored tokens
// ---------------------------------------------------------------------------

pub struct Parker {
    parked: Vec<ReplyTo<&'static str>>,
    answer_first: usize,
}
struct Park;
impl Message for Park {
    type Result = &'static str;
}
struct StopAnswering(usize);
impl Message for StopAnswering {
    type Result = ();
}

#[actor]
impl Parker {
    #[deferred_handler]
    async fn park(&mut self, _: Park, reply: PendingReply<'_, &'static str>, _: &Context<Self>) {
        if let Ok(token) = reply.defer() {
            self.parked.push(token);
        }
    }

    /// Stop; `stopped()` answers the first `n` parked callers.
    #[request_handler]
    async fn stop_answering(&mut self, msg: StopAnswering, ctx: &Context<Self>) {
        self.answer_first = msg.0;
        ctx.stop();
    }

    #[stopped]
    async fn on_stop(&mut self, _: &Context<Self>) {
        let n = self.answer_first.min(self.parked.len());
        for token in self.parked.drain(..n) {
            token.reply("answered in stopped()");
        }
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    // 1. Delegation: the slow request does not block the router.
    let backend = Backend.start();
    let router = Router {
        backend: backend.clone(),
    }
    .start();
    let slow = router.route(300);
    assert_eq!(router.ping().await.unwrap(), "pong");
    assert_eq!(router.route(1).await.unwrap(), 1);
    assert_eq!(slow.await.unwrap(), 300);
    println!("1. router served other requests while a delegated one was pending");

    // 2. CPU offload: a fast check, a check past its job deadline.
    let verifier = Verifier {
        verdicts: Vec::new(),
    }
    .start();
    verifier
        .send(Verify {
            id: 1,
            spin: Duration::from_millis(1),
        })
        .unwrap();
    verifier
        .send(Verify {
            id: 2,
            spin: Duration::from_millis(400),
        })
        .unwrap();
    let mut verdicts = Vec::new();
    while verdicts.len() < 2 {
        verdicts.extend(verifier.request(Verdicts).await.unwrap());
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    verdicts.sort();
    assert_eq!(verdicts, ["1: valid=true", "2: timed out"]);
    println!("2. {verdicts:?} (the hung check's thread keeps its slot until it returns)");

    // 3. Outgoing requests by id.
    let conn = Connection {
        current_requests: PendingReplies::with_capacity(2),
    }
    .start();
    let a = conn.request_raw(Outgoing { id: 1 }).unwrap();
    let dup = conn.request_raw(Outgoing { id: 1 }).unwrap();
    let b = conn.request_raw(Outgoing { id: 2 }).unwrap();
    let full = conn.request_raw(Outgoing { id: 3 }).unwrap();
    assert!(matches!(dup.await, Err(ActorError::ReplyDropped)));
    assert!(matches!(full.await, Err(ActorError::Overloaded)));
    drop(b); // the caller gives up: its entry is purged on the next insert
    assert!(conn
        .request(PeerResponse {
            id: 1,
            body: "block".into()
        })
        .await
        .unwrap());
    assert_eq!(a.await.unwrap(), "block");
    assert!(!conn
        .request(PeerResponse {
            id: 2,
            body: "late".into()
        })
        .await
        .unwrap());
    println!("3. responses matched by id; duplicate, full and late responses handled");

    // 4. Waiting for a free peer instead of polling.
    let table = PeerTable {
        free: 0,
        waiters: VecDeque::new(),
    }
    .start();
    let first = table.request_raw(WaitForPeer).unwrap();
    let second = table.request_raw(WaitForPeer).unwrap();
    assert!(matches!(
        table.request(WaitForPeer).await,
        Err(ActorError::Overloaded)
    ));
    table.send(Release(2)).unwrap();
    assert_eq!((first.await.unwrap(), second.await.unwrap()), (1, 0));
    println!("4. waiters answered when capacity freed; Overloaded at the queue bound");

    // 5. Owner stop: stopped() answers one caller, death answers the other.
    let parker = Parker {
        parked: Vec::new(),
        answer_first: 0,
    }
    .start();
    let one = parker.request_raw_until(Park, Deadline::Never).unwrap();
    let two = parker.request_raw_until(Park, Deadline::Never).unwrap();
    parker.request(StopAnswering(1)).await.unwrap();
    assert_eq!(one.await.unwrap(), "answered in stopped()");
    assert!(matches!(two.await, Err(ActorError::ActorStopped)));
    parker.join().await;
    println!("5. stopped() answered a stored token; death answered the rest ActorStopped");

    for actor in [
        router.child_handle(),
        backend.child_handle(),
        verifier.child_handle(),
        conn.child_handle(),
        table.child_handle(),
    ] {
        actor.stop();
    }
}
