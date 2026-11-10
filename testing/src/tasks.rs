//! Test kit for `tasks` (async) actors: [`Mock`], [`Probe`], harness helpers.
//!
//! Every async wait here is bounded by the Tokio timer only, so the helpers
//! also work under `#[tokio::test(start_paused = true)]`, where idle time
//! auto-advances.
//!
//! `#[track_caller]` does not reach into an `async fn`, so the waiting
//! helpers are plain functions returning a future: they record the caller's
//! location when called and name it in their failure message (`called at
//! file:line`). The panic itself is still reported inside this crate.
use crate::faults;
use crate::inbox::{Inbox, Received};
use crate::probe::ProbeCore;
use crate::rules::{Fence, Plan, Rules};
use crate::shared::SETTLE_TIMEOUT;
use spawned_concurrency::error::ActorError;
use spawned_concurrency::message::Message;
use spawned_concurrency::supervision::{ChildState, SupervisedChild, Supervisor, SupervisorReport};
use spawned_concurrency::system::{ActorSystem, ExecutionGroup, ShutdownReport, SystemError};
use spawned_concurrency::tasks::{
    Actor, ActorRef, ActorStart, Context, Handler, Receiver, Recipient,
};
use spawned_concurrency::{ChildHandle, ExitReason};
use spawned_concurrency::reply::{Deadline, PendingReply, ReplyReceiver};
use spawned_rt::tasks::{sleep, timeout};
use std::future::Future;
use std::ops::Deref;
use std::panic::Location;
use std::sync::Arc;
use std::time::Duration;

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
    mock_builder!();

    /// Fault: hold each `M` for `delay` before replying. The mock handles one
    /// message at a time, so later messages wait too, as with a slow actor.
    /// A delay longer than the caller's timeout forces `RequestTimeout`.
    pub fn delay<M: Message>(mut self, delay: Duration) -> Self {
        self.rules.delay::<M>(delay);
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
    fn handle(
        &mut self,
        msg: M,
        reply: PendingReply<'_, M::Result>,
        _: &Context<Self>,
    ) -> impl Future<Output = ()> + Send {
        let plan = self.rules.plan(&self.inbox, msg);
        async move {
            match plan {
                Plan::Reply {
                    delay,
                    value,
                    record,
                } => {
                    if let Some(delay) = delay {
                        sleep(delay).await;
                    }
                    if let Some(record) = record {
                        record.finish();
                    }
                    let _ = reply.reply(value);
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

mock_ref_common!();

impl MockRef {
    /// Wait for the oldest not-yet-expected `M` and return it. Other message
    /// types stay in the log. Panics with the log contents after `timeout`.
    #[track_caller]
    pub fn expect<M: Message>(&self, timeout: Duration) -> impl Future<Output = M> + '_ {
        let at = Location::caller();
        async move { self.inbox.expect_async::<M>(timeout, &self.who(), at).await }
    }

    /// Panic if any not-yet-expected message is (or arrives) in the log within `window`.
    #[track_caller]
    pub fn expect_no_message(&self, window: Duration) -> impl Future<Output = ()> + '_ {
        let at = Location::caller();
        async move { self.inbox.expect_none_async(window, &self.who(), at).await }
    }

    /// Wait until the mock has handled every message sent to it before this
    /// call by this task (per-sender FIFO). Panics if the mock has stopped or
    /// does not answer within 5 s; see [`settle_within`](MockRef::settle_within).
    #[track_caller]
    pub fn settle(&self) -> impl Future<Output = ()> + '_ {
        self.settle_within(SETTLE_TIMEOUT)
    }

    /// [`settle`](MockRef::settle) with a chosen bound, for mocks with long
    /// `delay` rules or tests that want to fail faster.
    #[track_caller]
    pub fn settle_within(&self, timeout: Duration) -> impl Future<Output = ()> + '_ {
        let at = Location::caller();
        async move {
            if let Err(error) = self.actor.request_with_timeout(Fence, timeout).await {
                self.settle_failed(error, at);
            }
        }
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

probe_common!();

impl<M: Message> Probe<M> {
    /// Wait for the oldest not-yet-expected message. Panics after `timeout`.
    #[track_caller]
    pub fn expect(&self, timeout: Duration) -> impl Future<Output = M> + '_ {
        let at = Location::caller();
        async move {
            self.core
                .inbox
                .expect_async::<M>(timeout, "Probe", at)
                .await
        }
    }

    /// Panic if any not-yet-expected message is (or arrives) within `window`.
    #[track_caller]
    pub fn expect_no_message(&self, window: Duration) -> impl Future<Output = ()> + '_ {
        let at = Location::caller();
        async move { self.core.inbox.expect_none_async(window, "Probe", at).await }
    }
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// Send `fence` as a request and wait for its reply: every message this task
/// sent to `actor` earlier has then been handled (mailboxes are FIFO per
/// sender). Use any cheap request the actor already handles. Panics on error.
#[track_caller]
pub fn settle<A, M>(
    actor: &ActorRef<A>,
    fence: M,
    timeout: Duration,
) -> impl Future<Output = M::Result> + '_
where
    A: Handler<M>,
    M: Message,
{
    let at = Location::caller();
    async move {
        match actor.request_with_timeout(fence, timeout).await {
            Ok(reply) => reply,
            Err(error) => panic!(
                "actor {:?} did not settle on `{}` within {timeout:?}: {error}; exit reason \
                 {:?} (settle called at {at})",
                actor.id(),
                std::any::type_name::<M>(),
                actor.exit_reason()
            ),
        }
    }
}

/// Wait for `handle` to exit, or panic after `deadline` instead of hanging.
/// Works for actors of either mode.
#[track_caller]
pub fn exit_within(handle: &ChildHandle, deadline: Duration) -> impl Future<Output = ExitReason> {
    exit_within_at(handle.clone(), deadline, Location::caller())
}

async fn exit_within_at(
    handle: ChildHandle,
    deadline: Duration,
    at: &'static Location<'static>,
) -> ExitReason {
    match timeout(deadline, handle.wait_exit_async()).await {
        Ok(reason) => reason,
        Err(_) => panic!(
            "actor {:?} did not exit within {deadline:?}; a handler may be blocked or the \
             actor was never asked to stop (exit_within called at {at})",
            handle.id()
        ),
    }
}

/// [`exit_within`] for an `ActorRef`.
#[track_caller]
pub fn join_within<A: Actor>(
    actor: &ActorRef<A>,
    deadline: Duration,
) -> impl Future<Output = ExitReason> {
    exit_within_at(actor.child_handle(), deadline, Location::caller())
}

/// Poll `condition` every millisecond until it holds; panic naming `what` after `timeout`.
/// Measured on the Tokio clock, like every wait in this module.
#[track_caller]
pub fn wait_until<'a>(
    limit: Duration,
    what: &'a str,
    mut condition: impl FnMut() -> bool + 'a,
) -> impl Future<Output = ()> + 'a {
    let at = Location::caller();
    async move {
        let poll = async {
            while !condition() {
                sleep(Duration::from_millis(1)).await;
            }
        };
        if timeout(limit, poll).await.is_err() {
            panic!("timed out after {limit:?} waiting until {what} (wait_until called at {at})");
        }
    }
}

