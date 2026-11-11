//! OneForOne recovery on explicitly owned execution groups.
//!
//! A replacement is a fresh actor instance. Old ActorRef values never rebind,
//! accepted work is not replayed, and constructors do not recover state unless
//! application code explicitly does so.
use crate::lifecycle_trace::TreeEvent;
use crate::{
    lifecycle_trace::{self as trace, Event, TraceId},
    system::{
        self, ActorSystem, Admission, ExecutionGroup, Scope, ScopedTask, ShutdownUnit, SystemError,
        Tracked,
    },
    tasks::{Actor, ActorRef},
    ActorId, ExitReason,
};
use futures::{
    future::{select, BoxFuture, Either},
    FutureExt,
};
use spawned_rt::tasks::{self as rt, watch, CancellationToken};
use std::{
    collections::VecDeque,
    future::Future,
    panic::AssertUnwindSafe,
    sync::{Arc, Mutex, Weak},
    time::Duration,
};
// Virtual time under `--cfg spawned_sim`; `std::time::Instant` otherwise.
use spawned_rt::time::Instant;

mod nested;
pub use nested::{SupervisedSupervisor, SupervisorSpec};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartMode {
    Permanent,
    Transient,
    Temporary,
}
impl RestartMode {
    /// The restart table. A Transient child restarts if its final reason is
    /// abnormal or a cleanup failure (a destructor panic after its death
    /// point) was recorded: `is_abnormal()` alone no longer predicts it.
    fn restarts(self, reason: Option<&ExitReason>, cleanup_failed: bool) -> bool {
        match self {
            Self::Permanent => true,
            Self::Transient => {
                cleanup_failed || reason.map(ExitReason::is_abnormal).unwrap_or(true)
            }
            Self::Temporary => false,
        }
    }
}

/// Trace projection of a child's exit: the raw reason class, or the abnormal
/// class when a cleanup failure was recorded, so `Supervision.tla`'s restart
/// decision (`RestartFollowsPolicy`) stays valid.
fn exit_class(reason: &ExitReason, cleanup_failed: bool) -> &'static str {
    if cleanup_failed && !reason.is_abnormal() {
        "Panic"
    } else {
        trace::reason(Some(reason))
    }
}

#[derive(Debug, Clone)]
pub struct Backoff {
    /// First retry's delay cap, not its minimum. With jitter enabled the delay
    /// can be anywhere from zero through this value (1s permits a 150ms retry).
    /// Each unstable failure doubles the cap, up to `maximum`; it resets after
    /// `stable_after` ready time. Without jitter the delay equals the cap.
    pub initial: Duration,
    /// Cap on the exponential delay before jitter.
    pub maximum: Duration,
    pub stable_after: Duration,
    /// Full jitter: draw from zero through the capped bound.
    /// For initial=1s/max=5s, bounds are 1s, 2s, 4s, then 5s. Disable for a
    /// deterministic configured delay. Actual restart latency also includes
    /// cleanup, placement, construction and readiness.
    pub jitter: bool,
    /// Reproducible policy testing. This is scheduling jitter, not cryptography.
    pub seed: Option<u64>,
}
impl Default for Backoff {
    fn default() -> Self {
        Self {
            initial: Duration::from_millis(10),
            maximum: Duration::from_secs(5),
            stable_after: Duration::from_secs(30),
            jitter: true,
            seed: None,
        }
    }
}
impl Backoff {
    /// Capped exponential delay before jitter.
    pub(crate) fn bound(&self, failures: u32) -> Duration {
        self.initial
            .saturating_mul(1u32 << failures.min(31))
            .min(self.maximum)
    }
    /// With jitter, a draw from `0..=bound(failures)`.
    pub(crate) fn delay(&self, failures: u32, rng: &mut u64) -> Duration {
        let bound = self.bound(failures);
        if !self.jitter {
            return bound;
        }
        jitter(bound, rng)
    }
}

/// Advances the jitter stream and draws a delay in `0..=bound`.
pub(crate) fn jitter(bound: Duration, rng: &mut u64) -> Duration {
    *rng = rng
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    let nanos = bound.as_nanos().min(u64::MAX as u128) as u64;
    Duration::from_nanos(jitter_draw(nanos, *rng))
}

/// Maps a stream value onto `0..=nanos`.
pub(crate) fn jitter_draw(nanos: u64, rng: u64) -> u64 {
    if nanos == u64::MAX {
        rng
    } else {
        rng % (nanos + 1)
    }
}

/// Per-child jitter stream: the supervisor seed mixed with the child's name,
/// so siblings sharing a seed draw different delays. Injective in `seed`.
pub(crate) fn jitter_seed(seed: u64, name: &[u8]) -> u64 {
    seed ^ name
        .iter()
        .fold(0u64, |a, b| a.wrapping_mul(31).wrapping_add(*b as u64))
}

/// The affected children are fully joined before ordered reconstruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SupervisorStrategy {
    #[default]
    OneForOne,
    OneForAll,
    RestForOne,
}

