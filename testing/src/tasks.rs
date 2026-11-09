//! Test kit for `tasks` (async) actors: [`Mock`], [`Probe`], harness helpers.
use crate::faults;
use crate::inbox::{Inbox, Received};
use crate::probe::ProbeCore;
use crate::rules::{Fence, Plan, Rules};
use spawned_concurrency::error::ActorError;
use spawned_concurrency::message::Message;
use spawned_concurrency::supervision::{SupervisedChild, Supervisor, SupervisorReport};
use spawned_concurrency::system::{ActorSystem, ExecutionGroup, ShutdownReport, SystemError};
use spawned_concurrency::tasks::{
    Actor, ActorRef, ActorStart, Context, Handler, Receiver, Recipient,
};
use spawned_concurrency::{ChildHandle, ExitReason};
use spawned_rt::tasks::{oneshot, sleep, timeout};
use std::future::Future;
use std::ops::Deref;
use std::sync::Arc;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Mock
// ---------------------------------------------------------------------------

/// A closure-backed actor that handles **every** message type.
///
/// `Mock` implements `Handler<M>` for all `M: Message`, so `ActorRef<Mock>`
/// satisfies every `#[protocol]` blanket impl: call protocol methods on it and
/// convert it with the generated `to_x_ref()` to any protocol reference (`XRef`).
/// Code under test that holds an `XRef` or a `Recipient<M>` needs no change.
///
/// Each message is logged in arrival order. Configure replies per type with
/// [`on`](Mock::on); a message with no rule makes the mock panic with the
/// message type and the rule to add, so the caller sees `ActorStopped` and
/// [`MockRef::assert_no_unexpected`] / [`MockRef::received`] say why.
///
/// ```ignore
/// let prices = Mock::new().on::<price_protocol::Price>(|_| 7).start();
/// assert_eq!(invoice(&prices.to_price_ref(), 3).await, 21);
/// let _asked: price_protocol::Price = prices.expect(Duration::from_secs(1)).await;
/// ```
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

    /// Answer each `M` with `reply(&msg)`. The message is still logged for
    /// [`MockRef::expect`]. For fire-and-forget messages return `()`.
    pub fn on<M: Message>(mut self, reply: impl FnMut(&M) -> M::Result + Send + 'static) -> Self {
        self.rules.on::<M>(reply);
        self
    }

    /// Acknowledge and log every message whose reply type is `()` (protocol
    /// sends) even without a rule. Requests without a rule still fail loudly.
    pub fn allow_sends(mut self) -> Self {
        self.rules.allow_sends();
        self
    }

    /// Fault: hold each `M` for `delay` before replying. The mock handles one
    /// message at a time, so later messages wait too, as with a slow actor.
    /// A delay longer than the caller's timeout forces `RequestTimeout`.
    pub fn delay<M: Message>(mut self, delay: Duration) -> Self {
        self.rules.delay::<M>(delay);
        self
    }

    /// Fault: log each `M` as [`crate::Outcome::Discarded`] and do nothing else, as if
    /// the message were lost after a successful send.
    pub fn discard<M: Message<Result = ()>>(mut self) -> Self {
        self.rules.discard::<M>();
        self
    }

    /// Fault: panic with `reason` on each `M`. The mock exits with
    /// `ExitReason::Panic` and pending callers see `ActorStopped`.
    pub fn crash_on<M: Message>(mut self, reason: impl Into<String>) -> Self {
        self.rules.crash_on::<M>(reason.into());
        self
    }

    /// Start on the ambient Tokio runtime.
    pub fn start(self) -> MockRef {
        let inbox = self.inbox.clone();
        MockRef {
            actor: ActorStart::start(self),
            inbox,
        }
    }

    /// Start on an [`ExecutionGroup`].
    pub fn start_on(self, group: &ExecutionGroup) -> Result<MockRef, SystemError> {
        let inbox = self.inbox.clone();
        Ok(MockRef {
            actor: ActorStart::start_on(self, group)?,
            inbox,
        })
    }
}

impl Actor for Mock {}

impl<M: Message> Handler<M> for Mock {
    fn handle(&mut self, msg: M, _: &Context<Self>) -> impl Future<Output = M::Result> + Send {
        let plan = self.rules.plan(&self.inbox, msg);
        async move {
            match plan {
                Plan::Reply { delay, value } => {
                    if let Some(delay) = delay {
                        sleep(delay).await;
                    }
                    value
                }
                Plan::Crash(reason) => panic!("{reason}"),
            }
        }
    }
}

