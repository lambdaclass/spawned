//! Test kit for `threads` (blocking) actors: [`Mock`], [`Probe`], harness
//! helpers. Same API as [`crate::tasks`] without `async`; no Tokio needed.
use crate::inbox::{Inbox, Received};
use crate::probe::ProbeCore;
use crate::rules::{Fence, Plan, Rules};
use crate::shared::SETTLE_TIMEOUT;
use spawned_concurrency::error::ActorError;
use spawned_concurrency::message::Message;
use spawned_concurrency::reply::{Deadline, PendingReply, ReplyReceiver};
use spawned_concurrency::threads::{
    Actor, ActorRef, ActorStart, Context, Handler, Receiver, Recipient,
};
use spawned_concurrency::{ChildHandle, ExitReason};
use std::ops::Deref;
use std::panic::Location;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A closure-backed native actor that handles **every** message type; see
/// [`tasks::Mock`](crate::tasks::Mock) for the contract. `ActorRef<Mock>`
/// implements every `#[protocol]` through its `threads` blanket impl.
pub struct Mock {
    rules: Rules,
    inbox: Arc<Inbox>,
}

impl Default for Mock {
    fn default() -> Self {
        Self::new()
    }
}

impl Mock {
    mock_builder!();

    /// Fault: block the mock's thread for `delay` before replying to each `M`.
    pub fn delay<M: Message>(mut self, delay: Duration) -> Self {
        self.rules.delay::<M>(delay);
        self
    }

    /// Start on a dedicated OS thread.
    pub fn start(self) -> MockRef {
        let inbox = self.inbox.clone();
        MockRef {
            actor: ActorStart::start(self),
            inbox,
        }
    }
}

impl Actor for Mock {}

impl<M: Message> Handler<M> for Mock {
    fn handle(&mut self, msg: M, reply: PendingReply<'_, M::Result>, _: &Context<Self>) {
        match self.rules.plan(&self.inbox, msg) {
            Plan::Reply {
                delay,
                value,
                record,
            } => {
                if let Some(delay) = delay {
                    std::thread::sleep(delay);
                }
                if let Some(record) = record {
                    record.finish();
                }
                reply.reply(value);
            }
            Plan::Discard { delay } => {
                if let Some(delay) = delay {
                    std::thread::sleep(delay);
                }
                drop(reply);
            }
            Plan::Defer => {
                if let Ok(token) = reply.defer() {
                    self.inbox.store_deferred::<M>(token);
                }
            }
            Plan::Crash(reason) => panic!("{reason}"),
        }
    }
}

/// A running native [`Mock`]; dereferences to `ActorRef<Mock>`.
#[derive(Clone)]
pub struct MockRef {
    actor: ActorRef<Mock>,
    inbox: Arc<Inbox>,
}

mock_ref_common!();

impl MockRef {
    /// Block until the oldest not-yet-expected `M` arrives and return it.
    #[track_caller]
    pub fn expect<M: Message>(&self, timeout: Duration) -> M {
        self.inbox
            .expect_blocking::<M>(timeout, &self.who(), Location::caller())
    }

    /// Panic if any not-yet-expected message is (or arrives) within `window`.
    #[track_caller]
    pub fn expect_no_message(&self, window: Duration) {
        self.inbox
            .expect_none_blocking(window, &self.who(), Location::caller())
    }

    /// Block until the mock has handled everything this thread sent before.
    /// Panics if the mock has stopped or does not answer within 5 s; see
    /// [`settle_within`](MockRef::settle_within).
    #[track_caller]
    pub fn settle(&self) {
        self.settle_within(SETTLE_TIMEOUT)
    }

    /// [`settle`](MockRef::settle) with a chosen bound.
    #[track_caller]
    pub fn settle_within(&self, timeout: Duration) {
        if let Err(error) = self.actor.request_with_timeout(Fence, timeout) {
            self.settle_failed(error, Location::caller());
        }
    }
}

/// A native [`Recipient<M>`] that records what it is sent; see
/// [`tasks::Probe`](crate::tasks::Probe).
pub struct Probe<M: Message> {
    core: Arc<ProbeCore<M>>,
}

probe_common!();

impl<M: Message> Probe<M> {
    /// Block until the oldest not-yet-expected message arrives.
    #[track_caller]
    pub fn expect(&self, timeout: Duration) -> M {
        self.core
            .inbox
            .expect_blocking::<M>(timeout, "Probe", Location::caller())
    }

    /// Panic if any not-yet-expected message is (or arrives) within `window`.
    #[track_caller]
    pub fn expect_no_message(&self, window: Duration) {
        self.core
            .inbox
            .expect_none_blocking(window, "Probe", Location::caller())
    }
}

/// Request `fence` and return its reply: everything this thread sent to
/// `actor` earlier has been handled. Panics on error.
#[track_caller]
pub fn settle<A, M>(actor: &ActorRef<A>, fence: M, timeout: Duration) -> M::Result
where
    A: Handler<M>,
    M: Message,
{
    match actor.request_with_timeout(fence, timeout) {
        Ok(reply) => reply,
        Err(error) => panic!(
            "actor {:?} did not settle on `{}` within {timeout:?}: {error}; exit reason {:?}",
            actor.id(),
            std::any::type_name::<M>(),
            actor.exit_reason()
        ),
    }
}

/// Run `f` inside one of `actor`'s turns, with its state and `Context`, and
/// return its result: a test reaches `ctx` (to start a job, defer a reply,
/// arm a timer) or inspects state without an ad hoc message or a
/// `cfg(test)` bypass. Messages sent to `actor` earlier are handled first.
/// Panics if the actor is gone or `f` panicked (which crashes the actor, as
/// any handler panic does).
#[track_caller]
pub fn in_actor<A, R, F>(actor: &ActorRef<A>, f: F) -> R
where
    A: Actor,
    R: Send + 'static,
    F: FnOnce(&mut A, &Context<A>) -> R + Send + 'static,
{
    let reply = actor.__in_turn(f).and_then(|rx| rx.recv());
    reply.unwrap_or_else(|error| {
        panic!(
            "in_actor on {:?} failed: {error}; exit reason {:?}",
            actor.id(),
            actor.exit_reason()
        )
    })
}

/// Block until `handle` exits, or panic after `deadline` instead of hanging.
///
/// Polls every millisecond: `ChildHandle` has no blocking wait with a
/// timeout, and a waiter thread per call would leak on timeout.
#[track_caller]
pub fn exit_within(handle: &ChildHandle, deadline: Duration) -> ExitReason {
    let end = Instant::now() + deadline;
    loop {
        if let Some(reason) = handle.exit_reason() {
            return reason;
        }
        if Instant::now() >= end {
            panic!(
                "actor {:?} did not exit within {deadline:?}; a handler may be blocked or the \
                 actor was never asked to stop",
                handle.id()
            );
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// [`exit_within`] for a native `ActorRef`.
#[track_caller]
pub fn join_within<A: Actor>(actor: &ActorRef<A>, deadline: Duration) -> ExitReason {
    exit_within(&actor.child_handle(), deadline)
}

/// Poll `condition` every millisecond until it holds; panic naming `what` after `timeout`.
#[track_caller]
pub fn wait_until(timeout: Duration, what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !condition() {
        if Instant::now() >= deadline {
            panic!("timed out after {timeout:?} waiting until {what}");
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}