#[derive(Debug, Clone)]
pub struct SupervisorConfig {
    pub strategy: SupervisorStrategy,
    /// Supervisor-wide restarts per `restart_window` (OTP's intensity and
    /// period). One restart is one restart action: a OneForOne child retry,
    /// or one OneForAll/RestForOne recovery round, however many children
    /// the round rebuilds. A failed replacement inside a round requests a
    /// new round, which is charged again (OTP's `try_again_restart`).
    pub max_restarts: usize,
    pub restart_window: Duration,
    pub max_children: usize,
    pub backoff: Backoff,
}
impl Default for SupervisorConfig {
    fn default() -> Self {
        Self {
            strategy: SupervisorStrategy::OneForOne,
            max_restarts: 5,
            restart_window: Duration::from_secs(30),
            max_children: 1024,
            backoff: Backoff::default(),
        }
    }
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum SupervisorError {
    #[error(transparent)]
    System(#[from] SystemError),
    #[error("invalid supervisor configuration: {0}")]
    Configuration(String),
    #[error("child name already exists: {0}")]
    DuplicateChild(String),
    #[error("child capacity exceeded")]
    Capacity,
    /// The start was cancelled or the system closed before it completed.
    #[error("supervisor stopped before startup completed")]
    Stopped,
    #[error("supervisor failed: {0}")]
    Failed(String),
    /// A child's first start failed (S1, OTP `failed_to_start_child`): its
    /// factory errored or panicked, it stopped or panicked before readiness,
    /// it missed `startup_timeout`, or its placement or group failed.
    /// Returned after the rollback finished; `rollback_failures` lists
    /// cleanup failures of that rollback.
    #[error("failed to start child {child}: {cause}")]
    FailedToStartChild {
        child: String,
        cause: String,
        rollback_failures: Vec<String>,
    },
    /// A child's cleanup got stuck during the initial start (S2).
    #[error("child cleanup stuck during initial start: {child}: {cause}")]
    StartupCleanupStuck {
        child: String,
        cause: String,
        rollback_failures: Vec<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildState {
    Waiting,
    Starting,
    Running,
    Backoff,
    Stopping,
    Stopped,
    Failed,
}

#[derive(Debug, Clone)]
pub struct ChildSnapshot {
    pub name: String,
    pub state: ChildState,
    pub generation: u64,
    pub actor_id: Option<ActorId>,
    pub last_exit: Option<ExitReason>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("child is not currently ready: {0:?}")]
pub struct ChildUnavailable(pub ChildState);

/// The construction scope is allocated before an ActorId exists. Its helpers
/// run on the assigned group and are included in cleanup before a retry.
#[derive(Clone)]
pub struct FactoryContext {
    scope: Arc<Scope>,
    generation: u64,
}
impl FactoryContext {
    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn execution_group(&self) -> ExecutionGroup {
        self.scope.group()
    }
    pub fn spawn_helper<F>(&self, future: F) -> Result<ScopedTask<F::Output>, SystemError>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.scope.spawn_helper(future)
    }
    pub async fn cancelled(&self) {
        self.scope.token.cancelled().await;
    }
    pub fn spawn_blocking_helper<F, T>(&self, work: F) -> Result<ScopedTask<T>, SystemError>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        self.scope.spawn_blocking_helper(work)
    }
}

type Factory<A> =
    Arc<dyn Fn(FactoryContext) -> BoxFuture<'static, Result<A, String>> + Send + Sync>;
pub struct ChildSpec<A: Actor> {
    name: String,
    group: ExecutionGroup,
    factory: Factory<A>,
    restart: RestartMode,
    startup_timeout: Duration,
    shutdown_timeout: Duration,
    bounds: crate::reply::jobs::Bounds,
}
impl<A: Actor> ChildSpec<A> {
    /// Create a permanent child with five-second startup and shutdown limits.
    /// Set both limits explicitly when the application's deadlines are shorter.
    pub fn new<F, U, E>(name: impl Into<String>, group: &ExecutionGroup, factory: F) -> Self
    where
        F: Fn(FactoryContext) -> U + Send + Sync + 'static,
        U: Future<Output = Result<A, E>> + Send + 'static,
        E: std::fmt::Display,
    {
        let factory = Arc::new(factory);
        Self {
            name: name.into(),
            group: group.clone(),
            factory: Arc::new(move |ctx| {
                let factory = factory.clone();
                Box::pin(async move { factory(ctx).await.map_err(|e| e.to_string()) })
            }),
            restart: RestartMode::Permanent,
            startup_timeout: Duration::from_secs(5),
            shutdown_timeout: Duration::from_secs(5),
            bounds: Default::default(),
        }
    }
    /// Override `Actor::MAX_OUTSTANDING` (deferred credits per instance,
    /// capped at 65535) for every generation of this child.
    pub fn max_outstanding(mut self, max: usize) -> Self {
        self.bounds.max_outstanding = Some(max);
        self
    }
    /// Override `Actor::MAX_NATIVE_JOBS` (blocking jobs running at once) for
    /// every generation of this child. The default (at most 4) is easy to
    /// outgrow; its only symptom under load is `NativeBudget` refusals.
    pub fn max_native_jobs(mut self, max: usize) -> Self {
        self.bounds.max_native_jobs = Some(max);
        self
    }
    pub fn restart(mut self, restart: RestartMode) -> Self {
        self.restart = restart;
        self
    }
    /// Limit construction plus readiness for each attempt (default: five seconds).
    pub fn startup_timeout(mut self, timeout: Duration) -> Self {
        self.startup_timeout = timeout;
        self
    }
    /// Limit each generation's cleanup before failing the tree (default: five
    /// seconds). System shutdown has a separate ordering/report deadline; expiry
    /// signals remaining children but still retains ownership until cleanup joins.
    pub fn shutdown_timeout(mut self, timeout: Duration) -> Self {
        self.shutdown_timeout = timeout;
        self
    }
}

struct RecoveryMeta {
    id: u64,
    origin: usize,
    finished: std::sync::atomic::AtomicBool,
}
struct RecoverySlot {
    meta: Arc<RecoveryMeta>,
    stop: CancellationToken,
    resume: CancellationToken,
    cleaned: watch::Sender<bool>,
    ready: watch::Sender<bool>,
}
impl RecoverySlot {
    fn new(meta: Arc<RecoveryMeta>) -> Arc<Self> {
        let (cleaned, _) = watch::channel(false);
        let (ready, _) = watch::channel(false);
        Arc::new(Self {
            meta,
            stop: CancellationToken::new(),
            resume: CancellationToken::new(),
            cleaned,
            ready,
        })
    }
}
pub(crate) struct RecoveryQueue {
    pub(crate) pending: Vec<bool>,
    pub(crate) covered: Vec<u64>,
    pub(crate) next_id: u64,
}
impl RecoveryQueue {
    /// Opens the next round at the lowest pending origin: clears pending from
    /// `start` on, stamps those children as covered by the fresh round id and
    /// returns `(origin, start, id)`.
    pub(crate) fn select(&mut self, one_for_all: bool) -> Option<(usize, usize, u64)> {
        let origin = self.pending.iter().position(|pending| *pending)?;
        let start = if one_for_all { 0 } else { origin };
        for pending in &mut self.pending[start..] {
            *pending = false;
        }
        let id = self.next_id;
        self.next_id = id.checked_add(1).expect("recovery IDs exhausted");
        self.covered[start..].fill(id);
        Some((origin, start, id))
    }
    pub(crate) fn request(&mut self, index: usize, after: u64) -> bool {
        if after < self.covered[index] {
            return false;
        }
        self.pending[index] = true;
        true
    }
}
struct ViewData<A: Actor> {
    recovery: Option<Arc<RecoverySlot>>,
    snapshot: ChildSnapshot,
    current: Option<ActorRef<A>>,
    scope: Option<Arc<Scope>>,
}
struct View<A: Actor> {
    index: usize,
    data: Mutex<ViewData<A>>,
    gate: Arc<Admission>,
    failed: Arc<std::sync::atomic::AtomicBool>,
    changed: watch::Sender<u64>,
    initial: watch::Sender<bool>,
    stop_turn: CancellationToken,
    done: watch::Sender<bool>,
    shutdown_timeout: Duration,
    /// 1-based declaration index: the model's child id in lifecycle traces.
    trace_index: usize,
}
impl<A: Actor> View<A> {
    fn unavailable(&self) -> ChildUnavailable {
        if self.failed.load(std::sync::atomic::Ordering::Acquire) {
            ChildUnavailable(ChildState::Failed)
        } else {
            let state = system::lock(&self.data).snapshot.state;
            ChildUnavailable(
                if matches!(state, ChildState::Stopped | ChildState::Failed) {
                    state
                } else {
                    ChildState::Stopping
                },
            )
        }
    }
    fn change(&self, f: impl FnOnce(&mut ViewData<A>)) {
        {
            f(&mut system::lock(&self.data));
        }
        self.changed.send_modify(|n| *n = n.wrapping_add(1));
    }
    fn recovery(&self, after: u64) -> Option<Arc<RecoverySlot>> {
        system::lock(&self.data)
            .recovery
            .clone()
            .filter(|r| r.meta.id > after)
    }
    async fn next_recovery(&self, after: u64) -> Arc<RecoverySlot> {
        let mut changes = self.changed.subscribe();
        loop {
            changes.borrow_and_update();
            if let Some(round) = self.recovery(after) {
                return round;
            }
            let _ = changes.changed().await;
        }
    }
    async fn recovery_stopping(&self, after: u64) {
        self.next_recovery(after).await.stop.cancelled().await;
    }
    fn finish_unless_recovering(&self, after: u64, failed: bool) -> bool {
        {
            let mut data = system::lock(&self.data);
            if !failed && data.recovery.as_ref().is_some_and(|r| r.meta.id > after) {
                return false;
            }
            data.current = None;
            data.scope = None;
            data.snapshot.state = if failed || data.snapshot.state == ChildState::Failed {
                ChildState::Failed
            } else {
                ChildState::Stopped
            };
        }
        self.changed.send_modify(|n| *n = n.wrapping_add(1));
        self.done.send_replace(true);
        true
    }
    /// Cleanup stuck past its deadlines: the snapshot settles as Failed,
    /// which releases a waiting `start()` (S2), while the controller keeps
    /// awaiting the real join.
    fn mark_stuck(&self) {
        self.change(|data| {
            data.current = None;
            data.snapshot.state = ChildState::Failed;
        });
    }
    /// Terminal, or settled as Failed by a stuck cleanup.
    async fn settled(&self) {
        let mut changes = self.changed.subscribe();
        loop {
            changes.borrow_and_update();
            let state = system::lock(&self.data).snapshot.state;
            if matches!(state, ChildState::Stopped | ChildState::Failed) || *self.done.borrow() {
                return;
            }
            if changes.changed().await.is_err() {
                return;
            }
        }
    }
    fn finish(&self, failed: bool) {
        self.change(|data| {
            data.current = None;
            data.scope = None;
            data.snapshot.state = if failed || data.snapshot.state == ChildState::Failed {
                ChildState::Failed
            } else {
                ChildState::Stopped
            };
        });
        self.done.send_replace(true);
    }
}

/// Explicit lookup of a replacement, not a rebound ActorRef. Resolution can
/// race with a subsequent crash; callers must still handle delivery errors.
pub struct SupervisedChild<A: Actor> {
    view: Arc<View<A>>,
}
impl<A: Actor> Clone for SupervisedChild<A> {
    fn clone(&self) -> Self {
        Self {
            view: self.view.clone(),
        }
    }
}
impl<A: Actor> SupervisedChild<A> {
    pub fn snapshot(&self) -> ChildSnapshot {
        system::lock(&self.view.data).snapshot.clone()
    }
    pub fn current(&self) -> Result<ActorRef<A>, ChildUnavailable> {
        self.view
            .gate
            .if_open(|| {
                let data = system::lock(&self.view.data);
                if data.scope.as_ref().is_some_and(|s| s.token.is_cancelled()) {
                    return Err(ChildUnavailable(ChildState::Stopping));
                }
                data.current
                    .clone()
                    .ok_or(ChildUnavailable(data.snapshot.state))
            })
            .unwrap_or_else(|| Err(self.view.unavailable()))
    }
    pub async fn wait_ready(&self) -> Result<ActorRef<A>, ChildUnavailable> {
        self.wait_for_generation(0).await
    }
    /// Wait for a ready generation strictly greater than the supplied value.
    pub async fn wait_for_generation(&self, after: u64) -> Result<ActorRef<A>, ChildUnavailable> {
        let mut changes = self.view.changed.subscribe();
        loop {
            changes.borrow_and_update();
            let snapshot = self.snapshot();
            if snapshot.generation > after {
                if let Ok(actor) = self.current() {
                    return Ok(actor);
                }
            }
            if matches!(snapshot.state, ChildState::Stopped | ChildState::Failed) {
                return Err(ChildUnavailable(snapshot.state));
            }
            if !self.view.gate.is_open() {
                return Err(self.view.unavailable());
            }
            if changes.changed().await.is_err() {
                return Err(ChildUnavailable(ChildState::Stopped));
            }
        }
    }
}

trait ChildControl: Send + Sync {
    fn assign_recovery(&self, slot: Arc<RecoverySlot>) -> bool;
    fn stop_generation(&self, slot: &RecoverySlot);
    fn snapshot(&self) -> ChildSnapshot;
    fn close_admission(&self);
    fn stop(&self);
    fn done(&self) -> watch::Receiver<bool>;
    fn initial(&self) -> watch::Receiver<bool>;
    fn shutdown_timeout(&self) -> Duration;
    fn finish_unstarted(&self, failed: bool);
    fn settled(&self) -> BoxFuture<'_, ()>;
}
impl<A: Actor> ChildControl for View<A> {
    fn assign_recovery(&self, slot: Arc<RecoverySlot>) -> bool {
        let old = {
            let mut data = system::lock(&self.data);
            if matches!(
                data.snapshot.state,
                ChildState::Stopped | ChildState::Failed
            ) {
                return false;
            }
            let old = data.recovery.replace(slot);
            data.current = None;
            data.snapshot.state = ChildState::Stopping;
            if let Some(scope) = &data.scope {
                scope.close_admission();
            }
            old
        };
        drop(old);
        self.changed.send_modify(|n| *n = n.wrapping_add(1));
        true
    }
    /// Collateral stop (OneForAll/RestForOne): the supervisor's parent exit
    /// signal (E3); the controller escalates to a kill at `shutdown_timeout`.
    fn stop_generation(&self, slot: &RecoverySlot) {
        slot.stop.cancel();
        let scope = system::lock(&self.data).scope.clone();
        if let Some(scope) = scope {
            scope.signal_parent(ExitReason::Shutdown);
        }
    }
    fn snapshot(&self) -> ChildSnapshot {
        system::lock(&self.data).snapshot.clone()
    }
    fn close_admission(&self) {
        let (scope, constructing) = {
            let data = system::lock(&self.data);
            (data.scope.clone(), data.current.is_none())
        };
        if let Some(scope) = scope {
            scope.close_admission();
            if constructing {
                scope.stop(ExitReason::Shutdown);
            }
        }
        self.changed.send_modify(|n| *n = n.wrapping_add(1));
    }
    /// Supervisor shutdown of this child (V9, reverse order): the parent
    /// exit signal `exit(Child, shutdown)` (E3). A non-trapping child ends
    /// `Shutdown` without `stopped()`; a trapping child finishes its earlier
    /// messages and runs `stopped()`. The controller kills it at
    /// `shutdown_timeout`.
    fn stop(&self) {
        self.stop_turn.cancel();
        if let Some(scope) = system::lock(&self.data).scope.clone() {
            scope.signal_parent(ExitReason::Shutdown);
        }
    }
    fn done(&self) -> watch::Receiver<bool> {
        self.done.subscribe()
    }
    fn initial(&self) -> watch::Receiver<bool> {
        self.initial.subscribe()
    }
    fn shutdown_timeout(&self) -> Duration {
        self.shutdown_timeout
    }
    fn finish_unstarted(&self, failed: bool) {
        if self.snapshot().generation == 0 {
            self.finish(failed);
        }
    }
    fn settled(&self) -> BoxFuture<'_, ()> {
        Box::pin(View::settled(self))
    }
}

struct RetryBudget {
    attempts: VecDeque<Instant>,
    reserved: usize,
    max: usize,
    window: Duration,
}
impl RetryBudget {
    fn available(&self, now: Instant) -> bool {
        self.attempts
            .iter()
            .filter(|&&time| now.saturating_duration_since(time) < self.window)
            .count()
            .saturating_add(self.reserved)
            < self.max
    }
    fn take(&mut self, now: Instant) -> bool {
        admit_in_window(
            &mut self.attempts,
            self.max.saturating_sub(self.reserved),
            self.window,
            now,
            |now, t| now.saturating_duration_since(t),
        )
    }
}

/// Rolling-window rate limit: forget stamps at least `window` old, then admit
/// `now` only while fewer than `max` remain. `since(now, t)` is the elapsed
/// time from `t` to `now`, generic so the window can be checked without clocks.
pub(crate) fn admit_in_window<T: Copy>(
    attempts: &mut VecDeque<T>,
    max: usize,
    window: Duration,
    now: T,
    since: impl Fn(T, T) -> Duration,
) -> bool {
    while attempts.front().is_some_and(|t| since(now, *t) >= window) {
        attempts.pop_front();
    }
    if attempts.len() >= max {
        return false;
    }
    attempts.push_back(now);
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailureKind {
    Other,
    /// A child's first start failed (S1).
    FirstStart,
    Cleanup,
    Coordinator,
    RestartBudget,
}
#[derive(Debug, Clone)]
struct FailureOrigin {
    child: String,
    generation: u64,
    reason: Option<ExitReason>,
    last_error: Option<String>,
}
#[derive(Debug, Clone)]
struct Failure {
    kind: FailureKind,
    message: String,
    origin: Option<FailureOrigin>,
    logged: bool,
}
impl Failure {
    fn new(kind: FailureKind, message: String, origin: Option<FailureOrigin>) -> Self {
        Self {
            kind,
            message,
            origin,
            logged: false,
        }
    }
}
impl std::ops::Deref for Failure {
    type Target = str;
    fn deref(&self) -> &str {
        &self.message
    }
}

struct Core {
    recovery: Mutex<RecoveryQueue>,
    recovery_changed: watch::Sender<u64>,
    gate: Arc<Admission>,
    cancel: CancellationToken,
    admission_closed: CancellationToken,
    done: watch::Sender<bool>,
    ready: watch::Sender<bool>,
    failure: Mutex<Option<Failure>>,
    failed: Arc<std::sync::atomic::AtomicBool>,
    cleanup_failure: Mutex<Option<String>>,
    /// Every recorded cleanup failure, for the rollback list of a failed
    /// start (S2).
    cleanup_failures: Mutex<Vec<String>>,
    children: Mutex<Vec<Arc<dyn ChildControl>>>,
    budget: Mutex<RetryBudget>,
    config: SupervisorConfig,
    trace: TraceId,
    #[cfg(test)]
    initial_ready_pause: Mutex<Option<(CancellationToken, watch::Sender<bool>)>>,
    #[cfg(test)]
    retry_admission_pause: Mutex<Option<(CancellationToken, watch::Sender<bool>)>>,
    #[cfg(feature = "lifecycle-trace")]
    deadline_recorded: std::sync::atomic::AtomicBool,
}
impl Core {
    fn close_admission(&self) {
        trace::atomic(|| {
            if self.gate.is_open() {
                self.cohort(|| Event::sup("CClose"));
                trace::tree(self.trace, |id| TreeEvent::new("TClose", id));
            }
            #[cfg(feature = "lifecycle-trace")]
            if self.gate.is_open() && !trace::close_suppressed() {
                trace::emit(self.trace, || Event::sup("Close"));
            }
            self.gate.close();
        });
        self.admission_closed.cancel();
        let children = system::lock(&self.children).clone();
        for child in children {
            child.close_admission();
        }
    }
    fn close(&self) {
        trace::atomic(|| {
            self.close_admission();
            if !self.cancel.is_cancelled() {
                trace::tree(self.trace, |id| TreeEvent::new("TCancel", id));
                self.cancel.cancel();
            }
        });
    }
    fn cancel_unready_startup(&self) {
        trace::atomic(|| {
            let cancel = self.gate.close_unless(
                || *self.ready.borrow(),
                || {
                    self.cohort(|| Event::sup("CClose"));
                    trace::tree(self.trace, |id| TreeEvent::new("TClose", id));
                    #[cfg(feature = "lifecycle-trace")]
                    if !trace::close_suppressed() {
                        trace::emit(self.trace, || Event::sup("Close"));
                    }
                },
            );
            if cancel {
                self.close();
            }
        });
    }
    fn stop_remaining(&self) {
        trace::atomic(|| {
            self.close();
            #[cfg(feature = "lifecycle-trace")]
            if !*self.done.borrow()
                && !self
                    .deadline_recorded
                    .swap(true, std::sync::atomic::Ordering::Relaxed)
            {
                trace::emit(self.trace, || Event::sup("ShutdownDeadline"));
            }
        });
        let children = system::lock(&self.children).clone();
        for child in children.iter().rev() {
            child.stop();
        }
    }
    #[cfg(test)]
    fn fail(&self, error: String) {
        self.fail_kind(FailureKind::Other, error);
    }
    fn fail_kind(&self, kind: FailureKind, error: String) {
        self.fail_record(Failure::new(kind, error, None));
    }
    fn fail_record(&self, record: Failure) {
        let mut failure = system::lock(&self.failure);
        if failure.is_none() {
            *failure = Some(record);
        }
        self.failed
            .store(true, std::sync::atomic::Ordering::Release);
        drop(failure);
        self.close();
    }
    #[inline(always)]
    fn cohort(&self, make: impl FnOnce() -> Event) {
        #[cfg(feature = "lifecycle-trace")]
        if self.config.strategy != SupervisorStrategy::OneForOne {
            trace::emit(self.trace, make);
        }
        #[cfg(not(feature = "lifecycle-trace"))]
        let _ = make;
    }
    fn record_cleanup_failure(&self, error: &str) {
        system::lock(&self.cleanup_failures).push(error.to_string());
        let mut failure = system::lock(&self.cleanup_failure);
        if failure.is_none() {
            *failure = Some(error.to_string());
        }
    }
    fn fail_cleanup(&self, error: String) {
        self.record_cleanup_failure(&error);
        self.fail_kind(FailureKind::Cleanup, error);
    }
    fn fail_cleanup_for(
        &self,
        snapshot: ChildSnapshot,
        error: String,
        event: impl FnOnce() -> Option<Event>,
    ) {
        self.record_cleanup_failure(&error);
        let record = snapshot_failure(snapshot, FailureKind::Cleanup, error);
        trace::failing(|| {
            trace::emit_some(self.trace, event);
            self.fail_record(record);
        });
    }
    fn request_recovery(&self, index: usize, after: u64) {
        trace::atomic(|| {
            self.gate.if_open(|| {
                let mut queue = system::lock(&self.recovery);
                let accepted = queue.request(index, after);
                self.cohort(|| {
                    Event::new("CRequest", index + 1, trace::flag(accepted)).round(after)
                });
                if accepted {
                    self.recovery_changed
                        .send_modify(|n| *n = n.wrapping_add(1));
                }
            })
        });
    }
    /// `fail` as part of the model action `event` (whose `Fail` closes the
    /// gate), so the gate flip is not traced as a separate `Close`.
    fn fail_as_kind(
        &self,
        kind: FailureKind,
        error: String,
        event: impl FnOnce() -> Option<Event>,
    ) {
        trace::failing(|| {
            trace::emit_some(self.trace, event);
            self.fail_kind(kind, error);
        });
    }
    fn log_failure_once(&self) {
        let failure = {
            let mut slot = system::lock(&self.failure);
            let Some(failure) = slot.as_mut() else {
                return;
            };
            if failure.logged {
                return;
            }
            failure.logged = true;
            failure.clone()
        };
        let (child, generation, reason, last_error) = failure
            .origin
            .as_ref()
            .map(|origin| {
                (
                    origin.child.as_str(),
                    origin.generation,
                    origin.reason.as_ref(),
                    origin.last_error.as_deref(),
                )
            })
            .unwrap_or(("<supervisor>", 0, None, None));
        let event = if failure.kind == FailureKind::RestartBudget {
            "restart_budget_exhausted"
        } else {
            "supervision_give_up"
        };
        if let Err(payload) = std::panic::catch_unwind(AssertUnwindSafe(|| {
            tracing::warn!(event, child, generation, ?reason, ?last_error,
                kind = ?failure.kind, error = %failure.message, "supervisor failed");
        })) {
            crate::mailbox::dispose_panic(payload);
        }
    }
    /// A concurrent ordinary stop wins over a later child failure. The gate
    /// lock, rather than optional trace instrumentation, commits this decision.
    #[cfg(test)]
    fn fail_if_open_as(&self, error: String, event: impl FnOnce() -> Option<Event>) -> bool {
        self.fail_record_if_open_as(Failure::new(FailureKind::Other, error, None), event)
    }
    fn fail_record_if_open_as(
        &self,
        record: Failure,
        event: impl FnOnce() -> Option<Event>,
    ) -> bool {
        trace::failing(|| {
            let committed = self.gate.close_if_open(|| {
                trace::emit_some(self.trace, event);
                self.cohort(|| Event::sup("CClose"));
                trace::tree(self.trace, |id| TreeEvent::new("TClose", id));
                let mut failure = system::lock(&self.failure);
                if failure.is_none() {
                    *failure = Some(record);
                }
                self.failed
                    .store(true, std::sync::atomic::Ordering::Release);
            });
            if committed {
                self.close();
            }
            committed
        })
    }
    fn reserve_retry(&self, child: usize) -> Option<RetryReservation<'_>> {
        trace::atomic(|| {
            self.gate
                .if_open(|| {
                    let mut budget = system::lock(&self.budget);
                    let now = Instant::now();
                    // Pending backoffs reserve capacity, so siblings cannot all sleep
                    // against the same final permit. Only actual grants enter the window.
                    while budget
                        .attempts
                        .front()
                        .is_some_and(|time| now.saturating_duration_since(*time) >= budget.window)
                    {
                        budget.attempts.pop_front();
                    }
                    if !budget.available(now) {
                        return None;
                    }
                    budget.reserved += 1;
                    trace::emit(self.trace, || {
                        Event::new("RetryReserve", child, "")
                            .count(budget.attempts.len() + budget.reserved)
                    });
                    Some(RetryReservation {
                        core: self,
                        child,
                        held: true,
                    })
                })
                .flatten()
        })
    }
    /// One OneForAll/RestForOne recovery round is one restart action (OTP's
    /// `add_restart` runs once per restart, before the cohort is rebuilt):
    /// take its grant now. The round's controllers then rebuild without a
    /// grant of their own. Pending per-child reservations (retries during
    /// the initial start) keep their capacity. Returns whether it was taken.
    fn charge_round(&self) -> bool {
        let mut budget = system::lock(&self.budget);
        budget.take(Instant::now())
    }
    /// Caller holds the admission gate and slot lock; charge atomically with
    /// generation admission, so placement failure or supersession refunds it.
    fn retry_under_gate(&self, reservation: &mut RetryReservation<'_>) -> bool {
        let mut budget = system::lock(&self.budget);
        budget.reserved -= 1;
        reservation.held = false;
        let taken = budget.take(Instant::now());
        debug_assert!(taken, "reserved retry lost its capacity");
        if taken {
            trace::emit(self.trace, || {
                Event::new("BackoffDone", reservation.child, "retry")
                    .count(budget.attempts.len() + budget.reserved)
            });
        }
        taken
    }
    #[cfg(test)]
    fn retry(&self, reservation: &mut RetryReservation<'_>) -> bool {
        self.gate
            .if_open(|| self.retry_under_gate(reservation))
            .unwrap_or(false)
    }
}
struct RetryReservation<'a> {
    core: &'a Core,
    child: usize,
    held: bool,
}
impl Drop for RetryReservation<'_> {
    fn drop(&mut self) {
        if self.held {
            trace::atomic(|| {
                let mut budget = system::lock(&self.core.budget);
                budget.reserved -= 1;
                trace::emit(self.core.trace, || {
                    Event::new("RetryRelease", self.child, "")
                        .count(budget.attempts.len() + budget.reserved)
                });
            });
        }
    }
}
impl ShutdownUnit for Core {
    fn close_admission(&self) {
        Core::close_admission(self);
    }
    fn stop(&self) {
        Core::close(self);
    }
    fn stop_remaining(&self) {
        Core::stop_remaining(self);
    }
    fn joined(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let _ = system::wait_true(self.done.subscribe()).await;
        })
    }
    fn is_done(&self) -> bool {
        *self.done.borrow()
    }
    fn failure(&self) -> Option<String> {
        system::lock(&self.failure)
            .as_ref()
            .map(|f| f.message.clone())
    }
}