/// A running [`Mock`]: dereferences to `ActorRef<Mock>` (protocol methods,
/// `to_x_ref()`, `recipient()`, `child_handle()`) and reads the receive log.
///
/// Inherent methods win over protocol methods of the same name; call those
/// through [`actor_ref`](MockRef::actor_ref) or the protocol reference.
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

    /// Wait for the oldest not-yet-expected `M` and return it. Other message
    /// types stay in the log. Panics with the log contents after `timeout`.
    pub async fn expect<M: Message>(&self, timeout: Duration) -> M {
        self.inbox.expect_async::<M>(timeout, &self.who()).await
    }

    /// Panic if any not-yet-expected message is (or arrives) in the log within `window`.
    pub async fn expect_no_message(&self, window: Duration) {
        self.inbox.expect_none_async(window, &self.who()).await
    }

    /// Wait until the mock has handled every message sent to it before this
    /// call by this task (per-sender FIFO). Panics if the mock has stopped.
    pub async fn settle(&self) {
        if let Err(error) = self
            .actor
            .request_with_timeout(Fence, Duration::from_secs(5))
            .await
        {
            panic!(
                "{}: could not settle ({error}); exit reason {:?}, log {:?}",
                self.who(),
                self.actor.exit_reason(),
                self.received()
            );
        }
    }

    /// Every message received so far, in arrival order, including expected ones.
    pub fn received(&self) -> Vec<Received> {
        self.inbox.received()
    }

    /// How many `M` were received so far, including expected ones.
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

// ---------------------------------------------------------------------------
// Probe
// ---------------------------------------------------------------------------

/// A [`Recipient<M>`] that records what it is sent; no actor or runtime work.
///
/// Requests are answered by the closure given to [`replying`](Probe::replying);
/// a plain [`Probe::new`] logs a request as [`crate::Outcome::Unexpected`] and drops
/// its reply, so the caller sees `ActorStopped`.
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

    /// The probe as a `Recipient<M>`, to hand to the code under test.
    pub fn recipient(&self) -> Recipient<M> {
        self.core.clone()
    }

    /// Wait for the oldest not-yet-expected message. Panics after `timeout`.
    pub async fn expect(&self, timeout: Duration) -> M {
        self.core.inbox.expect_async::<M>(timeout, "Probe").await
    }

    /// Panic if any not-yet-expected message is (or arrives) within `window`.
    pub async fn expect_no_message(&self, window: Duration) {
        self.core.inbox.expect_none_async(window, "Probe").await
    }

    /// Every message received so far, in arrival order, including expected ones.
    pub fn received(&self) -> Vec<Received> {
        self.core.inbox.received()
    }

    /// Take every not-yet-expected message, oldest first.
    pub fn take_all(&self) -> Vec<M> {
        self.core.inbox.take_all::<M>()
    }

    /// Fault: behave like a stopped actor; later sends and requests return
    /// `ActorStopped`.
    pub fn close(&self) {
        self.core.close();
    }
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// Send `fence` as a request and wait for its reply: every message this task
/// sent to `actor` earlier has then been handled (mailboxes are FIFO per
/// sender). Use any cheap request the actor already handles. Panics on error.
pub async fn settle<A, M>(actor: &ActorRef<A>, fence: M, timeout: Duration) -> M::Result
where
    A: Handler<M>,
    M: Message,
{
    match actor.request_with_timeout(fence, timeout).await {
        Ok(reply) => reply,
        Err(error) => panic!(
            "actor {:?} did not settle on `{}` within {timeout:?}: {error}; exit reason {:?}",
            actor.id(),
            std::any::type_name::<M>(),
            actor.exit_reason()
        ),
    }
}

/// Wait for `handle` to exit, or panic after `deadline` instead of hanging.
/// Works for actors of either mode.
pub async fn exit_within(handle: &ChildHandle, deadline: Duration) -> ExitReason {
    match timeout(deadline, handle.wait_exit_async()).await {
        Ok(reason) => reason,
        Err(_) => panic!(
            "actor {:?} did not exit within {deadline:?}; a handler may be blocked or the \
             actor was never asked to stop",
            handle.id()
        ),
    }
}