/// Wait for a ready generation strictly greater than `after`, or panic with
/// the child's snapshot after `deadline`.
#[track_caller]
pub fn wait_for_generation<A: Actor>(
    child: &SupervisedChild<A>,
    after: u64,
    deadline: Duration,
) -> impl Future<Output = ActorRef<A>> + '_ {
    let at = Location::caller();
    async move {
        match timeout(deadline, child.wait_for_generation(after)).await {
            Ok(Ok(actor)) => actor,
            Ok(Err(unavailable)) => panic!(
                "supervised child will not reach a generation after {after}: {unavailable}; \
                 snapshot {:?} (wait_for_generation called at {at})",
                child.snapshot()
            ),
            Err(_) => panic!(
                "supervised child did not reach a generation after {after} within \
                 {deadline:?}; snapshot {:?} (wait_for_generation called at {at})",
                child.snapshot()
            ),
        }
    }
}

/// Fault: make the current instance of `child` exit with `reason` (see
/// [`faults::exit`]) and return its replacement once the supervisor has started
/// it, or panic after `deadline` (for example when the restart mode does not
/// restart that reason). `deadline` bounds the whole call: finding a running
/// instance, injecting the fault and waiting for the replacement.
///
/// The instance targeted and the generation waited past always belong
/// together, even if the child restarts on its own meanwhile, so the returned
/// reference is the first replacement of the targeted instance. A later
/// instance is never faulted: the fault is aimed at the targeted instance's
/// own handle, and a `Kill` is committed before `faults::exit` returns. If the
/// targeted instance dies of its own cause first, the fault is a no-op and the
/// replacement after that death is returned.
#[track_caller]
pub fn restart_child<A: Actor>(
    child: &SupervisedChild<A>,
    reason: ExitReason,
    deadline: Duration,
) -> impl Future<Output = ActorRef<A>> + '_ {
    let at = Location::caller();
    async move {
        // The generation faulted, once the fault is injected.
        let faulted = std::cell::Cell::new(None);
        let run = async {
            let (current, generation) = faultable_instance(child).await;
            faults::exit(&current.child_handle(), reason);
            faulted.set(Some(generation));
            child.wait_for_generation(generation).await
        };
        let outcome = timeout(deadline, run).await;
        match (outcome, faulted.get()) {
            (Ok(Ok(actor)), _) => actor,
            (Ok(Err(unavailable)), generation) => panic!(
                "supervised child will not reach a generation after {generation:?}: \
                 {unavailable}; snapshot {:?} (restart_child called at {at})",
                child.snapshot()
            ),
            (Err(_), None) => panic!(
                "supervised child never had a stable instance to fault within {deadline:?}; \
                 snapshot {:?} (restart_child called at {at})",
                child.snapshot()
            ),
            (Err(_), Some(generation)) => panic!(
                "supervised child did not reach a generation after {generation} within \
                 {deadline:?} (one deadline for the whole call); snapshot {:?} \
                 (restart_child called at {at})",
                child.snapshot()
            ),
        }
    }
}

/// The running instance and its generation, from views that agree. `current()`
/// and `snapshot()` take the lock separately; a restart between them would
/// pair instance N with generation N+1, so retry until the snapshot names the
/// instance read. Unbounded here: the caller's deadline bounds it.
async fn faultable_instance<A: Actor>(child: &SupervisedChild<A>) -> (ActorRef<A>, u64) {
    loop {
        match child.current() {
            Ok(current) => {
                let snapshot = child.snapshot();
                if snapshot.actor_id == Some(current.id()) {
                    return (current, snapshot.generation);
                }
            }
            // A child that will never run again has nothing to fault.
            Err(e)
                if matches!(
                    child.snapshot().state,
                    ChildState::Stopped | ChildState::Failed
                ) =>
            {
                panic!("no running instance to fault: {e}")
            }
            // Between generations (an instance stopping, backoff,
            // starting): wait for the next instance, up to the deadline.
            Err(_) => {}
        }
        sleep(Duration::from_millis(1)).await;
    }
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