trait Runner: Send {
    fn run(self: Box<Self>, core: Arc<Core>) -> BoxFuture<'static, ()>;
}
struct TypedRunner<A: Actor> {
    spec: ChildSpec<A>,
    view: Arc<View<A>>,
}
impl<A: Actor> Runner for TypedRunner<A> {
    fn run(self: Box<Self>, core: Arc<Core>) -> BoxFuture<'static, ()> {
        let view = self.view.clone();
        Box::pin(async move {
            if let Err(payload) = AssertUnwindSafe(run_child(self.spec, self.view, core.clone()))
                .catch_unwind()
                .await
            {
                let message = format!(
                    "child controller panicked: {}",
                    crate::error::panic_message(&*payload)
                );
                crate::mailbox::dispose_panic(payload);
                core.fail_cleanup_for(view.snapshot(), message, || {
                    Some(Event::new(
                        "Unmodelled",
                        view.trace_index,
                        "ControllerPanicked",
                    ))
                });
                let scope = system::lock(&view.data).scope.clone();
                if let Some(scope) = scope {
                    scope.stop(ExitReason::Shutdown);
                    scope.joined().await;
                }
                view.finish(true);
            }
        })
    }
}

pub struct SupervisorBuilder {
    owner: Weak<system::SystemInner>,
    core: Arc<Core>,
    runners: Vec<Box<dyn Runner>>,
    started: bool,
}
impl SupervisorBuilder {
    pub(crate) fn new(
        system: &ActorSystem,
        config: SupervisorConfig,
    ) -> Result<Self, SupervisorError> {
        Self::from_owner(Arc::downgrade(&system.inner), config)
    }
    fn from_owner(
        owner: Weak<system::SystemInner>,
        config: SupervisorConfig,
    ) -> Result<Self, SupervisorError> {
        if config.restart_window.is_zero()
            || config.max_children == 0
            || config.max_children > 65536
            || config.max_restarts > 65536
            || config.backoff.maximum < config.backoff.initial
        {
            return Err(SupervisorError::Configuration(
                "invalid limits or backoff".into(),
            ));
        }
        let (done, _) = watch::channel(false);
        let (ready, _) = watch::channel(false);
        let (recovery_changed, _) = watch::channel(0);
        let core = Arc::new(Core {
            recovery: Mutex::new(RecoveryQueue {
                pending: Vec::new(),
                covered: Vec::new(),
                next_id: 1,
            }),
            recovery_changed,
            gate: Admission::new(),
            cancel: CancellationToken::new(),
            admission_closed: CancellationToken::new(),
            done,
            ready,
            failure: Mutex::new(None),
            failed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            cleanup_failure: Mutex::new(None),
            cleanup_failures: Mutex::new(Vec::new()),
            children: Mutex::new(Vec::new()),
            budget: Mutex::new(RetryBudget {
                attempts: VecDeque::new(),
                reserved: 0,
                max: config.max_restarts,
                window: config.restart_window,
            }),
            config,
            trace: TraceId::next(),
            #[cfg(test)]
            initial_ready_pause: Mutex::new(None),
            #[cfg(test)]
            retry_admission_pause: Mutex::new(None),
            #[cfg(feature = "lifecycle-trace")]
            deadline_recorded: Default::default(),
        });
        #[cfg(feature = "lifecycle-trace")]
        trace::describe(
            core.trace,
            match core.config.strategy {
                SupervisorStrategy::OneForOne => "OneForOne",
                SupervisorStrategy::OneForAll => "OneForAll",
                SupervisorStrategy::RestForOne => "RestForOne",
            },
            core.config.max_restarts,
        );
        Ok(Self {
            owner,
            core,
            runners: Vec::new(),
            started: false,
        })
    }

    /// Lifecycle trace id of this supervisor (`lifecycle_trace::take`).
    #[cfg(feature = "lifecycle-trace")]
    #[doc(hidden)]
    pub fn trace_id(&self) -> u64 {
        self.core.trace.0
    }

    pub fn add_supervisor(
        &mut self,
        spec: SupervisorSpec,
    ) -> Result<SupervisedSupervisor, SupervisorError> {
        nested::add(self, spec)
    }
    pub fn add<A: Actor>(
        &mut self,
        spec: ChildSpec<A>,
    ) -> Result<SupervisedChild<A>, SupervisorError> {
        self.owner
            .upgrade()
            .ok_or(SystemError::Closed)?
            .accepts_group(&spec.group)?;
        if spec.name.is_empty()
            || spec.name.contains('\0')
            || spec.startup_timeout.is_zero()
            || spec.shutdown_timeout.is_zero()
        {
            return Err(SupervisorError::Configuration(
                "invalid child name or timeout".into(),
            ));
        }
        let mut children = system::lock(&self.core.children);
        if children.len() >= self.core.config.max_children {
            return Err(SupervisorError::Capacity);
        }
        if children.iter().any(|c| c.snapshot().name == spec.name) {
            return Err(SupervisorError::DuplicateChild(spec.name));
        }
        let (changed, _) = watch::channel(0);
        let (initial, _) = watch::channel(false);
        let (done, _) = watch::channel(false);
        let view = Arc::new(View {
            index: children.len(),
            data: Mutex::new(ViewData {
                recovery: None,
                snapshot: ChildSnapshot {
                    name: spec.name.clone(),
                    state: ChildState::Waiting,
                    generation: 0,
                    actor_id: None,
                    last_exit: None,
                    last_error: None,
                },
                current: None,
                scope: None,
            }),
            gate: self.core.gate.clone(),
            failed: self.core.failed.clone(),
            changed,
            initial,
            done,
            stop_turn: CancellationToken::new(),
            shutdown_timeout: spec.shutdown_timeout,
            trace_index: children.len() + 1,
        });
        children.push(view.clone());
        #[cfg(feature = "lifecycle-trace")]
        trace::describe_child(
            self.core.trace,
            match spec.restart {
                RestartMode::Permanent => "Permanent",
                RestartMode::Transient => "Transient",
                RestartMode::Temporary => "Temporary",
            },
        );
        {
            let mut queue = system::lock(&self.core.recovery);
            queue.pending.push(false);
            queue.covered.push(0);
        }
        self.runners.push(Box::new(TypedRunner {
            spec,
            view: view.clone(),
        }));
        Ok(SupervisedChild { view })
    }