/// [`exit_within`] for an `ActorRef`.
pub async fn join_within<A: Actor>(actor: &ActorRef<A>, deadline: Duration) -> ExitReason {
    exit_within(&actor.child_handle(), deadline).await
}

/// Poll `condition` every millisecond until it holds; panic naming `what` after `timeout`.
pub async fn wait_until(timeout: Duration, what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !condition() {
        if Instant::now() >= deadline {
            panic!("timed out after {timeout:?} waiting until {what}");
        }
        sleep(Duration::from_millis(1)).await;
    }
}

/// Wait for a ready generation strictly greater than `after`, or panic with
/// the child's snapshot after `deadline`.
pub async fn wait_for_generation<A: Actor>(
    child: &SupervisedChild<A>,
    after: u64,
    deadline: Duration,
) -> ActorRef<A> {
    match timeout(deadline, child.wait_for_generation(after)).await {
        Ok(Ok(actor)) => actor,
        Ok(Err(unavailable)) => panic!(
            "supervised child will not reach a generation after {after}: {unavailable}; \
             snapshot {:?}",
            child.snapshot()
        ),
        Err(_) => panic!(
            "supervised child did not reach a generation after {after} within {deadline:?}; \
             snapshot {:?}",
            child.snapshot()
        ),
    }
}

/// Fault: make the current instance of `child` exit with `reason` (see
/// [`faults::exit`]) and return its replacement once the supervisor has started
/// it, or panic after `deadline` (for example when the restart mode does not
/// restart that reason).
pub async fn restart_child<A: Actor>(
    child: &SupervisedChild<A>,
    reason: ExitReason,
    deadline: Duration,
) -> ActorRef<A> {
    let generation = child.snapshot().generation;
    let current = child
        .current()
        .unwrap_or_else(|e| panic!("no running instance to fault: {e}"));
    faults::exit(&current.child_handle(), reason);
    wait_for_generation(child, generation, deadline).await
}

/// Shut a supervisor down within `deadline`, or panic with its report.
pub async fn shutdown_supervisor(supervisor: &Supervisor, deadline: Duration) -> SupervisorReport {
    let report = supervisor.shutdown(deadline).await;
    assert!(
        report.complete,
        "supervisor did not shut down within {deadline:?}: failure {:?}, children {:#?}",
        report.failure, report.children
    );
    report
}

/// Shut an [`ActorSystem`] down within `deadline`, or panic with a report of
/// what is still running instead of hanging the test.
pub async fn shutdown_system(system: &ActorSystem, deadline: Duration) -> ShutdownReport {
    let report = system.shutdown(deadline).await;
    assert!(
        report.complete,
        "ActorSystem did not shut down within {deadline:?}\n  unfinished groups: {:?}\n  \
         unfinished actors: {:?}\n  exits: {:?}\n  supervisor failures: {:?}\n  \
         group failures: {:?}",
        report.unfinished_groups,
        report.unfinished_actors,
        report.exits,
        report.supervisor_failures,
        report.group_failures
    );
    report
}

/// An [`ActorSystem`] with one async execution group, for tests.
pub struct TestSystem {
    system: ActorSystem,
    group: ExecutionGroup,
}

impl TestSystem {
    /// A system with a two-worker async group named `test`.
    pub fn new() -> Self {
        Self::with_workers(2)
    }

    /// A system with an async group of `workers` workers.
    pub fn with_workers(workers: usize) -> Self {
        let system = ActorSystem::new().expect("test ActorSystem");
        let group = system
            .async_group("test", workers)
            .expect("test execution group");
        Self { system, group }
    }

    pub fn system(&self) -> &ActorSystem {
        &self.system
    }

    pub fn group(&self) -> &ExecutionGroup {
        &self.group
    }

    /// Start `actor` on the test group. Panics if the system is closed.
    pub fn start<A: Actor>(&self, actor: A) -> ActorRef<A> {
        ActorStart::start_on(actor, &self.group).expect("start on test group")
    }

    /// Start a mock on the test group.
    pub fn start_mock(&self, mock: Mock) -> MockRef {
        mock.start_on(&self.group)
            .expect("start mock on test group")
    }

    /// [`shutdown_system`]: panics with a report if not complete by `deadline`.
    pub async fn shutdown(self, deadline: Duration) -> ShutdownReport {
        shutdown_system(&self.system, deadline).await
    }
}

impl Default for TestSystem {
    fn default() -> Self {
        Self::new()
    }
}
