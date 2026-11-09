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
    time::{Duration, Instant},
};

mod nested;
pub use nested::{SupervisedSupervisor, SupervisorSpec};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartMode {
    Permanent,
    Transient,
    Temporary,
}
impl RestartMode {
    fn restarts(self, reason: Option<&ExitReason>) -> bool {
        match self {
            Self::Permanent => true,
            Self::Transient => reason.map(ExitReason::is_abnormal).unwrap_or(true),
            Self::Temporary => false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Backoff {
    pub initial: Duration,
    pub maximum: Duration,
    pub stable_after: Duration,
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
    fn delay(&self, failures: u32, rng: &mut u64) -> Duration {
        let bound = self
            .initial
            .saturating_mul(1u32 << failures.min(31))
            .min(self.maximum);
        if !self.jitter {
            return bound;
        }
        *rng = rng
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let nanos = bound.as_nanos().min(u64::MAX as u128) as u64;
        Duration::from_nanos(if nanos == u64::MAX {
            *rng
        } else {
            *rng % (nanos + 1)
        })
    }
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
    #[error("supervisor stopped before startup completed")]
    Stopped,
    #[error("supervisor failed: {0}")]
    Failed(String),
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
}
impl<A: Actor> ChildSpec<A> {
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
        }
    }
    pub fn restart(mut self, restart: RestartMode) -> Self {
        self.restart = restart;
        self
    }
    pub fn startup_timeout(mut self, timeout: Duration) -> Self {
        self.startup_timeout = timeout;
        self
    }
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
struct RecoveryQueue {
    pending: Vec<bool>,
    covered: Vec<u64>,
    next_id: u64,
}
impl RecoveryQueue {
    fn request(&mut self, index: usize, after: u64) -> bool {
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
    changed: watch::Sender<u64>,
    initial: watch::Sender<bool>,
    stop_turn: CancellationToken,
    done: watch::Sender<bool>,
    shutdown_timeout: Duration,
    /// 1-based declaration index: the model's child id in lifecycle traces.
    trace_index: usize,
}
impl<A: Actor> View<A> {
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
            data.snapshot.state = if failed {
                ChildState::Failed
            } else {
                ChildState::Stopped
            };
        }
        self.changed.send_modify(|n| *n = n.wrapping_add(1));
        self.done.send_replace(true);
        true
    }
    fn finish(&self, failed: bool) {
        self.change(|data| {
            data.current = None;
            data.scope = None;
            data.snapshot.state = if failed {
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
            .unwrap_or(Err(ChildUnavailable(ChildState::Stopping)))
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
                return Err(ChildUnavailable(ChildState::Stopping));
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
    fn finish_unstarted(&self);
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
    fn stop_generation(&self, slot: &RecoverySlot) {
        slot.stop.cancel();
        let scope = system::lock(&self.data).scope.clone();
        if let Some(scope) = scope {
            scope.stop(ExitReason::Shutdown);
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
    fn stop(&self) {
        self.stop_turn.cancel();
        if let Some(scope) = system::lock(&self.data).scope.clone() {
            scope.stop(ExitReason::Shutdown);
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
    fn finish_unstarted(&self) {
        if self.snapshot().generation == 0 {
            self.finish(false);
        }
    }
}

struct RetryBudget {
    attempts: VecDeque<Instant>,
    max: usize,
    window: Duration,
}
impl RetryBudget {
    fn take(&mut self, now: Instant) -> bool {
        while self
            .attempts
            .front()
            .is_some_and(|t| now.saturating_duration_since(*t) >= self.window)
        {
            self.attempts.pop_front();
        }
        if self.attempts.len() >= self.max {
            return false;
        }
        self.attempts.push_back(now);
        true
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
    failure: Mutex<Option<String>>,
    cleanup_failure: Mutex<Option<String>>,
    children: Mutex<Vec<Arc<dyn ChildControl>>>,
    budget: Mutex<RetryBudget>,
    config: SupervisorConfig,
    trace: TraceId,
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
    fn fail(&self, error: String) {
        let mut failure = system::lock(&self.failure);
        if failure.is_none() {
            *failure = Some(error);
        }
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
        let mut failure = system::lock(&self.cleanup_failure);
        if failure.is_none() {
            *failure = Some(error.to_string());
        }
    }
    fn fail_cleanup(&self, error: String) {
        self.record_cleanup_failure(&error);
        self.fail(error);
    }
    fn fail_cleanup_as(&self, error: String, event: impl FnOnce() -> Option<Event>) {
        self.record_cleanup_failure(&error);
        self.fail_as(error, event);
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
    fn fail_as(&self, error: String, event: impl FnOnce() -> Option<Event>) {
        trace::failing(|| {
            trace::emit_some(self.trace, event);
            self.fail(error);
        });
    }
    /// A concurrent ordinary stop wins over a later child failure. The gate
    /// lock, rather than optional trace instrumentation, commits this decision.
    fn fail_if_open_as(&self, error: String, event: impl FnOnce() -> Option<Event>) -> bool {
        trace::failing(|| {
            let committed = self.gate.close_if_open(|| {
                trace::emit_some(self.trace, event);
                self.cohort(|| Event::sup("CClose"));
                trace::tree(self.trace, |id| TreeEvent::new("TClose", id));
                let mut failure = system::lock(&self.failure);
                if failure.is_none() {
                    *failure = Some(error);
                }
            });
            if committed {
                self.close();
            }
            committed
        })
    }
    /// Records under the gate lock: callers must already be inside a
    /// `trace::atomic` section (lock order is section, then gate).
    fn retry(&self, child: usize) -> bool {
        self.gate
            .if_open(|| {
                let mut budget = system::lock(&self.budget);
                let taken = budget.take(Instant::now());
                if taken {
                    trace::emit(self.trace, || {
                        Event::new("BackoffDone", child, "retry").count(budget.attempts.len())
                    });
                }
                taken
            })
            .unwrap_or(false)
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
        system::lock(&self.failure).clone()
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
                core.fail_as(
                    format!(
                        "child controller panicked: {}",
                        crate::error::panic_message(&*payload)
                    ),
                    || {
                        Some(Event::new(
                            "Unmodelled",
                            view.trace_index,
                            "ControllerPanicked",
                        ))
                    },
                );
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
            cleanup_failure: Mutex::new(None),
            children: Mutex::new(Vec::new()),
            budget: Mutex::new(RetryBudget {
                attempts: VecDeque::new(),
                max: config.max_restarts,
                window: config.restart_window,
            }),
            config,
            trace: TraceId::next(),
            #[cfg(feature = "lifecycle-trace")]
            deadline_recorded: Default::default(),
        });
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
            changed,
            initial,
            done,
            stop_turn: CancellationToken::new(),
            shutdown_timeout: spec.shutdown_timeout,
            trace_index: children.len() + 1,
        });
        children.push(view.clone());
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

    /// Start children in declaration order. Cancellation of this startup future
    /// requests cleanup; it does not leave an orphaned background tree.
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
        let result = match select(Box::pin(cancelled), Box::pin(system::wait_true(ready))).await {
            Either::Right((true, _)) if core.gate.is_open() => {
                guard.armed = false;
                Ok(Supervisor { core: core.clone() })
            }
            _ => Err(system::lock(&core.failure)
                .clone()
                .map(SupervisorError::Failed)
                .unwrap_or(SupervisorError::Stopped)),
        };
        result
    }
}
impl Drop for SupervisorBuilder {
    fn drop(&mut self) {
        if !self.started {
            self.core.close();
            for child in system::lock(&self.core.children).iter() {
                child.finish_unstarted();
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
            self.core.close();
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
        system::lock(&self.core.failure).clone()
    }
    pub async fn join(&self) {
        let _ = system::wait_true(self.core.done.subscribe()).await;
    }
    pub async fn shutdown(&self, deadline: Duration) -> SupervisorReport {
        self.stop();
        let complete = rt::timeout(deadline, system::wait_true(self.core.done.subscribe()))
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
    let children = system::lock(&core.children).clone();
    let mut tasks = Vec::new();
    let coordination = if core.config.strategy == SupervisorStrategy::OneForOne {
        None
    } else {
        let coordinator = core.clone();
        Some(rt::spawn(async move {
            if let Err(payload) = AssertUnwindSafe(run_recoveries(coordinator.clone()))
                .catch_unwind()
                .await
            {
                coordinator.fail(format!(
                    "recovery coordinator panicked: {}",
                    crate::error::panic_message(&*payload)
                ));
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
        child.finish_unstarted();
    }
    trace::atomic(|| {
        let committed = core.gate.if_open(|| {
            trace::emit(core.trace, || Event::new("SupStartDone", 0, "true"));
            core.cohort(|| Event::sup("CInitialReady"));
            core.ready.send_replace(true);
        });
        if committed.is_none() {
            trace::emit(core.trace, || Event::new("SupStartDone", 0, "false"));
        }
    });
    if !core.gate.is_open() {
        core.close();
    }
    core.cancel.cancelled().await;
    trace::emit(core.trace, || Event::sup("SupCancelSeen"));
    for (index, child) in children.iter().enumerate().rev() {
        let c = index + 1;
        trace::emit(core.trace, || Event::new("SupStopTurn", c, ""));
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
                core.fail_cleanup_as(
                    format!("child cleanup deadline exceeded: {}", child.snapshot().name),
                    || Some(Event::new("SupChildTimeout", c, "")),
                );
            }
        });
    }
    for task in tasks {
        if let Err(error) = task.await {
            core.fail_as(format!("child controller failed: {error}"), || {
                Some(Event::new("Unmodelled", 0, "ControllerFailed"))
            });
        }
    }
    if let Some(coordination) = coordination {
        if let Err(error) = coordination.await {
            core.fail(format!("recovery coordinator failed: {error}"));
        }
    }
    trace::atomic(|| {
        trace::emit(core.trace, || Event::sup("SupJoin"));
        core.cohort(|| Event::sup("CDone"));
        trace::tree(core.trace, |id| TreeEvent::new("TDone", id));
        core.done.send_replace(true);
    });
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
            queue
                .pending
                .iter()
                .position(|pending| *pending)
                .map(|origin| {
                    let start = if core.config.strategy == SupervisorStrategy::OneForAll {
                        0
                    } else {
                        origin
                    };
                    for pending in &mut queue.pending[start..] {
                        *pending = false;
                    }
                    let id = queue.next_id;
                    queue.next_id = id.checked_add(1).expect("recovery IDs exhausted");
                    queue.covered[start..].fill(id);
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
                    (start, meta)
                })
        });
        let Some((start, meta)) = round else {
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
                    core.fail_cleanup(format!(
                        "coordinated cleanup stuck for {}",
                        child.snapshot().name
                    ));
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
    let mut failures = 0u32;
    let seed = core.config.backoff.seed.unwrap_or_else(|| {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64
            ^ NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    });
    let mut rng = seed
        ^ system::lock(&view.data)
            .snapshot
            .name
            .bytes()
            .fold(0u64, |a, b| a.wrapping_mul(31).wrapping_add(b as u64));
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
                    return;
                }
                continue;
            }
            collateral = round.meta.origin != view.index;
        }
        if !first {
            if core.config.max_restarts == 0 {
                // A concurrent stop wins: only fail a supervisor that is still open.
                trace::atomic(|| {
                    if core.gate.is_open() {
                        core.fail_if_open_as(
                            format!("restart budget exhausted by {}", spec.name),
                            || Some(Event::new("BackoffDone", c, "exhausted")),
                        );
                    } else {
                        trace::emit(core.trace, || Event::new("BackoffDone", c, "closed"));
                    }
                });
                break;
            }
            let delay = if collateral {
                Duration::ZERO
            } else {
                core.config.backoff.delay(failures, &mut rng)
            };
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
                trace::emit(core.trace, || Event::new("BackoffDone", c, "closed"));
                break;
            }
            if view.recovery(last_recovery).is_some() {
                continue;
            }
            if count_attempt {
                failures = failures.saturating_add(1);
            }
            let retried = trace::atomic(|| {
                let retried = core.retry(c);
                if !retried {
                    if core.gate.is_open() {
                        core.fail_if_open_as(
                            format!("restart budget exhausted by {}", spec.name),
                            || {
                                let held = system::lock(&core.budget).attempts.len();
                                Some(Event::new("BackoffDone", c, "exhausted").count(held))
                            },
                        );
                    } else {
                        trace::emit(core.trace, || Event::new("BackoffDone", c, "closed"));
                    }
                }
                retried
            });
            if !retried {
                break;
            }
        }
        first = false;
        let (scope, lease) = match spec.group.reserve(Some(core.gate.clone())) {
            Ok(attempt) => attempt,
            Err(e) => {
                trace::atomic(|| {
                    if core.gate.is_open() {
                        core.fail_if_open_as(
                            format!("placement failed for {}: {e}", spec.name),
                            || Some(Event::new("Unmodelled", c, "PlacementFailed")),
                        );
                    } else {
                        trace::emit(core.trace, || Event::new("Attempt", c, "false"));
                    }
                });
                break;
            }
        };
        let generation = trace::atomic(|| {
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
                    data.snapshot.generation += 1;
                    data.snapshot.state = ChildState::Starting;
                    data.snapshot.actor_id = None;
                    data.current = None;
                    data.scope = Some(scope.clone());
                    Some(data.snapshot.generation)
                })
                .flatten();
            trace::emit(core.trace, || {
                Event::new("Attempt", c, trace::flag(generation.is_some()))
            });
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
        view.changed.send_modify(|n| *n = n.wrapping_add(1));
        let ctx = FactoryContext {
            scope: scope.clone(),
            generation,
        };
        let factory = spec.factory.clone();
        let attempt_scope = scope.clone();
        let construction = async move {
            if !attempt_scope.can_start() {
                return Err("scope closed before construction".into());
            }
            let factory_work = AssertUnwindSafe(async move { factory(ctx).await }).catch_unwind();
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
                        scope.stop(ExitReason::Shutdown);
                    }
                    view.change(|v| {
                        v.current = None;
                        v.snapshot.state = ChildState::Stopping;
                    });
                    let cleanup_started = Instant::now();
                    cleanup_until = Some(cleanup_started);
                    exit = Some(
                        match rt::timeout(spec.shutdown_timeout, child.wait_exit_async()).await {
                            Ok(reason) => {
                                if !stop_turn {
                                    trace::emit(core.trace, || {
                                        Event::new("SelfExit", c, trace::reason(Some(&reason)))
                                    });
                                }
                                reason
                            }
                            Err(_) => {
                                if !stop_turn {
                                    trace::emit(core.trace, || {
                                        Event::new("SelfExit", c, "unknown")
                                    });
                                }
                                stuck = true;
                                core.fail_cleanup_as(
                                    format!("actor cleanup stuck for {}", spec.name),
                                    || Some(Event::new("CleanupStuck", c, "")),
                                );
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
            v.snapshot.state = ChildState::Stopping;
        });
        let remaining = cleanup_until
            .map(|started| spec.shutdown_timeout.saturating_sub(started.elapsed()))
            .unwrap_or(spec.shutdown_timeout);
        if rt::timeout(remaining, scope.joined()).await.is_err() {
            core.fail_cleanup_as(format!("owned cleanup stuck for {}", spec.name), || {
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
            });
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
        view.change(|v| {
            v.snapshot.last_exit = exit.clone();
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
        let restart = if startup_timed_out {
            spec.restart != RestartMode::Temporary
        } else {
            spec.restart.restarts(exit.as_ref())
        };
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
                core.fail_if_open_as(format!("execution group closed for {}", spec.name), || {
                    Some(Event::new("Unmodelled", c, "GroupClosed"))
                });
                return false;
            }
            if restart {
                if core.config.strategy != SupervisorStrategy::OneForOne
                    && !*core.ready.borrow()
                    && ready_at.is_some()
                {
                    // A dependency already published ready but died before the
                    // initial tree committed. Roll back the whole startup rather
                    // than leave later factories bound to its obsolete instance.
                    core.fail_if_open_as(
                        format!(
                            "ready child exited during initial tree startup: {}",
                            spec.name
                        ),
                        || Some(Event::new("Unmodelled", c, "InitialCohortFailed")),
                    );
                    return false;
                }
                trace::emit(core.trace, || Event::new("Decide", c, "restart"));
                return true;
            }
            if ready_at.is_some() {
                trace::emit(core.trace, || Event::new("Decide", c, "stop"));
            } else {
                core.fail_if_open_as(
                    format!("non-restartable child failed startup: {}", spec.name),
                    || Some(Event::new("Decide", c, "fail")),
                );
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
                return;
            }
            continue;
        }
        let in_initial_cohort = system::lock(&view.data).recovery.as_ref().is_some_and(|r| {
            r.meta.id == last_recovery
                && !*r.ready.borrow()
                && !r.meta.finished.load(std::sync::atomic::Ordering::Acquire)
        });
        if core.config.strategy != SupervisorStrategy::OneForOne
            && *core.ready.borrow()
            && !in_initial_cohort
        {
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
}

#[cfg(test)]
mod policy_tests {
    use super::*;
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

    #[test]
    fn retry_window_releases_capacity_at_boundary_and_does_not_bank_unused_retries() {
        let now = Instant::now();
        let mut budget = RetryBudget {
            attempts: VecDeque::new(),
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