    /// Start children in declaration order. Dropping this future before readiness
    /// commits requests cleanup. After readiness, ActorSystem owns the tree even
    /// if this future is dropped before returning its Supervisor handle.
    pub async fn start(self) -> Result<Supervisor, SupervisorError> {
        self.start_owned(true).await
    }
    async fn start_owned(mut self, register_root: bool) -> Result<Supervisor, SupervisorError> {
        let owner = self.owner.upgrade().ok_or(SystemError::Closed)?;
        // Registration and controller dispatch happen without an intervening await.
        if register_root {
            owner.register_unit(self.core.clone())?;
        }
        trace::tree(self.core.trace, |id| {
            TreeEvent::new("TCreate", id).peer(0, self.runners.len())
        });
        self.started = true;
        let runners = std::mem::take(&mut self.runners);
        let core = self.core.clone();
        owner
            .control
            .handle
            .spawn(run_supervisor(core.clone(), runners));
        let mut guard = StartupGuard {
            core: core.clone(),
            armed: true,
        };
        let ready = core.ready.subscribe();
        let cancelled = core.cancel.cancelled();
        let _ = select(Box::pin(cancelled), Box::pin(system::wait_true(ready))).await;
        // Readiness is historical, just like ActorRef::ready(). A later system
        // admission close must not turn successful startup into rollback.
        let initialized = *core.ready.borrow();
        if initialized {
            guard.armed = false;
            return Ok(Supervisor { core: core.clone() });
        }
        // S2: the error is returned only after the rollback finished: every
        // child is terminal, or settled as Failed by its stuck cleanup.
        let children = system::lock(&core.children).clone();
        for child in &children {
            child.settled().await;
        }
        Err(core.start_error())
    }
}
impl Core {
    /// The cause of a failed initial start (S2), with the rollback's cleanup
    /// failures listed separately.
    fn start_error(&self) -> SupervisorError {
        let Some(failure) = system::lock(&self.failure).clone() else {
            return SupervisorError::Stopped;
        };
        let rollback_failures: Vec<String> = system::lock(&self.cleanup_failures)
            .iter()
            .filter(|f| **f != failure.message)
            .cloned()
            .collect();
        match (failure.kind, failure.origin) {
            (FailureKind::FirstStart, Some(origin)) => {
                let prefix = format!("failed to start child {}: ", origin.child);
                let cause = failure
                    .message
                    .strip_prefix(&prefix)
                    .unwrap_or(&failure.message)
                    .to_string();
                SupervisorError::FailedToStartChild {
                    child: origin.child,
                    cause,
                    rollback_failures,
                }
            }
            (FailureKind::Cleanup, Some(origin)) => SupervisorError::StartupCleanupStuck {
                child: origin.child,
                cause: failure.message,
                rollback_failures,
            },
            _ => SupervisorError::Failed(failure.message),
        }
    }
}
impl Drop for SupervisorBuilder {
    fn drop(&mut self) {
        if !self.started {
            self.core.close();
            for child in system::lock(&self.core.children).iter() {
                child.finish_unstarted(self.core.failed.load(std::sync::atomic::Ordering::Acquire));
            }
            trace::atomic(|| {
                trace::tree(self.core.trace, |id| TreeEvent::new("TDone", id));
                self.core.done.send_replace(true);
            });
        }
    }
}
struct StartupGuard {
    core: Arc<Core>,
    armed: bool,
}
impl Drop for StartupGuard {
    fn drop(&mut self) {
        if self.armed {
            self.core.cancel_unready_startup();
        }
    }
}

/// Owned by ActorSystem even if public handles are dropped.
#[derive(Clone)]
pub struct Supervisor {
    core: Arc<Core>,
}
#[derive(Debug, Clone)]
pub struct SupervisorReport {
    pub complete: bool,
    pub failure: Option<String>,
    pub children: Vec<ChildSnapshot>,
}
impl Supervisor {
    pub fn stop(&self) {
        self.core.close();
    }
    /// Lifecycle trace id of this supervisor (`lifecycle_trace::take`).
    #[cfg(feature = "lifecycle-trace")]
    #[doc(hidden)]
    pub fn trace_id(&self) -> u64 {
        self.core.trace.0
    }
    pub fn snapshots(&self) -> Vec<ChildSnapshot> {
        system::lock(&self.core.children)
            .iter()
            .map(|c| c.snapshot())
            .collect()
    }
    pub fn failure(&self) -> Option<String> {
        system::lock(&self.core.failure)
            .as_ref()
            .map(|f| f.message.clone())
    }
    pub async fn join(&self) {
        let _ = system::wait_true(self.core.done.subscribe()).await;
    }
    pub async fn shutdown(&self, grace: Duration) -> SupervisorReport {
        self.stop();
        let complete = rt::timeout(grace, system::wait_true(self.core.done.subscribe()))
            .await
            .unwrap_or(false);
        if !complete {
            self.core.stop_remaining();
        }
        SupervisorReport {
            complete,
            failure: self.failure(),
            children: self.snapshots(),
        }
    }
}

async fn run_supervisor(core: Arc<Core>, runners: Vec<Box<dyn Runner>>) {
    // All coordinator phases share a boundary. Keep ownership and join progress
    // outside it so a panic during startup, stopping, joining or final publication
    // cannot detach a controller or lose the runtime/subtree completion signal.
    let mut runners = Some(runners);
    let mut tasks = Vec::new();
    let mut joined = 0;
    let mut coordination = None;
    let mut emergency = false;
    loop {
        let result = AssertUnwindSafe(async {
            if let Some(runners) = runners.take() {
                run_supervisor_body(core.clone(), runners, &mut tasks, &mut coordination).await;
            }
            if emergency {
                let children = system::lock(&core.children).clone();
                let mut signalled = true;
                for child in children.iter().rev() {
                    if *child.done().borrow() {
                        continue;
                    }
                    if let Err(payload) = std::panic::catch_unwind(AssertUnwindSafe(|| {
                        child.finish_unstarted(true);
                        child.stop();
                    })) {
                        let message = crate::error::panic_message(&*payload);
                        crate::mailbox::dispose_panic(payload);
                        core.record_cleanup_failure(&format!(
                            "emergency child stop panicked: {message}"
                        ));
                        signalled = false;
                    }
                }
                if !signalled {
                    return false;
                }
            }
            while joined < tasks.len() {
                let result = (&mut tasks[joined]).await;
                joined += 1; // Never poll a consumed JoinHandle after a later panic.
                if let Err(error) = result {
                    let message = format!("child controller failed: {error}");
                    if error.is_panic() {
                        crate::mailbox::dispose_panic(error.into_panic());
                    }
                    core.record_cleanup_failure(&message);
                    core.fail_as_kind(FailureKind::Coordinator, message, || {
                        Some(Event::new("Unmodelled", 0, "ControllerFailed"))
                    });
                }
            }
            if let Some(task) = coordination.as_mut() {
                let result = task.await;
                coordination = None;
                if let Err(error) = result {
                    let message = format!("recovery coordinator failed: {error}");
                    if error.is_panic() {
                        crate::mailbox::dispose_panic(error.into_panic());
                    }
                    core.record_cleanup_failure(&message);
                    core.fail_kind(FailureKind::Coordinator, message);
                }
            }
            let children = system::lock(&core.children).clone();
            for child in &children {
                child.finish_unstarted(core.failed.load(std::sync::atomic::Ordering::Acquire));
            }
            core.log_failure_once();
            trace::atomic(|| {
                trace::emit(core.trace, || Event::sup("SupJoin"));
                core.cohort(|| Event::sup("CDone"));
                trace::tree(core.trace, |id| TreeEvent::new("TDone", id));
                // Copy the tapes before `done` lets a test `take` them.
                #[cfg(feature = "lifecycle-trace")]
                trace::dump_all(core.trace);
                core.done.send_replace(true);
            });
            true
        })
        .catch_unwind()
        .await;
        match result {
            Ok(true) => return,
            Ok(false) => (),
            Err(payload) => {
                let message = crate::error::panic_message(&*payload);
                crate::mailbox::dispose_panic(payload);
                let message = format!("supervisor coordinator panicked: {message}");
                core.record_cleanup_failure(&message);
                core.fail_kind(FailureKind::Coordinator, message);
                emergency = true;
            }
        }
        rt::sleep(Duration::from_millis(10)).await;
    }
}

async fn run_supervisor_body(
    core: Arc<Core>,
    runners: Vec<Box<dyn Runner>>,
    tasks: &mut Vec<rt::JoinHandle<()>>,
    coordination: &mut Option<rt::JoinHandle<()>>,
) {
    let children = system::lock(&core.children).clone();
    *coordination = if core.config.strategy == SupervisorStrategy::OneForOne {
        None
    } else {
        let coordinator = core.clone();
        Some(rt::spawn(async move {
            if let Err(payload) = AssertUnwindSafe(run_recoveries(coordinator.clone()))
                .catch_unwind()
                .await
            {
                let message = format!(
                    "recovery coordinator panicked: {}",
                    crate::error::panic_message(&*payload)
                );
                crate::mailbox::dispose_panic(payload);
                coordinator.record_cleanup_failure(&message);
                coordinator.fail_kind(FailureKind::Coordinator, message);
                coordinator.stop_remaining();
            }
        }))
    };
    for (index, runner) in runners.into_iter().enumerate() {
        let open = trace::atomic(|| {
            let open = core.gate.is_open();
            if open {
                trace::emit(core.trace, || Event::new("SupLaunch", index + 1, ""));
            }
            open
        });
        if !open {
            break;
        }
        tasks.push(rt::spawn(runner.run(core.clone())));
        let event = select(
            Box::pin(core.admission_closed.cancelled()),
            Box::pin(system::wait_true(children[index].initial())),
        )
        .await;
        if !matches!(event, Either::Right((true, _))) {
            break;
        }
    }
    for child in &children {
        child.finish_unstarted(core.failed.load(std::sync::atomic::Ordering::Acquire));
    }
    let initialized = trace::atomic(|| {
        let committed = core.gate.if_open(|| {
            trace::emit(core.trace, || Event::new("SupStartDone", 0, "true"));
            core.cohort(|| Event::sup("CInitialReady"));
            core.ready.send_replace(true);
        });
        if committed.is_none() {
            trace::emit(core.trace, || Event::new("SupStartDone", 0, "false"));
        }
        committed.is_some()
    });

    #[cfg(test)]
    let ready_probe = {
        let pause = system::lock(&core.initial_ready_pause).take();
        if let Some((resume, passed)) = pause {
            resume.cancelled().await;
            Some(passed)
        } else {
            None
        }
    };
    // Once readiness commits, later admission closure is not a stop turn.
    // ActorSystem owns cross-target ordering and cancels this tree in its turn.
    if !initialized {
        core.close();
    }
    #[cfg(test)]
    if let Some(passed) = ready_probe {
        passed.send_replace(true);
    }
    core.cancel.cancelled().await;
    trace::emit(core.trace, || Event::sup("SupCancelSeen"));
    for (index, child) in children.iter().enumerate().rev() {
        let c = index + 1;
        trace::emit(core.trace, || Event::new("SupStopTurn", c, ""));
        fail_point!("shutdown.before_stop_child", async);
        child.stop();
        let finished = rt::timeout(child.shutdown_timeout(), system::wait_true(child.done()))
            .await
            .is_ok();
        // The child may finish between the deadline firing and this check;
        // decide on its published completion, atomically with recording.
        trace::atomic(|| {
            if finished || *child.done().borrow() {
                trace::emit(core.trace, || Event::new("SupChildDone", c, ""));
            } else {
                core.fail_cleanup_for(
                    child.snapshot(),
                    format!("child cleanup deadline exceeded: {}", child.snapshot().name),
                    || Some(Event::new("SupChildTimeout", c, "")),
                );
            }
        });
    }
}

