//! Test kit for `threads` (blocking) actors: [`Mock`], [`Probe`], harness
//! helpers. Same API as [`crate::tasks`] without `async`; no Tokio needed.
use crate::inbox::{Inbox, Received};
use crate::probe::ProbeCore;
use crate::rules::{Fence, Plan, Rules};
use crate::shared::SETTLE_TIMEOUT;
use spawned_concurrency::error::ActorError;
use spawned_concurrency::message::Message;
use spawned_concurrency::threads::{
    Actor, ActorRef, ActorStart, Context, Handler, Receiver, Recipient,
};
use spawned_concurrency::{ChildHandle, ExitReason};
use spawned_rt::threads::oneshot;
use std::ops::Deref;
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
    fn handle(&mut self, msg: M, _: &Context<Self>) -> M::Result {
        match self.rules.plan(&self.inbox, msg) {
            Plan::Reply { delay, value } => {
                if let Some(delay) = delay {
                    std::thread::sleep(delay);
                }
                value
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
    pub fn expect<M: Message>(&self, timeout: Duration) -> M {
        self.inbox.expect_blocking::<M>(timeout, &self.who())
    }

    /// Panic if any not-yet-expected message is (or arrives) within `window`.
    pub fn expect_no_message(&self, window: Duration) {
        self.inbox.expect_none_blocking(window, &self.who())
    }

    /// Block until the mock has handled everything this thread sent before.
    /// Panics if the mock has stopped or does not answer within 5 s; see
    /// [`settle_within`](MockRef::settle_within).
    pub fn settle(&self) {
        self.settle_within(SETTLE_TIMEOUT)
    }

    /// [`settle`](MockRef::settle) with a chosen bound.
    pub fn settle_within(&self, timeout: Duration) {
        if let Err(error) = self.actor.request_with_timeout(Fence, timeout) {
            self.settle_failed(error);
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
    pub fn expect(&self, timeout: Duration) -> M {
        self.core.inbox.expect_blocking::<M>(timeout, "Probe")
    }

    /// Panic if any not-yet-expected message is (or arrives) within `window`.
    pub fn expect_no_message(&self, window: Duration) {
        self.core.inbox.expect_none_blocking(window, "Probe")
    }
}

/// Request `fence` and return its reply: everything this thread sent to
/// `actor` earlier has been handled. Panics on error.
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

/// Block until `handle` exits, or panic after `deadline` instead of hanging.
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
pub fn join_within<A: Actor>(actor: &ActorRef<A>, deadline: Duration) -> ExitReason {
    exit_within(&actor.child_handle(), deadline)
}

/// Poll `condition` every millisecond until it holds; panic naming `what` after `timeout`.
pub fn wait_until(timeout: Duration, what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !condition() {
        if Instant::now() >= deadline {
            panic!("timed out after {timeout:?} waiting until {what}");
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}
