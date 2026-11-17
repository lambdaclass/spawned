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

mod child;
pub use child::*;
mod state;
pub(crate) use state::*;
mod builder;
pub use builder::*;
mod run;
use run::*;

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

/// Restart delay policy. Non-exhaustive: start from `Backoff::default()` (or
/// `new()`) and chain the setters, or assign the public fields of a value.
#[derive(Debug, Clone)]
#[non_exhaustive]
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
    /// The default policy: 10 ms initial cap, 5 s maximum, stable after
    /// 30 s, jitter on, no fixed seed.
    pub fn new() -> Self {
        Self::default()
    }
    /// Set [`initial`](Self::initial).
    pub fn initial(mut self, initial: Duration) -> Self {
        self.initial = initial;
        self
    }
    /// Set [`maximum`](Self::maximum).
    pub fn maximum(mut self, maximum: Duration) -> Self {
        self.maximum = maximum;
        self
    }
    /// Set [`stable_after`](Self::stable_after).
    pub fn stable_after(mut self, stable_after: Duration) -> Self {
        self.stable_after = stable_after;
        self
    }
    /// Set [`jitter`](Self::jitter).
    pub fn jitter(mut self, jitter: bool) -> Self {
        self.jitter = jitter;
        self
    }
    /// Seed the jitter for reproducible tests.
    pub fn seed(mut self, seed: u64) -> Self {
        self.seed = Some(seed);
        self
    }

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
#[non_exhaustive]
pub enum SupervisorStrategy {
    #[default]
    OneForOne,
    OneForAll,
    RestForOne,
}

/// Supervisor flags (OTP's `strategy`, `intensity` and `period`, plus a
/// child cap and a restart backoff). Non-exhaustive: start from
/// `SupervisorConfig::default()` (or `new()`) and chain the setters,
/// `SupervisorConfig::new().strategy(SupervisorStrategy::OneForAll).max_restarts(3)`,
/// or assign the public fields of a value.
#[derive(Debug, Clone)]
#[non_exhaustive]
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
impl SupervisorConfig {
    /// OneForOne, 5 restarts per 30 s, 1024 children, the default backoff.
    pub fn new() -> Self {
        Self::default()
    }
    /// Set [`strategy`](Self::strategy).
    pub fn strategy(mut self, strategy: SupervisorStrategy) -> Self {
        self.strategy = strategy;
        self
    }
    /// Set [`max_restarts`](Self::max_restarts) (OTP's intensity).
    pub fn max_restarts(mut self, max_restarts: usize) -> Self {
        self.max_restarts = max_restarts;
        self
    }
    /// Set [`restart_window`](Self::restart_window) (OTP's period).
    pub fn restart_window(mut self, restart_window: Duration) -> Self {
        self.restart_window = restart_window;
        self
    }
    /// Set [`max_children`](Self::max_children).
    pub fn max_children(mut self, max_children: usize) -> Self {
        self.max_children = max_children;
        self
    }
    /// Set [`backoff`](Self::backoff).
    pub fn backoff(mut self, backoff: Backoff) -> Self {
        self.backoff = backoff;
        self
    }
}

#[derive(Debug, Clone, thiserror::Error)]
#[non_exhaustive]
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
#[non_exhaustive]
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
#[non_exhaustive]
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

#[cfg(test)]
mod guard_tests;
#[cfg(test)]
mod policy_tests;