async fn run_recoveries(core: Arc<Core>) {
    let children = system::lock(&core.children).clone();
    let mut changes = core.recovery_changed.subscribe();
    'rounds: loop {
        changes.borrow_and_update();
        if !core.gate.is_open() {
            return;
        }
        let round = trace::atomic(|| {
            let mut queue = system::lock(&core.recovery);
            // The round is the restart action: charge it once, before any
            // child of the cohort is stopped or rebuilt.
            let origin = queue.pending.iter().position(|pending| *pending)?;
            if !core.charge_round() {
                return Some(Err(origin));
            }
            queue
                .select(core.config.strategy == SupervisorStrategy::OneForAll)
                .map(|(origin, start, id)| {
                    let meta = Arc::new(RecoveryMeta {
                        id,
                        origin,
                        finished: std::sync::atomic::AtomicBool::new(false),
                    });
                    core.cohort(|| {
                        Event::new("CSelect", origin + 1, "")
                            .round(id)
                            .count(start + 1)
                    });
                    Ok((start, meta))
                })
        });
        if let Some(Err(origin)) = round {
            // Exhausted: a concurrent stop wins; only an open supervisor fails.
            let snapshot = children[origin].snapshot();
            let message = format!("restart budget exhausted by {}", snapshot.name);
            core.fail_record_if_open_as(
                snapshot_failure(snapshot, FailureKind::RestartBudget, message),
                || None,
            );
            core.log_failure_once();
            return;
        }
        let Some(Ok((start, meta))) = round else {
            if matches!(
                select(
                    Box::pin(core.admission_closed.cancelled()),
                    Box::pin(changes.changed())
                )
                .await,
                Either::Left(_)
            ) {
                return;
            }
            continue;
        };
        let mut affected = Vec::new();
        for (affected_index, child) in children.iter().enumerate().skip(start) {
            let slot = RecoverySlot::new(meta.clone());
            let accepted = trace::atomic(|| {
                let accepted = child.assign_recovery(slot.clone());
                core.cohort(|| {
                    Event::new("CAssign", affected_index + 1, trace::flag(accepted)).round(meta.id)
                });
                accepted
            });
            if accepted {
                affected.push((affected_index + 1, child.clone(), slot));
            }
        }
        for (index, child, slot) in affected.iter().rev() {
            if !core.gate.is_open() {
                return;
            }
            trace::atomic(|| {
                child.stop_generation(slot);
                core.cohort(|| Event::new("CStop", *index, "").round(meta.id));
            });
            match select(
                Box::pin(core.admission_closed.cancelled()),
                Box::pin(rt::timeout(
                    child.shutdown_timeout(),
                    system::wait_true(slot.cleaned.subscribe()),
                )),
            )
            .await
            {
                Either::Left(_) => return,
                Either::Right((Ok(true), _)) => (),
                _ => {
                    core.fail_cleanup_for(
                        child.snapshot(),
                        format!("coordinated cleanup stuck for {}", child.snapshot().name),
                        || None,
                    );
                    core.stop_remaining();
                    return;
                }
            }
        }
        for (index, _, slot) in &affected {
            if !core.gate.is_open() {
                return;
            }
            if system::lock(&core.recovery)
                .pending
                .iter()
                .any(|pending| *pending)
            {
                trace::atomic(|| {
                    core.cohort(|| Event::new("CEnd", 0, "superseded").round(meta.id));
                    meta.finished
                        .store(true, std::sync::atomic::Ordering::Release);
                });
                continue 'rounds;
            }
            trace::atomic(|| {
                core.cohort(|| Event::new("CResume", *index, "").round(meta.id));
                slot.resume.cancel();
            });
            loop {
                changes.borrow_and_update();
                // A ready upstream or previously unaffected dependency can fail
                // while this child initializes. Supersede the round rather than
                // await a dependent that cannot start without that upstream.
                if system::lock(&core.recovery)
                    .pending
                    .iter()
                    .any(|pending| *pending)
                {
                    trace::atomic(|| {
                        core.cohort(|| Event::new("CEnd", 0, "superseded").round(meta.id));
                        meta.finished
                            .store(true, std::sync::atomic::Ordering::Release);
                    });
                    continue 'rounds;
                }
                match select(
                    Box::pin(core.admission_closed.cancelled()),
                    Box::pin(select(
                        Box::pin(system::wait_true(slot.ready.subscribe())),
                        Box::pin(changes.changed()),
                    )),
                )
                .await
                {
                    Either::Left(_) => return,
                    Either::Right((Either::Left((true, _)), _)) => break,
                    Either::Right((Either::Left((false, _)), _)) => return,
                    Either::Right((Either::Right(_), _)) => (),
                }
            }
        }
        trace::atomic(|| {
            core.cohort(|| Event::new("CEnd", 0, "complete").round(meta.id));
            meta.finished
                .store(true, std::sync::atomic::Ordering::Release);
        });
    }
}

