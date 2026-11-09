//! Test kit for `threads` (blocking) actors: [`Mock`], [`Probe`], harness
//! helpers. Same API as [`crate::tasks`] without `async`; no Tokio needed.
use crate::inbox::{Inbox, Received};
use crate::probe::ProbeCore;
use crate::rules::{Fence, Plan, Rules};
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
    /// A strict mock: every message type needs a rule.
    pub fn new() -> Self {
        Self {
            rules: Rules::default(),
            inbox: Arc::new(Inbox::default()),
        }
    }

    /// Answer each `M` with `reply(&msg)`; the message is still logged.
    pub fn on<M: Message>(mut self, reply: impl FnMut(&M) -> M::Result + Send + 'static) -> Self {
        self.rules.on::<M>(reply);
        self
    }

    /// Acknowledge and log every message whose reply type is `()`.
    pub fn allow_sends(mut self) -> Self {
        self.rules.allow_sends();
        self
    }

    /// Fault: block the mock's thread for `delay` before replying to each `M`.
    pub fn delay<M: Message>(mut self, delay: Duration) -> Self {
        self.rules.delay::<M>(delay);
        self
    }

    /// Fault: log each `M` as discarded and do nothing else.
    pub fn discard<M: Message<Result = ()>>(mut self) -> Self {
        self.rules.discard::<M>();
        self
    }

    /// Fault: panic with `reason` on each `M`.
    pub fn crash_on<M: Message>(mut self, reason: impl Into<String>) -> Self {
        self.rules.crash_on::<M>(reason.into());
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

impl Deref for MockRef {
    type Target = ActorRef<Mock>;
    fn deref(&self) -> &ActorRef<Mock> {
        &self.actor
    }
}

impl MockRef {
    fn who(&self) -> String {
        format!("Mock {:?}", self.actor.id())
    }

    /// The underlying actor reference.
    pub fn actor_ref(&self) -> ActorRef<Mock> {
        self.actor.clone()
    }

    /// Block until the oldest not-yet-expected `M` arrives and return it.
    pub fn expect<M: Message>(&self, timeout: Duration) -> M {
        self.inbox.expect_blocking::<M>(timeout, &self.who())
    }

    /// Panic if any not-yet-expected message is (or arrives) within `window`.
    pub fn expect_no_message(&self, window: Duration) {
        self.inbox.expect_none_blocking(window, &self.who())
    }

    /// Block until the mock has handled everything this thread sent before.
    pub fn settle(&self) {
        if let Err(error) = self
            .actor
            .request_with_timeout(Fence, Duration::from_secs(5))
        {
            panic!(
                "{}: could not settle ({error}); exit reason {:?}, log {:?}",
                self.who(),
                self.actor.exit_reason(),
                self.received()
            );
        }
    }

    /// Every message received so far, in arrival order.
    pub fn received(&self) -> Vec<Received> {
        self.inbox.received()
    }

    /// How many `M` were received so far.
    pub fn count<M: Message>(&self) -> usize {
        self.inbox.count::<M>()
    }

    /// Take every not-yet-expected `M`, oldest first.
    pub fn take_all<M: Message>(&self) -> Vec<M> {
        self.inbox.take_all::<M>()
    }

    /// Panic if any message had no rule.
    pub fn assert_no_unexpected(&self) {
        self.inbox.assert_no_unexpected(&self.who());
    }
}

/// A native [`Recipient<M>`] that records what it is sent; see
/// [`tasks::Probe`](crate::tasks::Probe).
pub struct Probe<M: Message> {
    core: Arc<ProbeCore<M>>,
}

impl<M: Message> Clone for Probe<M> {
    fn clone(&self) -> Self {
        Self {
            core: self.core.clone(),
        }
    }
}

impl<M: Message> Default for Probe<M> {
    fn default() -> Self {
        Self::new()
    }
}

impl<M: Message> Receiver<M> for ProbeCore<M> {
    fn send(&self, msg: M) -> Result<(), ActorError> {
        ProbeCore::send(self, msg)
    }
    fn request_raw(&self, msg: M) -> Result<oneshot::Receiver<M::Result>, ActorError> {
        let (tx, rx) = oneshot::channel();
        if let Some(value) = self.request(msg)? {
            let _ = tx.send(value);
        }
        Ok(rx)
    }
}

impl<M: Message> Probe<M> {
    /// A probe for fire-and-forget messages.
    pub fn new() -> Self {
        Self {
            core: Arc::new(ProbeCore::new(None)),
        }
    }

    /// A probe that answers requests with `reply(&msg)`.
    pub fn replying(reply: impl FnMut(&M) -> M::Result + Send + 'static) -> Self {
        Self {
            core: Arc::new(ProbeCore::new(Some(Box::new(reply)))),
        }
    }

    /// The probe as a native `Recipient<M>`.
    pub fn recipient(&self) -> Recipient<M> {
        self.core.clone()
    }

    /// Block until the oldest not-yet-expected message arrives.
    pub fn expect(&self, timeout: Duration) -> M {
        self.core.inbox.expect_blocking::<M>(timeout, "Probe")
    }

    /// Panic if any not-yet-expected message is (or arrives) within `window`.
    pub fn expect_no_message(&self, window: Duration) {
        self.core.inbox.expect_none_blocking(window, "Probe")
    }

    /// Every message received so far, in arrival order.
    pub fn received(&self) -> Vec<Received> {
        self.core.inbox.received()
    }

    /// Take every not-yet-expected message, oldest first.
    pub fn take_all(&self) -> Vec<M> {
        self.core.inbox.take_all::<M>()
    }

    /// Fault: behave like a stopped actor.
    pub fn close(&self) {
        self.core.close();
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