async fn run_child<A: Actor>(spec: ChildSpec<A>, view: Arc<View<A>>, core: Arc<Core>) {
    let mut first = true;
    let mut last_recovery = 0;
    let mut collateral = false;
    // This iteration rebuilds for a recovery round (already charged).
    let mut in_round = false;
    let mut failures = 0u32;
    // Wall-clock entropy; the simulation seed's entropy stream under spawned_sim.
    let seed = core
        .config
        .backoff
        .seed
        .unwrap_or_else(spawned_rt::time::entropy);
    let mut rng = jitter_seed(seed, system::lock(&view.data).snapshot.name.as_bytes());
    let c = view.trace_index;
    loop {
        let open = trace::atomic(|| {
            let open = core.gate.is_open();
            let branch = if !open {
                "finish"
            } else if first {
                "attempt"
            } else {
                "backoff"
            };
            trace::emit(core.trace, || Event::new("LoopTop", c, branch));
            open
        });
        if !open {
            break;
        }
        if let Some(round) = view.recovery(last_recovery) {
            if matches!(
                select(
                    Box::pin(core.admission_closed.cancelled()),
                    Box::pin(round.stop.cancelled())
                )
                .await,
                Either::Left(_)
            ) {
                break;
            }
            trace::atomic(|| {
                core.cohort(|| Event::new("CClean", c, "").round(round.meta.id));
                round.cleaned.send_replace(true);
            });
            match select(
                Box::pin(core.admission_closed.cancelled()),
                Box::pin(select(
                    Box::pin(view.next_recovery(round.meta.id)),
                    Box::pin(round.resume.cancelled()),
                )),
            )
            .await
            {
                Either::Left(_) => break,
                Either::Right((Either::Left(_), _)) => continue,
                Either::Right((Either::Right(_), _)) => (),
            }
            last_recovery = round.meta.id;
            if spec.restart == RestartMode::Temporary {
                let failed = system::lock(&core.failure).is_some();
                // Publish terminal state under the same lock as assignment before
                // acknowledging the round. A superseding coordinator must either
                // skip this finished slot or be serviced by this controller.
                if trace::atomic(|| {
                    let finished = view.finish_unless_recovering(last_recovery, failed);
                    if finished {
                        trace::emit(core.trace, || Event::new("Finish", c, ""));
                        core.cohort(|| Event::new("CRetire", c, "").round(last_recovery));
                    }
                    finished
                }) {
                    trace::atomic(|| {
                        core.cohort(|| Event::new("CAck", c, "").round(round.meta.id));
                        round.ready.send_replace(true);
                    });
                    core.log_failure_once();
                    return;
                }
                continue;
            }
            collateral = round.meta.origin != view.index;
            in_round = true;
        }
        let mut backoff = Duration::ZERO;
        let mut retry_reservation = None;
        // A recovery round was charged once when the coordinator selected
        // it: its rebuilds (origin and collateral) take no grant of their own.
        let round_rebuild = std::mem::take(&mut in_round);
        if !first {
            fail_point!("supervisor.before_restart", async);
            let reservation = if round_rebuild {
                Some(None)
            } else {
                core.reserve_retry(c).map(Some)
            };
            let Some(reservation) = reservation else {
                // A concurrent stop wins: only fail a supervisor that is still open.
                trace::atomic(|| {
                    if core.gate.is_open() {
                        core.fail_record_if_open_as(
                            child_failure(
                                &spec,
                                &view,
                                FailureKind::RestartBudget,
                                format!("restart budget exhausted by {}", spec.name),
                            ),
                            || {
                                let budget = system::lock(&core.budget);
                                let held = budget.attempts.len() + budget.reserved;
                                Some(Event::new("BackoffDone", c, "exhausted").count(held))
                            },
                        );
                    } else {
                        trace::emit(core.trace, || Event::new("BackoffDone", c, "closed"));
                    }
                });
                core.log_failure_once();
                break;
            };
            let delay = if collateral {
                Duration::ZERO
            } else {
                core.config.backoff.delay(failures, &mut rng)
            };
            backoff = delay;
            let count_attempt = !collateral;
            collateral = false;
            view.change(|v| v.snapshot.state = ChildState::Backoff);
            if matches!(
                select(
                    Box::pin(core.admission_closed.cancelled()),
                    Box::pin(select(
                        Box::pin(async {
                            if core.config.strategy == SupervisorStrategy::OneForOne {
                                futures::future::pending::<()>().await;
                            } else {
                                view.recovery_stopping(last_recovery).await;
                            }
                        }),
                        Box::pin(rt::sleep(delay))
                    ))
                )
                .await,
                Either::Left(_)
            ) {
                drop(reservation);
                trace::emit(core.trace, || Event::new("BackoffDone", c, "closed"));
                break;
            }
            fail_point!("supervisor.during_backoff", async);
            if view.recovery(last_recovery).is_some() {
                continue;
            }
            if count_attempt {
                failures = failures.saturating_add(1);
            }
            retry_reservation = reservation;
        }
        first = false;
        let (scope, lease) = match spec.group.reserve(Some(core.gate.clone())) {
            Ok(attempt) => attempt,
            Err(e) => {
                trace::atomic(|| {
                    let was_retry = retry_reservation.is_some();
                    drop(retry_reservation.take());
                    if core.gate.is_open() {
                        core.fail_record_if_open_as(
                            child_failure(
                                &spec,
                                &view,
                                if *view.initial.borrow() {
                                    FailureKind::Other
                                } else {
                                    FailureKind::FirstStart
                                },
                                format!("placement failed for {}: {e}", spec.name),
                            ),
                            || Some(Event::new("Unmodelled", c, "PlacementFailed")),
                        );
                    } else {
                        trace::emit(core.trace, || {
                            Event::new(
                                if was_retry { "BackoffDone" } else { "Attempt" },
                                c,
                                if was_retry { "closed" } else { "false" },
                            )
                        });
                    }
                });
                break;
            }
        };
        #[cfg(test)]
        if retry_reservation.is_some() || round_rebuild {
            let pause = system::lock(&core.retry_admission_pause).take();
            if let Some((resume, entered)) = pause {
                entered.send_replace(true);
                resume.cancelled().await;
            }
        }
        let generation = trace::atomic(|| {
            let was_retry = retry_reservation.is_some();
            let generation = core
                .gate
                .if_open(|| {
                    let mut data = system::lock(&view.data);
                    if data
                        .recovery
                        .as_ref()
                        .is_some_and(|r| r.meta.id > last_recovery)
                    {
                        return None;
                    }
                    if let Some(reservation) = retry_reservation.as_mut() {
                        if !core.retry_under_gate(reservation) {
                            return None;
                        }
                    }
                    data.snapshot.generation += 1;
                    data.snapshot.state = ChildState::Starting;
                    data.snapshot.actor_id = None;
                    data.current = None;
                    data.scope = Some(scope.clone());
                    Some(data.snapshot.generation)
                })
                .flatten();
            if generation.is_none() && was_retry {
                drop(retry_reservation.take());
                if !core.gate.is_open() {
                    trace::emit(core.trace, || Event::new("BackoffDone", c, "closed"));
                }
            }
            if generation.is_some() || !was_retry {
                trace::emit(core.trace, || {
                    Event::new("Attempt", c, trace::flag(generation.is_some()))
                });
            }
            if let Some(generation) = generation {
                trace::tree(core.trace, |id| {
                    TreeEvent::new("TBegin", id).child(c, generation)
                });
                core.cohort(|| {
                    Event::new("CBegin", c, "")
                        .round(last_recovery)
                        .count(generation as usize)
                });
            }
            generation
        });
        let Some(generation) = generation else {
            scope.stop(ExitReason::Shutdown);
            drop(lease);
            if core.gate.is_open() {
                continue;
            } else {
                break;
            }
        };
        scope.set_name(&spec.name);
        if generation > 1 {
            let previous = view.snapshot();
            crate::actor_cleanup::log_safely(|| {
                tracing::info!(event = "restart_scheduled", child = %spec.name,
                generation = generation - 1, reason = ?previous.last_exit,
                error = ?previous.last_error, ?backoff, "child retry granted after backoff");
            });
            crate::actor_cleanup::log_safely(|| {
                tracing::info!(
                    event = "restart_started",
                    child = %spec.name,
                    generation,
                    reason = ?previous.last_exit,
                    error = ?previous.last_error,
                    "starting replacement child"
                );
            });
        }
        view.changed.send_modify(|n| *n = n.wrapping_add(1));
        let ctx = FactoryContext {
            scope: scope.clone(),
            generation,
        };
        let factory = spec.factory.clone();
        scope.set_bounds(spec.bounds);
        let attempt_scope = scope.clone();
        let construction = async move {
            if !attempt_scope.can_start() {
                return Err("scope closed before construction".into());
            }
            let factory_work = AssertUnwindSafe(async move {
                fail_point!("child.factory", async => return Err("failpoint child.factory".into()));
                factory(ctx).await
            })
            .catch_unwind();
            let result = match select(
                Box::pin(attempt_scope.token.cancelled()),
                Box::pin(factory_work),
            )
            .await
            {
                Either::Left(_) => return Err("construction cancelled".into()),
                Either::Right((Ok(result), _)) => result,
                Either::Right((Err(payload), _)) => {
                    return Err(format!(
                        "factory panicked: {}",
                        crate::error::panic_message(&*payload)
                    ))
                }
            };
            let actor = result?;
            let actor_lease = attempt_scope.actor_lease().map_err(|e| e.to_string())?;
            ActorRef::spawn_managed(actor, attempt_scope.clone(), actor_lease)
                .map_err(|e| e.to_string())
        };
        let task = scope.handle.spawn(Tracked {
            future: construction,
            lease,
        });
        let startup = async {
            let actor = task
                .await
                .map_err(|e| format!("construction task failed: {e}"))??;
            actor.ready().await.map_err(|e| e.to_string())?;
            Ok::<_, String>(actor)
        };
        let mut startup_timed_out = false;
        let began = match select(
            Box::pin(core.admission_closed.cancelled()),
            Box::pin(rt::timeout(spec.startup_timeout, startup)),
        )
        .await
        {
            Either::Left(_) => Err("supervisor stopping".to_string()),
            Either::Right((Err(_), _)) => {
                startup_timed_out = true;
                Err("startup deadline exceeded".to_string())
            }
            Either::Right((Ok(result), _)) => result,
        };
        #[cfg(feature = "failpoints")]
        let began = match began {
            Ok(actor) => {
                let mut began = Ok(actor);
                fail_point!("child.after_ready_before_publish", async => {
                    began = Err("failpoint child.after_ready_before_publish".to_string())
                });
                began
            }
            failed => failed,
        };
        let mut ready_at = None;
        let mut exit = None;
        let mut cleanup_until = None;
        // Trace bookkeeping: the model's StartupFailed (whose reason is known
        // only after cleanup) and CleanupStuck (which skips CleanupDone and
        // Decide).
        let startup_failed = began.is_err();
        let mut stuck = false;
        match began {
            Ok(actor) => {
                let published = trace::atomic(|| {
                    let published = core
                        .gate
                        .if_open(|| {
                            let mut data = system::lock(&view.data);
                            if data
                                .recovery
                                .as_ref()
                                .is_some_and(|r| r.meta.id > last_recovery)
                            {
                                return false;
                            }
                            data.snapshot.state = ChildState::Running;
                            data.snapshot.actor_id = Some(actor.id());
                            data.current = Some(actor.clone());
                            true
                        })
                        .unwrap_or(false);
                    trace::emit(core.trace, || {
                        Event::new("BecomeReady", c, trace::flag(published))
                    });
                    if published {
                        core.cohort(|| {
                            Event::new("CReady", c, "")
                                .round(last_recovery)
                                .count(generation as usize)
                        });
                    }
                    published
                });
                view.changed.send_modify(|n| *n = n.wrapping_add(1));
                if published {
                    ready_at = Some(Instant::now());
                    view.initial.send_replace(true);
                    let round = system::lock(&view.data).recovery.clone();
                    if let Some(round) = round.filter(|r| r.meta.id == last_recovery) {
                        trace::atomic(|| {
                            core.cohort(|| Event::new("CAck", c, "").round(round.meta.id));
                            round.ready.send_replace(true);
                        });
                    }
                    let child = actor.child_handle();
                    // Cancellation precedes terminal publication when stopped()
                    // hangs. Start its cleanup deadline at stop observation,
                    // rather than waiting forever for the completion channel.
                    let ended = select(
                        Box::pin(core.cancel.cancelled()),
                        Box::pin(scope.token.cancelled()),
                    )
                    .await;
                    let stop_turn = matches!(ended, Either::Left(_));
                    if stop_turn {
                        view.stop_turn.cancelled().await;
                        trace::emit(core.trace, || Event::new("StopObserved", c, ""));
                        // ChildView::stop already sent the parent's Shutdown.
                    }
                    view.change(|v| {
                        v.current = None;
                        v.snapshot.state = ChildState::Stopping;
                    });
                    let cleanup_started = Instant::now();
                    cleanup_until = Some(cleanup_started);
                    // The child's jobs are joined within the same deadline.
                    scope.set_cleanup_timeout(spec.shutdown_timeout);
                    exit = Some(
                        match rt::timeout(spec.shutdown_timeout, child.wait_exit_async()).await {
                            Ok(reason) => {
                                if !stop_turn {
                                    let failed = child.cleanup_failure().is_some();
                                    trace::emit(core.trace, || {
                                        Event::new("SelfExit", c, exit_class(&reason, failed))
                                    });
                                }
                                reason
                            }
                            // E3: the supervisor stopped this child and it
                            // did not reach its death point within
                            // shutdown_timeout: the parent's kill, then a
                            // second interval before cleanup is stuck.
                            Err(_) if stop_turn => {
                                scope.kill();
                                // The second interval runs from the kill.
                                cleanup_until = Some(Instant::now());
                                match rt::timeout(spec.shutdown_timeout, child.wait_exit_async())
                                    .await
                                {
                                    Ok(reason) => reason,
                                    Err(_) => {
                                        stuck = true;
                                        core.fail_cleanup_for(
                                            view.snapshot(),
                                            format!("actor cleanup stuck for {}", spec.name),
                                            || Some(Event::new("CleanupStuck", c, "")),
                                        );
                                        view.mark_stuck();
                                        child.wait_exit_async().await
                                    }
                                }
                            }
                            Err(_) => {
                                if !stop_turn {
                                    trace::emit(core.trace, || {
                                        Event::new("SelfExit", c, "unknown")
                                    });
                                }
                                stuck = true;
                                core.fail_cleanup_for(
                                    view.snapshot(),
                                    format!("actor cleanup stuck for {}", spec.name),
                                    || Some(Event::new("CleanupStuck", c, "")),
                                );
                                view.mark_stuck();
                                child.wait_exit_async().await
                            }
                        },
                    );
                }
            }
            Err(error) => view.change(|v| v.snapshot.last_error = Some(error)),
        }
        let startup_reason = |exit: Option<&ExitReason>| {
            if startup_timed_out {
                "timeout"
            } else {
                trace::reason(exit)
            }
        };
        scope.stop(exit.clone().unwrap_or(ExitReason::Shutdown));
        view.change(|v| {
            v.current = None;
            v.snapshot.last_exit = exit.clone();
            // A stuck cleanup already settled the snapshot as Failed (S2).
            if v.snapshot.state != ChildState::Failed {
                v.snapshot.state = ChildState::Stopping;
            }
        });
        fail_point!("child.before_cleanup_join", async);
        let remaining = cleanup_until
            .map(|started| spec.shutdown_timeout.saturating_sub(started.elapsed()))
            .unwrap_or(spec.shutdown_timeout);
        if rt::timeout(remaining, scope.joined()).await.is_err() {
            core.fail_cleanup_for(
                view.snapshot(),
                format!("owned cleanup stuck for {}", spec.name),
                || {
                    if stuck {
                        return None;
                    }
                    if startup_failed {
                        let reason = match scope.child().map(|child| child.exit_reason()) {
                            Some(None) if !startup_timed_out => "unknown",
                            Some(reason) => startup_reason(reason.as_ref()),
                            None => startup_reason(None),
                        };
                        trace::emit(core.trace, || Event::new("StartupFailed", c, reason));
                    }
                    Some(Event::new("CleanupStuck", c, ""))
                },
            );
            view.mark_stuck();
            scope.joined().await;
            trace::atomic(|| {
                trace::tree(core.trace, |id| {
                    TreeEvent::new("TJoined", id).child(c, generation)
                });
                core.cohort(|| Event::new("CJoined", c, "").count(generation as usize));
            });
            break;
        }
        trace::atomic(|| {
            trace::tree(core.trace, |id| {
                TreeEvent::new("TJoined", id).child(c, generation)
            });
            core.cohort(|| Event::new("CJoined", c, "").count(generation as usize));
        });
        if exit.is_none() {
            exit = scope.child().and_then(|child| child.exit_reason());
        }
        if !stuck {
            trace::atomic(|| {
                if startup_failed {
                    trace::emit(core.trace, || {
                        Event::new("StartupFailed", c, startup_reason(exit.as_ref()))
                    });
                }
                trace::emit(core.trace, || Event::new("CleanupDone", c, ""));
            });
        }
        let cleanup_failure = scope.child().and_then(|child| child.cleanup_failure());
        view.change(|v| {
            v.snapshot.last_exit = exit.clone();
            if cleanup_failure.is_some() {
                v.snapshot.last_error = cleanup_failure.clone();
            }
            if v.snapshot.actor_id.is_none() {
                v.snapshot.actor_id = scope.child().map(|child| child.id());
            }
            v.scope = None;
        });
        // A stable generation resets its failure streak even when another
        // child's recovery supersedes this controller during cleanup.
        if scope.was_stable(core.config.backoff.stable_after) {
            failures = 0;
        }
        if core.gate.is_open() && view.recovery(last_recovery).is_some() {
            continue;
        }
        // S1: a child's first start that fails (any cause) aborts the
        // supervisor's start whatever the restart type, as OTP's
        // `start_children` returns `failed_to_start_child`.
        let first_start = ready_at.is_none() && !*view.initial.borrow();
        // A generation that never became ready after the child's initial
        // readiness is a failed restart attempt: retried within the budget
        // whatever its exit reason, as OTP retries a failed restart
        // (`try_again_restart`), unless Temporary.
        let failed_restart = ready_at.is_none() && !first_start;
        let restart = if first_start {
            false
        } else if startup_timed_out || failed_restart {
            spec.restart != RestartMode::Temporary
        } else {
            spec.restart
                .restarts(exit.as_ref(), cleanup_failure.is_some())
        };
        // S1: a ready child's exit during the initial start is recorded and
        // processed once the tree commits ready, as OTP handles the queued
        // 'EXIT' after `init` returns. If the start aborts instead, the
        // exit is part of the rollback. Only the decision waits: cleanup
        // already ran within its deadline.
        let deferred = ready_at.is_some()
            && trace::atomic(|| {
                let defer = core.gate.is_open() && !*core.ready.borrow();
                if defer {
                    trace::emit(core.trace, || Event::new("DeferExit", c, ""));
                }
                defer
            });
        if deferred {
            let _ = select(
                Box::pin(core.admission_closed.cancelled()),
                Box::pin(system::wait_true(core.ready.subscribe())),
            )
            .await;
            trace::emit(core.trace, || Event::new("Undefer", c, ""));
        }
        // Gate check, decision and any resulting failure happen in one step,
        // so a concurrent stop is never followed by a failure of this child.
        let again = trace::atomic(|| {
            if !core.gate.is_open() {
                if !stuck {
                    trace::emit(core.trace, || Event::new("Decide", c, "closed"));
                }
                return false;
            }
            if spec.group.is_closed() {
                core.fail_record_if_open_as(
                    child_failure(
                        &spec,
                        &view,
                        if first_start {
                            FailureKind::FirstStart
                        } else {
                            FailureKind::Other
                        },
                        format!("execution group closed for {}", spec.name),
                    ),
                    || Some(Event::new("Unmodelled", c, "GroupClosed")),
                );
                return false;
            }
            if restart {
                trace::emit(core.trace, || Event::new("Decide", c, "restart"));
                return true;
            }
            if ready_at.is_some() {
                trace::emit(core.trace, || Event::new("Decide", c, "stop"));
            } else {
                let (kind, message) = if first_start {
                    let snapshot = view.snapshot();
                    let cause = snapshot
                        .last_error
                        .clone()
                        .or_else(|| exit.as_ref().map(|reason| format!("exited {reason}")))
                        .unwrap_or_else(|| "unknown".into());
                    (
                        FailureKind::FirstStart,
                        format!("failed to start child {}: {cause}", spec.name),
                    )
                } else {
                    (
                        FailureKind::Other,
                        format!("non-restartable child failed startup: {}", spec.name),
                    )
                };
                core.fail_record_if_open_as(child_failure(&spec, &view, kind, message), || {
                    Some(Event::new("Decide", c, "fail"))
                });
            }
            false
        });
        if !again {
            if core.config.strategy == SupervisorStrategy::OneForOne || !core.gate.is_open() {
                break;
            }
            let failed = system::lock(&core.failure).is_some();
            if trace::atomic(|| {
                let finished = view.finish_unless_recovering(last_recovery, failed);
                if finished {
                    trace::emit(core.trace, || Event::new("Finish", c, ""));
                    core.cohort(|| Event::new("CRetire", c, "").round(last_recovery));
                }
                finished
            }) {
                core.log_failure_once();
                return;
            }
            continue;
        }
        // A failed replacement inside a recovery round requests a new round
        // like any other failure, so OneForAll stops and rebuilds the whole
        // cohort again (and RestForOne the failed child and its later
        // siblings), as OTP's `try_again_restart` re-runs the strategy.
        if core.config.strategy != SupervisorStrategy::OneForOne && *core.ready.borrow() {
            core.request_recovery(view.index, last_recovery);
            if matches!(
                select(
                    Box::pin(core.admission_closed.cancelled()),
                    Box::pin(view.next_recovery(last_recovery))
                )
                .await,
                Either::Left(_)
            ) {
                break;
            }
        }
    }
    // Record Finish and publish completion together, so the supervisor's
    // deadline check sees both or neither.
    trace::atomic(|| {
        trace::emit(core.trace, || Event::new("Finish", c, ""));
        view.finish(system::lock(&core.failure).is_some());
        core.cohort(|| Event::new("CRetire", c, "").round(last_recovery));
    });
    core.log_failure_once();
}

fn child_failure<A: Actor>(
    spec: &ChildSpec<A>,
    view: &View<A>,
    kind: FailureKind,
    message: String,
) -> Failure {
    let mut snapshot = view.snapshot();
    snapshot.name = spec.name.clone();
    snapshot_failure(snapshot, kind, message)
}

fn snapshot_failure(snapshot: ChildSnapshot, kind: FailureKind, message: String) -> Failure {
    Failure::new(
        kind,
        message,
        Some(FailureOrigin {
            child: snapshot.name,
            generation: snapshot.generation,
            reason: snapshot.last_exit,
            last_error: snapshot.last_error,
        }),
    )
}

#[cfg(test)]
mod policy_tests {
    use super::*;
    #[test]
    fn jitter_draw_covers_the_whole_range() {
        // `nanos + 1` would overflow at the top of the range; the full range
        // takes the stream value as is.
        assert_eq!(jitter_draw(u64::MAX, 12345), 12345);
        assert_eq!(jitter_draw(u64::MAX, u64::MAX), u64::MAX);
        assert_eq!(jitter_draw(0, 12345), 0);
        assert_eq!(jitter_draw(9, 12345), 12345 % 10);
        let mut rng = 1;
        assert!(jitter(Duration::MAX, &mut rng) <= Duration::from_nanos(u64::MAX));
        assert_ne!(rng, 1, "each draw advances the stream");
    }
    #[test]
    fn jitter_seed_separates_supervisors_and_siblings() {
        // Kani proves injectivity in `seed`; this keeps the property under
        // `cargo test` (and cargo-mutants), which do not run Kani.
        assert_ne!(jitter_seed(1, b"a"), jitter_seed(2, b"a"));
        assert_ne!(jitter_seed(7, b"left"), jitter_seed(7, b"right"));
        assert_eq!(jitter_seed(7, b""), 7);
        assert_eq!(jitter_seed(0, b"ab"), 97 * 31 + 98);
    }
    #[tokio::test]
    async fn failure_classification_is_typed_and_terminal_failure_is_sticky() {
        let system = ActorSystem::new().unwrap();
        let group = system.async_group("terminal-order", 1).unwrap();
        let mut tree = system.supervisor(SupervisorConfig::default()).unwrap();
        let first = tree
            .add(ChildSpec::new("first", &group, |_| async {
                Ok::<_, String>(CoordinatorWorker)
            }))
            .unwrap();
        let second = tree
            .add(ChildSpec::new("second", &group, |_| async {
                Ok::<_, String>(CoordinatorWorker)
            }))
            .unwrap();
        first.view.finish(true);
        first.view.finish_unstarted(false); // stale non-failed sweep races last
        second.view.finish_unstarted(false);
        second.view.finish(true); // failed controller races last
        assert_eq!(first.snapshot().state, ChildState::Failed);
        assert_eq!(second.snapshot().state, ChildState::Failed);
        tree.core
            .fail("restart budget exhausted by a user-provided message".into());
        assert_eq!(
            system::lock(&tree.core.failure).as_ref().unwrap().kind,
            FailureKind::Other
        );
        drop(tree);
        assert!(system.shutdown(Duration::from_secs(10)).await.complete);
    }
    #[tokio::test]
    async fn initial_placement_failure_has_deterministic_failed_slots() {
        use crate::tasks::ActorStart;
        for _ in 0..8 {
            let system = ActorSystem::new().unwrap();
            let group = system.thread_group("placement-failure", 1).unwrap();
            let occupied = CoordinatorWorker.start_on(&group).unwrap();
            rt::timeout(Duration::from_secs(10), occupied.ready())
                .await
                .unwrap()
                .unwrap();
            let mut tree = system.supervisor(SupervisorConfig::default()).unwrap();
            let first = tree
                .add(ChildSpec::new("first", &group, |_| async {
                    Ok::<_, String>(CoordinatorWorker)
                }))
                .unwrap();
            let second = tree
                .add(ChildSpec::new("second", &group, |_| async {
                    Ok::<_, String>(CoordinatorWorker)
                }))
                .unwrap();
            let core = tree.core.clone();
            assert!(tree.start().await.is_err());
            assert!(rt::timeout(
                Duration::from_secs(10),
                system::wait_true(core.done.subscribe())
            )
            .await
            .unwrap());
            for child in [first, second] {
                assert_eq!(child.snapshot().generation, 0);
                assert_eq!(child.snapshot().state, ChildState::Failed);
                assert!(matches!(
                    child.current(),
                    Err(ChildUnavailable(ChildState::Failed))
                ));
            }
            assert!(system.shutdown(Duration::from_secs(10)).await.complete);
        }
    }
    struct PanicRunner;
    impl Runner for PanicRunner {
        fn run(self: Box<Self>, _: Arc<Core>) -> BoxFuture<'static, ()> {
            panic!("injected supervisor coordinator fault");
        }
    }
    struct CoordinatorWorker;
    impl Actor for CoordinatorWorker {}
    #[tokio::test]
    async fn coordinator_fault_joins_launched_controllers_and_helpers() {
        let system = ActorSystem::new().unwrap();
        let group = system
            .async_group("supervisor-coordinator-fault", 1)
            .unwrap();
        let mut tree = system.supervisor(SupervisorConfig::default()).unwrap();
        let (release, gate) = std::sync::mpsc::channel();
        let gate = Arc::new(Mutex::new(Some(gate)));
        tree.add(
            ChildSpec::new("live", &group, move |ctx| {
                let gate = system::lock(&gate).take().unwrap();
                async move {
                    ctx.spawn_blocking_helper(move || {
                        gate.recv_timeout(Duration::from_secs(10)).unwrap();
                    })
                    .unwrap();
                    Ok::<_, String>(CoordinatorWorker)
                }
            })
            .shutdown_timeout(Duration::from_secs(10)),
        )
        .unwrap();
        tree.add(ChildSpec::new("not-launched", &group, |_| async {
            Ok::<_, String>(CoordinatorWorker)
        }))
        .unwrap();
        tree.runners[1] = Box::new(PanicRunner);
        let core = tree.core.clone();
        // v0.8 (S2): start() returns its error only after the rollback, which
        // waits for the live child's owned helper.
        let start = rt::spawn(tree.start());
        assert!(rt::timeout(
            Duration::from_millis(30),
            system::wait_true(core.done.subscribe())
        )
        .await
        .is_err());
        release.send(()).unwrap();
        assert!(
            matches!(start.await.unwrap(), Err(SupervisorError::Failed(e)) if e.contains("injected supervisor coordinator fault"))
        );
        assert!(rt::timeout(
            Duration::from_secs(10),
            system::wait_true(core.done.subscribe())
        )
        .await
        .unwrap());
        assert!(system::lock(&core.children)
            .iter()
            .all(|child| *child.done().borrow()));
        let report = system.shutdown(Duration::from_secs(10)).await;
        assert!(report.complete);
        assert!(report
            .supervisor_failures
            .iter()
            .any(|e| e.contains("injected supervisor coordinator fault")));
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stop_wins_over_late_child_failure_without_trace_serialization() {
        let system = ActorSystem::new().unwrap();
        let tree = system.supervisor(SupervisorConfig::default()).unwrap();
        tree.core.close();
        assert!(!tree
            .core
            .fail_if_open_as("late child failure".into(), || None));
        assert!(system::lock(&tree.core.failure).is_none());
        assert!(tree
            .core
            .gate
            .if_open(|| tree.core.ready.send_replace(true))
            .is_none());
        assert!(!*tree.core.ready.borrow());
        // Cleanup failures remain observable after ordinary admission closure.
        tree.core.fail_cleanup("late cleanup failure".into());
        assert_eq!(
            system::lock(&tree.core.failure).as_deref(),
            Some("late cleanup failure")
        );
        drop(tree);
        assert!(system.shutdown(Duration::from_secs(5)).await.complete);
    }
    #[test]
    fn delayed_failure_notifications_are_absorbed_by_the_already_assigned_round() {
        let mut queue = RecoveryQueue {
            pending: vec![false; 3],
            covered: vec![0; 3],
            next_id: 1,
        };
        assert!(queue.request(1, 0));
        queue.pending.fill(false);
        // The coordinator publishes coverage before per-child assignment.
        queue.covered[1..].fill(1);
        assert!(!queue.request(2, 0));
        assert!(!queue.pending.iter().any(|value| *value));
        // A new failure in a reconstructed generation must still be admitted.
        assert!(queue.request(2, 1));
        assert!(queue.request(0, 0)); // unaffected upstream in RestForOne
    }

    #[tokio::test]
    async fn admission_closure_after_readiness_preserves_fixed_system_order() {
        use crate::tasks::ActorStart;
        struct Ordered {
            id: u32,
            log: Arc<Mutex<Vec<u32>>>,
            entered: Option<rt::oneshot::Sender<()>>,
            release: Option<rt::oneshot::Receiver<()>>,
        }
        impl Actor for Ordered {
            async fn started(&mut self, ctx: &crate::tasks::Context<Self>) {
                // v0.8: stopped() records the order only for a trapping
                // child (E3, decision 2).
                ctx.trap_exit(true);
            }
            async fn stopped(&mut self, _: &crate::tasks::Context<Self>) {
                system::lock(&self.log).push(self.id);
                if let Some(entered) = self.entered.take() {
                    let _ = entered.send(());
                }
                if let Some(release) = self.release.take() {
                    let _ = release.await;
                }
            }
        }
        let limit = Duration::from_secs(30);
        let system = ActorSystem::new().unwrap();
        let group = system.thread_group("fixed-order-2-1-2", 5).unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        for id in 0..2 {
            Ordered {
                id,
                log: log.clone(),
                entered: None,
                release: None,
            }
            .start_on(&group)
            .unwrap()
            .ready()
            .await
            .unwrap();
        }
        let mut builder = system.supervisor(SupervisorConfig::default()).unwrap();
        let child_log = log.clone();
        builder
            .add(ChildSpec::new("middle", &group, move |_| {
                let log = child_log.clone();
                async move {
                    Ok::<_, String>(Ordered {
                        id: 100,
                        log,
                        entered: None,
                        release: None,
                    })
                }
            }))
            .unwrap();
        let resume = CancellationToken::new();
        let (passed, after_check) = watch::channel(false);
        *system::lock(&builder.core.initial_ready_pause) = Some((resume.clone(), passed));
        let tree = builder.start().await.unwrap();
        Ordered {
            id: 200,
            log: log.clone(),
            entered: None,
            release: None,
        }
        .start_on(&group)
        .unwrap()
        .ready()
        .await
        .unwrap();
        let (entered, arrival) = rt::oneshot::channel();
        let (release, gate) = rt::oneshot::channel();
        Ordered {
            id: 201,
            log: log.clone(),
            entered: Some(entered),
            release: Some(gate),
        }
        .start_on(&group)
        .unwrap()
        .ready()
        .await
        .unwrap();
        let shutdown = system.shutdown(limit);
        tokio::pin!(shutdown);
        assert!(futures::poll!(&mut shutdown).is_pending());
        rt::timeout(limit, arrival).await.unwrap().unwrap();
        // The later standalone actor owns the current stop turn. Resume the
        // supervisor exactly between readiness and its cancellation wait.
        resume.cancel();
        assert!(rt::timeout(limit, system::wait_true(after_check))
            .await
            .unwrap());
        assert!(
            !tree.core.cancel.is_cancelled(),
            "admission closure stole the supervisor's stop turn"
        );
        release.send(()).unwrap();
        assert!(rt::timeout(limit, shutdown).await.unwrap().complete);
        assert_eq!(*system::lock(&log), vec![201, 200, 100, 1, 0]);
    }

    #[tokio::test]
    async fn startup_completion_or_drop_after_readiness_preserves_system_order() {
        use crate::tasks::ActorStart;
        struct Ordered {
            id: u32,
            log: Arc<Mutex<Vec<u32>>>,
            entered: Option<rt::oneshot::Sender<()>>,
            release: Option<rt::oneshot::Receiver<()>>,
        }
        impl Actor for Ordered {
            async fn started(&mut self, ctx: &crate::tasks::Context<Self>) {
                // v0.8: stopped() records the order only for a trapping
                // child (E3, decision 2).
                ctx.trap_exit(true);
            }
            async fn stopped(&mut self, _: &crate::tasks::Context<Self>) {
                system::lock(&self.log).push(self.id);
                if let Some(entered) = self.entered.take() {
                    let _ = entered.send(());
                }
                if let Some(release) = self.release.take() {
                    let _ = release.await;
                }
            }
        }
        for complete_start in [true, false] {
            let limit = Duration::from_secs(30);
            let system = ActorSystem::new().unwrap();
            let group = system.thread_group("fixed-order-2-1-2", 5).unwrap();
            let log = Arc::new(Mutex::new(Vec::new()));
            for id in 0..2 {
                Ordered {
                    id,
                    log: log.clone(),
                    entered: None,
                    release: None,
                }
                .start_on(&group)
                .unwrap()
                .ready()
                .await
                .unwrap();
            }
            let mut builder = system.supervisor(SupervisorConfig::default()).unwrap();
            let child_log = log.clone();
            // Hold construction until start() has been polled once, so the
            // first poll is pending however fast the controller runs.
            let (construct, gate) = rt::oneshot::channel::<()>();
            let gate = Arc::new(Mutex::new(Some(gate)));
            builder
                .add(ChildSpec::new("middle", &group, move |_| {
                    let log = child_log.clone();
                    let gate = system::lock(&gate).take();
                    async move {
                        if let Some(gate) = gate {
                            let _ = gate.await;
                        }
                        Ok::<_, String>(Ordered {
                            id: 100,
                            log,
                            entered: None,
                            release: None,
                        })
                    }
                }))
                .unwrap();
            let core = builder.core.clone();
            let mut startup = Box::pin(builder.start());
            assert!(futures::poll!(startup.as_mut()).is_pending());
            construct.send(()).unwrap();
            assert!(
                rt::timeout(limit, system::wait_true(core.ready.subscribe()))
                    .await
                    .unwrap()
            );
            Ordered {
                id: 200,
                log: log.clone(),
                entered: None,
                release: None,
            }
            .start_on(&group)
            .unwrap()
            .ready()
            .await
            .unwrap();
            let (entered, arrival) = rt::oneshot::channel();
            let (release, gate) = rt::oneshot::channel();
            Ordered {
                id: 201,
                log: log.clone(),
                entered: Some(entered),
                release: Some(gate),
            }
            .start_on(&group)
            .unwrap()
            .ready()
            .await
            .unwrap();
            let shutdown = system.shutdown(limit);
            tokio::pin!(shutdown);
            assert!(futures::poll!(&mut shutdown).is_pending());
            rt::timeout(limit, arrival).await.unwrap().unwrap();
            // Readiness committed, but the caller has not observed start's result.
            // Admission now closes while the later actor still owns the stop turn.
            if complete_start {
                assert!(rt::timeout(limit, startup.as_mut()).await.unwrap().is_ok());
            } else {
                drop(startup);
            }
            assert!(
                !core.cancel.is_cancelled(),
                "completed readiness was rolled back"
            );
            release.send(()).unwrap();
            assert!(rt::timeout(limit, shutdown).await.unwrap().complete);
            assert_eq!(*system::lock(&log), vec![201, 200, 100, 1, 0]);
        }
    }

    #[tokio::test]
    async fn startup_success_remains_historical_after_unobserved_ready_then_failure() {
        struct Idle;
        impl Actor for Idle {}
        let limit = Duration::from_secs(30);
        let system = ActorSystem::new().unwrap();
        let group = system.async_group("historical-ready-failure", 1).unwrap();
        let mut builder = system
            .supervisor(SupervisorConfig {
                max_restarts: 0,
                ..SupervisorConfig::default()
            })
            .unwrap();
        // Hold construction until start() has been polled once, so the
        // first poll is pending however fast the controller runs.
        let (construct, gate) = rt::oneshot::channel::<()>();
        let gate = Arc::new(Mutex::new(Some(gate)));
        let child = builder
            .add(ChildSpec::new("ready-then-failed", &group, move |_| {
                let gate = system::lock(&gate).take();
                async move {
                    if let Some(gate) = gate {
                        let _ = gate.await;
                    }
                    Ok::<_, String>(Idle)
                }
            }))
            .unwrap();
        let core = builder.core.clone();
        let mut startup = Box::pin(builder.start());
        assert!(futures::poll!(startup.as_mut()).is_pending());
        construct.send(()).unwrap();
        assert!(
            rt::timeout(limit, system::wait_true(core.ready.subscribe()))
                .await
                .unwrap()
        );
        child
            .current()
            .unwrap()
            .context()
            .stop_with_reason(ExitReason::Kill);
        assert!(rt::timeout(limit, system::wait_true(core.done.subscribe()))
            .await
            .unwrap());
        let tree = rt::timeout(limit, startup).await.unwrap().unwrap();
        assert!(tree.failure().unwrap().contains("budget exhausted"));
        assert!(system.shutdown(limit).await.complete);
    }

    #[tokio::test]
    async fn started_failures_allow_n_restarts_and_escalate_the_next_failure() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        // v0.8 (S1): a first start is not retried, so the first generation
        // is healthy and every replacement panics in started().
        struct FailsStarted(Arc<AtomicUsize>, bool);
        impl Actor for FailsStarted {
            async fn started(&mut self, _: &crate::tasks::Context<Self>) {
                if self.1 {
                    self.0.fetch_add(1, Ordering::SeqCst);
                    panic!("injected started failure");
                }
            }
        }
        for allowed in [0, 1, 3] {
            let system = ActorSystem::new().unwrap();
            let group = system.async_group("started-budget", 1).unwrap();
            let count = Arc::new(AtomicUsize::new(0));
            let attempts = count.clone();
            let mut builder = system
                .supervisor(SupervisorConfig {
                    max_restarts: allowed,
                    backoff: Backoff {
                        initial: Duration::ZERO,
                        maximum: Duration::ZERO,
                        jitter: false,
                        ..Backoff::default()
                    },
                    ..SupervisorConfig::default()
                })
                .unwrap();
            let child = builder
                .add(ChildSpec::new("startup-failure", &group, move |ctx| {
                    let attempts = attempts.clone();
                    let fail = ctx.generation() > 1;
                    async move { Ok::<_, String>(FailsStarted(attempts, fail)) }
                }))
                .unwrap();
            let core = builder.core.clone();
            rt::timeout(Duration::from_secs(30), builder.start())
                .await
                .unwrap()
                .unwrap();
            child.current().unwrap().child_handle().kill();
            assert!(rt::timeout(
                Duration::from_secs(30),
                system::wait_true(core.done.subscribe())
            )
            .await
            .unwrap());
            assert_eq!(count.load(Ordering::SeqCst), allowed);
            assert_eq!(child.snapshot().generation as usize, allowed + 1);
            assert_eq!(
                system::lock(&core.failure).as_ref().unwrap().kind,
                FailureKind::RestartBudget
            );
            assert_eq!(child.snapshot().state, ChildState::Failed);
            assert!(system.shutdown(Duration::from_secs(30)).await.complete);
        }
    }

    #[tokio::test]
    async fn sliding_window_expires_only_old_grants_before_the_next_retry() {
        let system = ActorSystem::new().unwrap();
        let group = system.async_group("sliding-budget", 1).unwrap();
        let window = Duration::from_secs(3600);
        let mut builder = system
            .supervisor(SupervisorConfig {
                max_restarts: 2,
                restart_window: window,
                backoff: Backoff {
                    initial: Duration::ZERO,
                    maximum: Duration::ZERO,
                    jitter: false,
                    ..Backoff::default()
                },
                ..SupervisorConfig::default()
            })
            .unwrap();
        let child = builder
            .add(ChildSpec::new("worker", &group, |_| async {
                Ok::<_, String>(CoordinatorWorker)
            }))
            .unwrap();
        let core = builder.core.clone();
        let tree = builder.start().await.unwrap();
        let mut actor = child.current().unwrap();
        for generation in 1..=2 {
            actor
                .child_handle()
                .stop_with_reason(ExitReason::Panic("retry".into()));
            actor = rt::timeout(
                Duration::from_secs(30),
                child.wait_for_generation(generation),
            )
            .await
            .unwrap()
            .unwrap();
        }
        {
            let mut budget = system::lock(&core.budget);
            assert_eq!(budget.attempts.len(), 2);
            // Control just the oldest timestamp; the real controller still owns
            // admission and retry. The second grant remains inside the window.
            budget.attempts[0] = Instant::now().checked_sub(window).unwrap();
        }
        actor
            .child_handle()
            .stop_with_reason(ExitReason::Panic("expired oldest".into()));
        let fourth = rt::timeout(Duration::from_secs(30), child.wait_for_generation(3))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(system::lock(&core.budget).attempts.len(), 2);
        fourth
            .child_handle()
            .stop_with_reason(ExitReason::Panic("now exhausted".into()));
        rt::timeout(Duration::from_secs(30), tree.join())
            .await
            .unwrap();
        assert_eq!(child.snapshot().generation, 4);
        assert_eq!(
            system::lock(&core.failure).as_ref().unwrap().kind,
            FailureKind::RestartBudget
        );
        assert!(system.shutdown(Duration::from_secs(30)).await.complete);
    }

    /// OTP charges `add_restart` when it decides a restart, before the
    /// cohort is rebuilt: a superseded round keeps its charge, and the
    /// superseding round is charged once more.
    #[tokio::test]
    async fn supersession_charges_each_selected_round_once() {
        let limit = Duration::from_secs(30);
        let system = ActorSystem::new().unwrap();
        let group = system
            .async_group("refund-superseded-admission", 2)
            .unwrap();
        let mut builder = system
            .supervisor(SupervisorConfig {
                strategy: SupervisorStrategy::RestForOne,
                max_restarts: 3,
                backoff: Backoff {
                    initial: Duration::ZERO,
                    maximum: Duration::ZERO,
                    jitter: false,
                    ..Backoff::default()
                },
                ..SupervisorConfig::default()
            })
            .unwrap();
        let mut children = Vec::new();
        for name in ["upstream", "middle", "tail"] {
            children.push(
                builder
                    .add(ChildSpec::new(name, &group, |_| async {
                        Ok::<_, String>(CoordinatorWorker)
                    }))
                    .unwrap(),
            );
        }
        let core = builder.core.clone();
        let tree = builder.start().await.unwrap();
        let resume = CancellationToken::new();
        let (entered, arrival) = watch::channel(false);
        *system::lock(&core.retry_admission_pause) = Some((resume.clone(), entered));
        children[1]
            .current()
            .unwrap()
            .child_handle()
            .stop_with_reason(ExitReason::Panic("middle fails".into()));
        assert!(rt::timeout(limit, system::wait_true(arrival))
            .await
            .unwrap());
        let old_round = system::lock(&children[1].view.data)
            .recovery
            .as_ref()
            .unwrap()
            .meta
            .id;
        {
            let budget = system::lock(&core.budget);
            // The round was charged once, at selection; its rebuilds hold
            // no reservation of their own.
            assert_eq!(budget.attempts.len(), 1);
            assert_eq!(budget.reserved, 0);
        }
        children[0]
            .current()
            .unwrap()
            .child_handle()
            .stop_with_reason(ExitReason::Panic("upstream supersedes".into()));
        let _new_round = rt::timeout(limit, children[1].view.next_recovery(old_round))
            .await
            .unwrap();
        resume.cancel();
        for child in &children {
            rt::timeout(limit, child.wait_for_generation(1))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(child.snapshot().generation, 2);
        }
        assert!(tree.failure().is_none());
        {
            let budget = system::lock(&core.budget);
            // Two restart actions: the superseded round and its successor,
            // however many children each rebuilt.
            assert_eq!(budget.attempts.len(), 2);
            assert_eq!(budget.reserved, 0);
        }
        assert!(system.shutdown(limit).await.complete);
    }

    #[tokio::test]
    async fn cohort_round_takes_one_grant_for_all_rebuilt_children() {
        let limit = Duration::from_secs(30);
        let system = ActorSystem::new().unwrap();
        let group = system.async_group("cohort-budget-contract", 2).unwrap();
        let mut builder = system
            .supervisor(SupervisorConfig {
                strategy: SupervisorStrategy::OneForAll,
                max_restarts: 1,
                backoff: Backoff {
                    initial: Duration::ZERO,
                    maximum: Duration::ZERO,
                    jitter: false,
                    ..Backoff::default()
                },
                ..SupervisorConfig::default()
            })
            .unwrap();
        let mut children = Vec::new();
        for index in 0..6 {
            children.push(
                builder
                    .add(ChildSpec::new(
                        format!("worker-{index}"),
                        &group,
                        |_| async { Ok::<_, String>(CoordinatorWorker) },
                    ))
                    .unwrap(),
            );
        }
        let core = builder.core.clone();
        let tree = builder.start().await.unwrap();
        children[5]
            .current()
            .unwrap()
            .child_handle()
            .stop_with_reason(ExitReason::Panic("last child fails".into()));
        // One grant rebuilds all six children (one restart action).
        for child in &children {
            rt::timeout(limit, child.wait_for_generation(1))
                .await
                .unwrap()
                .unwrap();
        }
        assert!(tree.failure().is_none());
        assert_eq!(system::lock(&core.budget).attempts.len(), 1);
        children[5]
            .current()
            .unwrap()
            .child_handle()
            .stop_with_reason(ExitReason::Panic("second failure".into()));
        rt::timeout(limit, tree.join()).await.unwrap();
        assert!(tree
            .failure()
            .unwrap()
            .contains("budget exhausted by worker-5"));
        assert_eq!(
            children
                .iter()
                .map(|child| child.snapshot().generation)
                .collect::<Vec<_>>(),
            vec![2; 6]
        );
        assert!(system.shutdown(limit).await.complete);
    }

    #[tokio::test]
    async fn retry_reservations_exclude_siblings_and_refund_interrupted_backoff() {
        let system = ActorSystem::new().unwrap();
        let tree = system
            .supervisor(SupervisorConfig {
                max_restarts: 1,
                ..SupervisorConfig::default()
            })
            .unwrap();
        let reservation = tree.core.reserve_retry(1).unwrap();
        assert!(tree.core.reserve_retry(2).is_none());
        assert_eq!(system::lock(&tree.core.budget).attempts.len(), 0);
        drop(reservation);
        let mut replacement = tree.core.reserve_retry(2).unwrap();
        tree.core.close();
        assert!(!trace::atomic(|| tree.core.retry(&mut replacement)));
        drop(replacement);
        {
            let budget = system::lock(&tree.core.budget);
            assert!(budget.attempts.is_empty());
            assert_eq!(budget.reserved, 0);
        }
        drop(tree);
        assert!(system.shutdown(Duration::from_secs(10)).await.complete);
    }

    #[test]
    fn retry_window_releases_capacity_at_boundary_and_does_not_bank_unused_retries() {
        let now = Instant::now();
        let mut budget = RetryBudget {
            attempts: VecDeque::new(),
            reserved: 0,
            max: 2,
            window: Duration::from_secs(10),
        };
        assert!(budget.take(now));
        assert!(budget.take(now + Duration::from_secs(1)));
        assert!(!budget.take(now + Duration::from_secs(9)));
        assert!(budget.take(now + Duration::from_secs(10)));
        assert!(!budget.take(now + Duration::from_secs(10)));
        assert!(budget.take(now + Duration::from_secs(100)));
        assert!(budget.take(now + Duration::from_secs(100)));
        assert!(!budget.take(now + Duration::from_secs(100)));
    }
    #[test]
    fn capped_backoff_and_seeded_jitter_remain_bounded_after_long_failure_streaks() {
        let mut policy = Backoff {
            initial: Duration::from_millis(10),
            maximum: Duration::from_millis(35),
            jitter: false,
            ..Backoff::default()
        };
        let mut seed = 7;
        assert_eq!(
            [0, 1, 2, 32, u32::MAX].map(|n| policy.delay(n, &mut seed)),
            [10, 20, 35, 35, 35].map(Duration::from_millis)
        );
        policy.jitter = true;
        let mut a = 42;
        let mut b = 42;
        let first: Vec<_> = (0..100).map(|n| policy.delay(n, &mut a)).collect();
        let replay: Vec<_> = (0..100).map(|n| policy.delay(n, &mut b)).collect();
        assert_eq!(first, replay);
        assert!(first.iter().all(|d| *d <= policy.maximum));
        assert!(first.windows(2).any(|pair| pair[0] != pair[1]));
    }
}
