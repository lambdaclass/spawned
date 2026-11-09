//! Owned execution groups and lifecycle-owned helper scopes.
//!
//! Ordinary `.start()` remains ambient and unmanaged. Explicit group admission
//! owns actors/helpers and keeps their runtime alive until managed cleanup ends.
use crate::{
    child_handle::ChildHandle,
    error::ExitReason,
    link,
    tasks::{Actor, ActorRef},
};
use futures::{
    future::{select, BoxFuture, Either},
    FutureExt,
};
use spawned_rt::tasks::{self as rt, watch, CancellationToken, Handle, JoinHandle};
use std::{
    collections::BTreeMap,
    future::Future,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, Weak,
    },
    time::{Duration, Instant},
};
mod group;
use group::GroupInner;

/// Resource models are opt-in; thread pools reserve one native thread per live
/// actor, while pinned shards multiplex actors cooperatively on fixed CPUs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutionProfile {
    AsyncPool {
        workers: usize,
    },
    ThreadPool {
        workers: usize,
        helper_workers: usize,
    },
    PinnedShards {
        cpus: Vec<usize>,
    },
}
impl ExecutionProfile {
    fn validate(&self) -> Result<(), SystemError> {
        if !(1..=1024).contains(&self.workers())
            || matches!(self, Self::ThreadPool { helper_workers, .. } if !(1..=1024).contains(helper_workers))
        {
            return Err(SystemError::InvalidWorkerBudget);
        }
        if let Self::PinnedShards { cpus } = self {
            let allowed =
                rt::affinity::available_cpus().map_err(|e| SystemError::Affinity(e.to_string()))?;
            let mut seen = std::collections::BTreeSet::new();
            if cpus
                .iter()
                .any(|cpu| !allowed.contains(cpu) || !seen.insert(*cpu))
            {
                return Err(SystemError::Affinity(
                    "CPUs must be unique members of the caller's allowed affinity mask".into(),
                ));
            }
        }
        Ok(())
    }
    fn workers(&self) -> usize {
        match self {
            Self::AsyncPool { workers } | Self::ThreadPool { workers, .. } => *workers,
            Self::PinnedShards { cpus } => cpus.len(),
        }
    }
}

static NEXT_ADMISSION: AtomicU64 = AtomicU64::new(1);
fn next_admission() -> Result<u64, SystemError> {
    let mut current = NEXT_ADMISSION.load(Ordering::Relaxed);
    loop {
        let next = current.checked_add(1).ok_or(SystemError::Closed)?;
        match NEXT_ADMISSION.compare_exchange_weak(
            current,
            next,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return Ok(current),
            Err(observed) => current = observed,
        }
    }
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum SystemError {
    #[error("no free persistent actor thread; stop/join a scope before admission")]
    Capacity,
    #[error("invalid shard index or profile without explicit shards")]
    InvalidShard,
    #[error("CPU affinity configuration failed: {0}")]
    Affinity(String),
    #[error("system, group or actor scope is closed")]
    Closed,
    #[error("name must be nonempty and contain no NUL bytes")]
    InvalidName,
    #[error("worker budget must be between 1 and 1024")]
    InvalidWorkerBudget,
    #[error("name already exists: {0}")]
    DuplicateName(String),
    #[error("runtime construction failed: {0}")]
    Runtime(String),
    #[error("execution group belongs to a different actor system")]
    ForeignGroup,
    #[error("this actor has no managed helper scope; use start_on")]
    UnmanagedActor,
    #[error("actor stopped before becoming ready: {0}")]
    StartupFailed(ExitReason),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HelperError {
    #[error("owned helper was cancelled before completion")]
    Cancelled,
    #[error("owned helper panicked: {0}")]
    Panicked(String),
}

/// A registered helper's join handle. Dropping it does not detach ownership:
/// its scope still cancels and joins the helper before cleanup completes.
pub struct ScopedTask<T>(JoinHandle<Result<T, HelperError>>);
impl<T> ScopedTask<T> {
    pub async fn join(self) -> Result<T, HelperError> {
        match self.0.await {
            Ok(result) => result,
            Err(error) if error.is_cancelled() => Err(HelperError::Cancelled),
            Err(error) => Err(HelperError::Panicked(error.to_string())),
        }
    }
    pub fn abort(&self) {
        self.0.abort();
    }
}

pub(crate) fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

pub(crate) struct Admission(Mutex<bool>);
impl Admission {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self(Mutex::new(true)))
    }
    pub(crate) fn close(&self) {
        *lock(&self.0) = false;
    }
    pub(crate) fn is_open(&self) -> bool {
        *lock(&self.0)
    }
    /// Commit a terminal decision and close admission under the same lock.
    /// The callback must not reenter admission or perform shutdown cleanup.
    pub(crate) fn close_if_open(&self, f: impl FnOnce()) -> bool {
        let mut open = lock(&self.0);
        if !*open {
            return false;
        }
        f();
        *open = false;
        true
    }
    /// Atomically preserve a historical commitment or close the gate. Callbacks
    /// cannot reenter admission; `before_close` runs only for an open gate.
    pub(crate) fn close_unless(
        &self,
        preserve: impl FnOnce() -> bool,
        before_close: impl FnOnce(),
    ) -> bool {
        let mut open = lock(&self.0);
        if preserve() {
            return false;
        }
        if *open {
            before_close();
            *open = false;
        }
        true
    }
    pub(crate) fn if_open<R>(&self, f: impl FnOnce() -> R) -> Option<R> {
        let open = lock(&self.0);
        (*open).then(f)
    }
}

pub(crate) struct Control {
    pub(crate) handle: Handle,
    close: CancellationToken,
    done: watch::Receiver<bool>,
}
impl Control {
    fn new() -> Result<Self, SystemError> {
        let close = CancellationToken::new();
        let closing = close.clone();
        let (done_tx, done) = watch::channel(false);
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("spawned-control".into())
            .spawn(move || {
                let built = std::panic::catch_unwind(rt::control_runtime);
                match built {
                    Ok(Ok(runtime)) => {
                        if tx.send(Ok(runtime.handle().clone())).is_ok() {
                            runtime.block_on(closing.cancelled());
                        }
                        drop(runtime);
                    }
                    Ok(Err(e)) => {
                        let _ = tx.send(Err(e.to_string()));
                    }
                    Err(_) => {
                        let _ = tx.send(Err("control runtime panicked".into()));
                    }
                }
                done_tx.send_replace(true);
            })
            .map_err(|e| SystemError::Runtime(e.to_string()))?;
        let handle = rx
            .recv()
            .map_err(|e| SystemError::Runtime(e.to_string()))?
            .map_err(SystemError::Runtime)?;
        Ok(Self {
            handle,
            close,
            done,
        })
    }
}
impl Drop for Control {
    fn drop(&mut self) {
        self.close.cancel();
    }
}

pub(crate) trait ShutdownUnit: Send + Sync {
    fn close_admission(&self);
    fn stop(&self);
    fn stop_remaining(&self);
    fn joined(&self) -> BoxFuture<'_, ()>;
    fn is_done(&self) -> bool;
    fn failure(&self) -> Option<String>;
}

#[derive(Clone)]
struct ShutdownUnitEntry {
    id: u64,
    unit: Arc<dyn ShutdownUnit>,
}
enum ShutdownTarget {
    Unit(ShutdownUnitEntry),
    Scope(Arc<Scope>),
}
impl ShutdownTarget {
    fn id(&self) -> u64 {
        match self {
            Self::Unit(entry) => entry.id,
            Self::Scope(scope) => scope.id,
        }
    }
}
struct SystemState {
    closing: bool,
    groups: Vec<Arc<GroupInner>>,
    units: Vec<ShutdownUnitEntry>,
    shutdown_scopes: Vec<Arc<Scope>>,
    coordinator_failures: Vec<String>,
}
pub(crate) struct SystemInner {
    state: Mutex<SystemState>,
    pub(crate) control: Control,
}

/// Owns runtime groups and managed lifecycle trees. Drop requests shutdown;
/// explicitly await shutdown to observe cleanup and unfinished work.
pub struct ActorSystem {
    pub(crate) inner: Arc<SystemInner>,
}
impl ActorSystem {
    pub fn new() -> Result<Self, SystemError> {
        Ok(Self {
            inner: Arc::new(SystemInner {
                state: Mutex::new(SystemState {
                    closing: false,
                    groups: Vec::new(),
                    units: Vec::new(),
                    shutdown_scopes: Vec::new(),
                    coordinator_failures: Vec::new(),
                }),
                control: Control::new()?,
            }),
        })
    }

    /// Construct a parked pool with an explicit async worker budget. Construction
    /// is synchronous/fallible; call it during application setup.
    pub fn async_group(
        &self,
        name: impl Into<String>,
        workers: usize,
    ) -> Result<ExecutionGroup, SystemError> {
        self.add_group(name.into(), ExecutionProfile::AsyncPool { workers })
    }
    /// Admit managed actors on an existing runtime, without creating or owning
    /// its worker pool. The caller must keep that runtime alive and driven until
    /// system shutdown completes. Shutdown joins managed roots/helpers, but does
    /// not stop the shared runtime or ambient tasks started outside the group.
    /// The reported worker count describes the shared pool, not reserved capacity.
    pub fn async_group_on(
        &self,
        name: impl Into<String>,
        runtime: &Handle,
    ) -> Result<ExecutionGroup, SystemError> {
        self.add_group_with_runtime(
            name.into(),
            ExecutionProfile::AsyncPool {
                workers: runtime.metrics().num_workers(),
            },
            Some(runtime.clone()),
        )
    }

    /// Fixed native actor roots sharing a sized async helper runtime. Each live
    /// scope reserves a worker even while idle; full admission returns Capacity.
    pub fn thread_group(
        &self,
        name: impl Into<String>,
        workers: usize,
    ) -> Result<ExecutionGroup, SystemError> {
        self.thread_group_with_helpers(name, workers, 1)
    }
    /// Fixed native actor roots with an explicit async helper-worker budget.
    /// Both worker budgets must be between 1 and 1024.
    pub fn thread_group_with_helpers(
        &self,
        name: impl Into<String>,
        workers: usize,
        helper_workers: usize,
    ) -> Result<ExecutionGroup, SystemError> {
        self.add_group(
            name.into(),
            ExecutionProfile::ThreadPool {
                workers,
                helper_workers,
            },
        )
    }
    /// Parked single-thread runtime shards, pinned to explicit allowed Linux CPUs.
    /// Use group.shard(i) to retain pipeline placement across actor generations.
    pub fn pinned_group(
        &self,
        name: impl Into<String>,
        cpus: Vec<usize>,
    ) -> Result<ExecutionGroup, SystemError> {
        self.add_group(name.into(), ExecutionProfile::PinnedShards { cpus })
    }
    fn add_group(
        &self,
        name: String,
        profile: ExecutionProfile,
    ) -> Result<ExecutionGroup, SystemError> {
        self.add_group_with_runtime(name, profile, None)
    }
    fn add_group_with_runtime(
        &self,
        name: String,
        profile: ExecutionProfile,
        runtime: Option<Handle>,
    ) -> Result<ExecutionGroup, SystemError> {
        if name.is_empty() || name.contains('\0') {
            return Err(SystemError::InvalidName);
        }
        profile.validate()?;
        let mut system = lock(&self.inner.state);
        if system.closing {
            return Err(SystemError::Closed);
        }
        if system.groups.iter().any(|g| g.name == name) {
            return Err(SystemError::DuplicateName(name));
        }
        let group = GroupInner::new(name, profile, runtime)?;
        let result = ExecutionGroup {
            inner: Arc::downgrade(&group),
            selection: None,
        };
        system.groups.push(group);
        Ok(result)
    }

    /// Close admission and request ordered shutdown. At expiry the report owns
    /// pending shutdown state, which can be awaited again. Blocking Rust is not
    /// forcibly killed and runtime destruction never blocks this caller thread.
    pub async fn shutdown(&self, deadline: Duration) -> ShutdownReport {
        self.inner.begin_shutdown(deadline);
        ShutdownReport {
            pending: Some(self.inner.clone()),
            complete: false,
            unfinished_groups: Vec::new(),
            unfinished_actors: Vec::new(),
            unfinished_actor_details: Vec::new(),
            exits: Vec::new(),
            supervisor_failures: Vec::new(),
            group_failures: Vec::new(),
            coordinator_failures: Vec::new(),
        }
        .wait(deadline)
        .await
    }

    pub fn supervisor(
        &self,
        config: crate::supervision::SupervisorConfig,
    ) -> Result<crate::supervision::SupervisorBuilder, crate::supervision::SupervisorError> {
        crate::supervision::SupervisorBuilder::new(self, config)
    }
}
impl Drop for ActorSystem {
    fn drop(&mut self) {
        self.inner.begin_shutdown(Duration::from_secs(5));
    }
}

impl SystemInner {
    pub(crate) fn accepts_group(&self, group: &ExecutionGroup) -> Result<(), SystemError> {
        let state = lock(&self.state);
        if state.closing {
            return Err(SystemError::Closed);
        }
        let group = group.inner.upgrade().ok_or(SystemError::Closed)?;
        if state.groups.iter().any(|owned| Arc::ptr_eq(owned, &group)) {
            Ok(())
        } else {
            Err(SystemError::ForeignGroup)
        }
    }
    pub(crate) fn register_unit(&self, unit: Arc<dyn ShutdownUnit>) -> Result<(), SystemError> {
        let mut state = lock(&self.state);
        if state.closing {
            return Err(SystemError::Closed);
        }
        state.units.retain(|entry| !entry.unit.is_done());
        state.units.push(ShutdownUnitEntry {
            id: next_admission()?,
            unit,
        });
        Ok(())
    }
    fn begin_shutdown(self: &Arc<Self>, grace: Duration) {
        let began = Instant::now();
        let (groups, units, scopes) = {
            // Keep initialization serialized with repeated shutdown calls so
            // even a zero-time report sees the admitted-scope snapshot.
            let mut state = lock(&self.state);
            if state.closing {
                return;
            }
            state.closing = true;
            let groups = state.groups.clone();
            let units = state.units.clone();
            for entry in &units {
                entry.unit.close_admission();
            }
            for group in &groups {
                group.close_admission();
            }
            let mut scopes: Vec<_> = groups
                .iter()
                .flat_map(|g| lock(&g.state).scopes.values().cloned().collect::<Vec<_>>())
                .collect();
            scopes.sort_by_key(|s| s.id);
            state.shutdown_scopes = scopes.clone();
            (groups, units, scopes)
        };
        let control_close = self.control.close.clone();
        // The coordinator holds ownership while cleanup is pending. Logical
        // group/actor handles never own this runtime or this system.
        let owner = self.clone();
        let mut targets: Vec<_> = units.iter().cloned().map(ShutdownTarget::Unit).collect();
        targets.extend(
            scopes
                .iter()
                .filter(|scope| scope.parent.is_none())
                .cloned()
                .map(ShutdownTarget::Scope),
        );
        targets.sort_by_key(ShutdownTarget::id);
        self.control.handle.spawn(async move {
            let ordered = std::panic::AssertUnwindSafe(async {
                // Keep reverse admission order across standalone actors and trees.
                // At the initial shutdown deadline, signal remaining targets rather
                // than leaving earlier dependencies running behind stuck cleanup.
                for target in targets.iter().rev() {
                    fail_point!("system.shutdown_before_target", async);
                    let remaining = grace.saturating_sub(began.elapsed());
                    match target {
                        ShutdownTarget::Unit(entry) => {
                            entry.unit.stop();
                            if rt::timeout(remaining, entry.unit.joined()).await.is_err() {
                                entry.unit.stop_remaining();
                            }
                        }
                        ShutdownTarget::Scope(scope) => {
                            scope.stop(ExitReason::Shutdown);
                            let _ = rt::timeout(remaining, scope.joined()).await;
                        }
                    }
                }
                // At grace expiry, no nested child may remain unsignalled behind
                // a stuck sibling's longer per-child cleanup timeout.
                for scope in scopes.iter().rev() {
                    scope.stop(ExitReason::Shutdown);
                }
                // Runtime teardown still requires actual completion, independently
                // of the ordering grace and the caller's report timeout.
                for entry in &units {
                    entry.unit.joined().await;
                }
                for scope in &scopes {
                    scope.joined().await;
                }
                for group in groups.iter().rev() {
                    wait_true(group.done.clone()).await;
                }
            })
            .catch_unwind()
            .await;
            if let Err(payload) = ordered {
                let message = crate::error::panic_message(&*payload);
                crate::mailbox::dispose_panic(payload);
                owner.record_coordinator_failure(format!(
                    "system shutdown coordinator panicked: {message}"
                ));
                owner.emergency_shutdown(&groups, &units, &scopes).await;
            }
            drop(owner);
            control_close.cancel();
        });
    }
    fn record_coordinator_failure(&self, message: String) {
        let mut state = lock(&self.state);
        if state.coordinator_failures.len() < 16 && !state.coordinator_failures.contains(&message) {
            state.coordinator_failures.push(message);
        }
    }
    async fn emergency_shutdown(
        &self,
        groups: &[Arc<GroupInner>],
        units: &[ShutdownUnitEntry],
        scopes: &[Arc<Scope>],
    ) {
        loop {
            let result = std::panic::AssertUnwindSafe(async {
                let mut signalled = true;
                for entry in units.iter().rev() {
                    if entry.unit.is_done() {
                        continue;
                    }
                    if let Err(payload) =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            entry.unit.stop_remaining()
                        }))
                    {
                        let message = crate::error::panic_message(&*payload);
                        crate::mailbox::dispose_panic(payload);
                        self.record_coordinator_failure(format!(
                            "emergency supervisor stop panicked: {message}"
                        ));
                        signalled = false;
                    }
                }
                for scope in scopes.iter().rev() {
                    if let Err(payload) =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            scope.stop(ExitReason::Shutdown)
                        }))
                    {
                        let message = crate::error::panic_message(&*payload);
                        crate::mailbox::dispose_panic(payload);
                        self.record_coordinator_failure(format!(
                            "emergency actor stop panicked: {message}"
                        ));
                        signalled = false;
                    }
                }
                for entry in units {
                    if rt::timeout(Duration::from_millis(10), entry.unit.joined())
                        .await
                        .is_err()
                    {
                        signalled = false;
                    }
                }
                for scope in scopes {
                    if rt::timeout(Duration::from_millis(10), scope.joined())
                        .await
                        .is_err()
                    {
                        signalled = false;
                    }
                }
                if !signalled {
                    return false;
                }
                for group in groups.iter().rev() {
                    wait_true(group.done.clone()).await;
                }
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
                    self.record_coordinator_failure(format!("emergency join panicked: {message}"));
                }
            }
            // Keep ownership and a visible failure if an internal stop keeps
            // failing. Reports remain incomplete; retry without a busy loop.
            rt::sleep(Duration::from_millis(10)).await;
        }
    }
}

/// Incomplete shutdown retains ownership, not permission to admit more work.
pub struct ShutdownReport {
    pending: Option<Arc<SystemInner>>,
    pub complete: bool,
    pub unfinished_groups: Vec<String>,
    /// Unfinished actor scopes, including registered helpers after actor exit.
    /// An ID may also appear in exits when only its helper cleanup remains.
    pub unfinished_actors: Vec<crate::ActorId>,
    /// Named/type diagnostics for the same unfinished scopes. Message type and
    /// turn age require the opt-in `diagnostics` feature; no payloads are retained.
    pub unfinished_actor_details: Vec<ActorDiagnostic>,
    pub exits: Vec<ActorExit>,
    pub supervisor_failures: Vec<String>,
    pub group_failures: Vec<String>,
    /// Coordinator panics recovered through emergency stop-and-join.
    pub coordinator_failures: Vec<String>,
}
#[derive(Debug, Clone)]
pub struct ActorExit {
    pub id: crate::ActorId,
    pub reason: ExitReason,
}
/// A point-in-time view; the actor may finish immediately after the snapshot.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ActorDiagnostic {
    pub id: crate::ActorId,
    /// ChildSpec name, or the first successful managed registration name.
    pub name: Option<String>,
    pub actor_type: &'static str,
    /// Active envelope type, absent when idle, in lifecycle hooks, or when
    /// `diagnostics` is disabled. Payloads are never inspected.
    pub message_type: Option<&'static str>,
    pub turn_age: Option<Duration>,
}
impl ShutdownReport {
    pub async fn wait(mut self, deadline: Duration) -> Self {
        let Some(owner) = self.pending.as_ref() else {
            return self;
        };
        self.complete = rt::timeout(deadline, wait_true(owner.control.done.clone()))
            .await
            .unwrap_or(false);
        let state = lock(&owner.state);
        self.coordinator_failures = state.coordinator_failures.clone();
        self.unfinished_groups = state
            .groups
            .iter()
            .filter(|g| !*g.done.borrow())
            .map(|g| g.name.clone())
            .collect();
        self.unfinished_actor_details = state
            .shutdown_scopes
            .iter()
            .filter(|s| !*s.done.borrow())
            .filter_map(|s| s.diagnostic())
            .collect();
        self.unfinished_actors = self.unfinished_actor_details.iter().map(|d| d.id).collect();
        self.exits = state
            .shutdown_scopes
            .iter()
            .filter_map(|s| {
                let child = s.child()?;
                Some(ActorExit {
                    id: child.id(),
                    reason: child.exit_reason()?,
                })
            })
            .collect();
        self.supervisor_failures = state
            .units
            .iter()
            .filter_map(|entry| entry.unit.failure())
            .collect();
        self.group_failures = state
            .groups
            .iter()
            .flat_map(|group| {
                lock(&group.failures)
                    .iter()
                    .map(|error| format!("{}: {error}", group.name))
                    .collect::<Vec<_>>()
            })
            .collect();
        self.complete &= self.unfinished_groups.is_empty();
        drop(state);
        if self.complete {
            self.pending = None;
        }
        self
    }
}

pub(crate) async fn wait_true(mut rx: watch::Receiver<bool>) -> bool {
    loop {
        if *rx.borrow_and_update() {
            return true;
        }
        if rx.changed().await.is_err() {
            return false;
        }
    }
}

struct GroupState {
    closing: bool,
    retiring: bool,
    scopes: BTreeMap<u64, Arc<Scope>>,
    free: Option<std::collections::BTreeSet<usize>>,
}

/// A non-owning placement capability. Cloning it cannot retain running workers
/// after its system has successfully shut down.
#[derive(Clone)]
pub struct ExecutionGroup {
    pub(crate) inner: Weak<GroupInner>,
    selection: Option<usize>,
}
impl std::fmt::Debug for ExecutionGroup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecutionGroup")
            .field("name", &self.name())
            .finish()
    }
}
impl ExecutionGroup {
    pub fn name(&self) -> Option<String> {
        self.inner.upgrade().map(|g| g.name.clone())
    }
    /// Async worker count, native actor-slot count, or pinned shard count.
    /// Excludes helper, owner, join-coordinator and system-control threads.
    pub fn worker_budget(&self) -> Option<usize> {
        self.inner.upgrade().map(|g| g.profile.workers())
    }
    pub fn profile(&self) -> Option<ExecutionProfile> {
        self.inner.upgrade().map(|g| g.profile.clone())
    }
    /// Select a pinned runtime shard or persistent native actor-thread slot.
    /// Shared multi-thread async pools have no stable worker-index placement.
    pub fn shard(&self, index: usize) -> Result<Self, SystemError> {
        let group = self.inner.upgrade().ok_or(SystemError::Closed)?;
        if matches!(group.profile, ExecutionProfile::AsyncPool { .. })
            || index >= group.shard_count()
        {
            return Err(SystemError::InvalidShard);
        }
        if lock(&group.state).closing {
            return Err(SystemError::Closed);
        }
        Ok(Self {
            inner: self.inner.clone(),
            selection: Some(index),
        })
    }
    pub fn is_closed(&self) -> bool {
        self.inner
            .upgrade()
            .map(|g| lock(&g.state).closing)
            .unwrap_or(true)
    }
    pub fn start<A: Actor>(&self, actor: A) -> Result<ActorRef<A>, SystemError> {
        let (scope, lease) = self.reserve(None)?;
        ActorRef::spawn_managed(actor, scope, lease)
    }
    pub(crate) fn reserve(
        &self,
        parent: Option<Arc<Admission>>,
    ) -> Result<(Arc<Scope>, Lease), SystemError> {
        fail_point!("system.reserve" => return Err(SystemError::Capacity));
        let _parent = parent.as_ref().map(|p| lock(&p.0));
        if _parent.as_deref() == Some(&false) {
            return Err(SystemError::Closed);
        }
        let group = self.inner.upgrade().ok_or(SystemError::Closed)?;
        let mut state = lock(&group.state);
        if state.closing {
            return Err(SystemError::Closed);
        }
        let id = next_admission()?;
        let target = group.choose(&mut state, self.selection)?;
        let (ready, _) = watch::channel(StartupState::Pending);
        let (done, _) = watch::channel(false);
        let scope = Arc::new(Scope {
            id,
            group: Arc::downgrade(&group),
            handle: group.handle(target),
            target,
            selection: self.selection,
            parent: parent.clone(),
            token: CancellationToken::new(),
            cause: crate::link::new_linked_exit_reason(),
            state: Mutex::new(ScopeState {
                open: true,
                leases: 1,
                roots: 1,
                actor: None,
                name: None,
                actor_type: "",
                registrations: Vec::new(),
                ready_at: None,
                ended_at: None,
            }),
            #[cfg(feature = "diagnostics")]
            active_turn: Mutex::new(None),
            ready,
            done,
        });
        state.scopes.insert(id, scope.clone());
        Ok((scope.clone(), Lease { scope, root: true }))
    }
}

#[derive(Debug, Clone)]
pub(crate) enum StartupState {
    Pending,
    Ready,
    Failed(ExitReason),
}
struct ScopeState {
    open: bool,
    leases: usize,
    roots: usize,
    actor: Option<ChildHandle>,
    name: Option<String>,
    actor_type: &'static str,
    registrations: Vec<crate::registry::RegistrationGuard>,
    ready_at: Option<Instant>,
    ended_at: Option<Instant>,
}
pub(crate) struct Scope {
    id: u64,
    target: usize,
    selection: Option<usize>,
    group: Weak<GroupInner>,
    pub(crate) handle: Handle,
    parent: Option<Arc<Admission>>,
    pub(crate) token: CancellationToken,
    pub(crate) cause: crate::link::LinkedExitReason,
    state: Mutex<ScopeState>,
    #[cfg(feature = "diagnostics")]
    active_turn: Mutex<Option<(&'static str, Instant)>>,
    ready: watch::Sender<StartupState>,
    done: watch::Sender<bool>,
}
impl Scope {
    pub(crate) fn group(&self) -> ExecutionGroup {
        ExecutionGroup {
            inner: self.group.clone(),
            // Native slots are exclusively leased by the current scope. New
            // actors may use another free slot; pinned shards retain affinity.
            selection: self.group.upgrade().and_then(|group| {
                matches!(group.profile, ExecutionProfile::PinnedShards { .. })
                    .then_some(self.selection)
                    .flatten()
            }),
        }
    }
    pub(crate) fn ready_rx(&self) -> watch::Receiver<StartupState> {
        self.ready.subscribe()
    }
    pub(crate) async fn wait_ready(&self) -> Result<(), SystemError> {
        let mut ready = self.ready_rx();
        loop {
            let state = ready.borrow_and_update().clone();
            match state {
                StartupState::Ready => return Ok(()),
                StartupState::Failed(reason) => return Err(SystemError::StartupFailed(reason)),
                StartupState::Pending => (),
            }
            if ready.changed().await.is_err() {
                return Err(SystemError::Closed);
            }
        }
    }
    pub(crate) async fn joined(&self) {
        let _ = wait_true(self.done.subscribe()).await;
    }
    pub(crate) fn stop(&self, reason: ExitReason) {
        {
            let mut state = lock(&self.state);
            state.open = false;
            if state.ended_at.is_none() {
                state.ended_at = Some(Instant::now());
            }
        }
        link::stop_with_reason(&self.cause, reason, || self.token.cancel());
    }
    pub(crate) fn close_admission(&self) {
        lock(&self.state).open = false;
    }
    pub(crate) fn register(
        &self,
        name: &str,
        id: crate::ActorId,
        actor: impl Clone + Send + Sync + 'static,
    ) -> Result<(), crate::registry::ManagedRegistrationError> {
        fail_point!("registry.before_register" => {
            return Err(crate::registry::ManagedRegistrationError::OwnerClosed)
        });
        let parent = self.parent.as_ref().map(|p| lock(&p.0));
        let Some(group) = self.group.upgrade() else {
            return Err(crate::registry::ManagedRegistrationError::OwnerClosed);
        };
        let group = lock(&group.state);
        let mut state = lock(&self.state);
        if parent.as_deref() == Some(&false)
            || group.closing
            || !state.open
            || self.token.is_cancelled()
        {
            return Err(crate::registry::ManagedRegistrationError::OwnerClosed);
        }
        state
            .registrations
            .push(crate::registry::register_owned(name, id, actor)?);
        if state.name.is_none() {
            state.name = Some(name.to_owned());
        }
        Ok(())
    }
    pub(crate) fn set_name(&self, name: &str) {
        lock(&self.state).name = Some(name.to_owned());
    }
    pub(crate) fn attach<A>(&self, child: ChildHandle) {
        let mut state = lock(&self.state);
        state.actor = Some(child);
        state.actor_type = std::any::type_name::<A>();
    }
    fn diagnostic(&self) -> Option<ActorDiagnostic> {
        let state = lock(&self.state);
        #[cfg(feature = "diagnostics")]
        let (message_type, turn_age) = match *lock(&self.active_turn) {
            Some((message, start)) => (Some(message), Some(start.elapsed())),
            None => (None, None),
        };
        #[cfg(not(feature = "diagnostics"))]
        let (message_type, turn_age) = (None, None);
        Some(ActorDiagnostic {
            id: state.actor.as_ref()?.id(),
            name: state.name.clone(),
            actor_type: state.actor_type,
            message_type,
            turn_age,
        })
    }
    #[cfg(feature = "diagnostics")]
    pub(crate) fn begin_turn(&self, message: &'static str) -> TurnGuard<'_> {
        *lock(&self.active_turn) = Some((message, Instant::now()));
        TurnGuard(self)
    }
    pub(crate) fn child(&self) -> Option<ChildHandle> {
        lock(&self.state).actor.clone()
    }
    pub(crate) fn can_start(&self) -> bool {
        let _parent = self.parent.as_ref().map(|p| lock(&p.0));
        if _parent.as_deref() == Some(&false) {
            return false;
        }
        let Some(g) = self.group.upgrade() else {
            return false;
        };
        let group = lock(&g.state);
        let scope = lock(&self.state);
        !group.closing && scope.open && !self.token.is_cancelled()
    }
    pub(crate) fn mark_ready(&self) -> bool {
        let _parent = self.parent.as_ref().map(|p| lock(&p.0));
        if _parent.as_deref() == Some(&false) {
            return false;
        }
        let Some(g) = self.group.upgrade() else {
            return false;
        };
        let group = lock(&g.state);
        let mut scope = lock(&self.state);
        if group.closing || !scope.open || self.token.is_cancelled() {
            return false;
        }
        scope.ready_at = Some(Instant::now());
        self.ready.send_replace(StartupState::Ready);
        true
    }
    pub(crate) fn end_running(&self) {
        let mut state = lock(&self.state);
        if state.ended_at.is_none() {
            state.ended_at = Some(Instant::now());
        }
    }
    pub(crate) fn was_stable(&self, interval: Duration) -> bool {
        let state = lock(&self.state);
        match (state.ready_at, state.ended_at) {
            (Some(ready), Some(end)) => end.saturating_duration_since(ready) >= interval,
            _ => false,
        }
    }
    pub(crate) fn exited(&self, reason: ExitReason) {
        self.stop(reason.clone());
        let pending = matches!(*self.ready.borrow(), StartupState::Pending);
        if pending {
            self.ready.send_replace(StartupState::Failed(reason));
        }
    }
    fn lease(self: &Arc<Self>, root: bool) -> Result<Lease, SystemError> {
        let parent = self.parent.as_ref().map(|p| lock(&p.0));
        if parent.as_deref() == Some(&false) {
            return Err(SystemError::Closed);
        }
        let Some(g) = self.group.upgrade() else {
            return Err(SystemError::Closed);
        };
        let group = lock(&g.state);
        let mut state = lock(&self.state);
        if group.closing || !state.open || self.token.is_cancelled() {
            return Err(SystemError::Closed);
        }
        state.leases = state.leases.checked_add(1).ok_or(SystemError::Closed)?;
        if root {
            state.roots += 1;
        }
        Ok(Lease {
            scope: self.clone(),
            root,
        })
    }
    pub(crate) fn actor_lease(self: &Arc<Self>) -> Result<Lease, SystemError> {
        self.lease(true)
    }
    /// Dispatch the actor root to its reserved worker or runtime shard.
    pub(crate) fn dispatch_actor<F>(&self, future: F) -> Result<(), SystemError>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.group
            .upgrade()
            .ok_or(SystemError::Closed)?
            .dispatch(self.target, future)
    }
    /// Internal lifecycle cleanup is deliberately not cancelled with ordinary
    /// helpers: its lease must survive until the owned subtree actually joins.
    pub(crate) fn track_lifecycle<F>(self: &Arc<Self>, future: F) -> Result<(), SystemError>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let lease = self.lease(false)?;
        drop(self.handle.spawn(Tracked { future, lease }));
        Ok(())
    }
    pub(crate) fn spawn_framework<F>(self: &Arc<Self>, future: F) -> JoinHandle<()>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        match self.lease(false) {
            Ok(lease) => {
                let token = self.token.clone();
                self.handle.spawn(Tracked {
                    future: async move {
                        let _ = select(Box::pin(token.cancelled()), Box::pin(future)).await;
                    },
                    lease,
                })
            }
            Err(_) => {
                drop(future);
                let task = self.handle.spawn(async {});
                task.abort();
                task
            }
        }
    }
    pub(crate) fn spawn_helper<F>(
        self: &Arc<Self>,
        future: F,
    ) -> Result<ScopedTask<F::Output>, SystemError>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let lease = self.lease(false)?;
        let token = self.token.clone();
        let work = async move {
            match select(
                Box::pin(token.cancelled()),
                Box::pin(std::panic::AssertUnwindSafe(future).catch_unwind()),
            )
            .await
            {
                Either::Left(_) => Err(HelperError::Cancelled),
                Either::Right((Ok(value), _)) => Ok(value),
                Either::Right((Err(payload), _)) => Err(HelperError::Panicked(
                    crate::error::panic_message(&*payload),
                )),
            }
        };
        Ok(ScopedTask(self.handle.spawn(Tracked {
            future: work,
            lease,
        })))
    }

    pub(crate) fn spawn_blocking_helper<F, T>(
        self: &Arc<Self>,
        work: F,
    ) -> Result<ScopedTask<T>, SystemError>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let lease = self.lease(false)?;
        let token = self.token.clone();
        let owned = BlockingWork { work, lease };
        Ok(ScopedTask(
            self.handle.spawn_blocking(move || owned.run(token)),
        ))
    }
}

#[cfg(feature = "diagnostics")]
pub(crate) struct TurnGuard<'a>(&'a Scope);
#[cfg(feature = "diagnostics")]
impl Drop for TurnGuard<'_> {
    fn drop(&mut self) {
        *lock(&self.0.active_turn) = None;
    }
}

// Keep work and lease captured together, so cancellation of a queued blocking
// helper destroys its payload before releasing scope ownership.
struct BlockingWork<F> {
    work: F,
    lease: Lease,
}
impl<F, T> BlockingWork<F>
where
    F: FnOnce() -> T,
{
    fn run(self, token: CancellationToken) -> Result<T, HelperError> {
        let Self {
            work,
            lease: _lease,
        } = self;
        if token.is_cancelled() {
            drop(work);
            return Err(HelperError::Cancelled);
        }
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(work))
            .map_err(|p| HelperError::Panicked(crate::error::panic_message(&*p)))
    }
}

pub(crate) struct Lease {
    scope: Arc<Scope>,
    root: bool,
}
impl Drop for Lease {
    fn drop(&mut self) {
        let (last, roots_done, registrations) = {
            let mut state = lock(&self.scope.state);
            state.leases -= 1;
            if self.root {
                state.roots -= 1;
            }
            let roots_done = state.roots == 0;
            if roots_done {
                state.open = false;
            }
            let registrations = if state.leases == 0 {
                std::mem::take(&mut state.registrations)
            } else {
                Vec::new()
            };
            (state.leases == 0, roots_done, registrations)
        };
        if roots_done {
            self.scope.token.cancel();
            let pending = matches!(*self.scope.ready.borrow(), StartupState::Pending);
            if pending {
                let reason = self
                    .scope
                    .child()
                    .and_then(|c| c.exit_reason())
                    .or_else(|| lock(&self.scope.cause).clone())
                    .unwrap_or(ExitReason::Kill);
                self.scope.ready.send_replace(StartupState::Failed(reason));
            }
        }
        if last {
            drop(registrations);
            if let Some(g) = self.scope.group.upgrade() {
                g.remove_scope(self.scope.id);
            }
            self.scope.done.send_replace(true);
        }
    }
}

pin_project_lite::pin_project! {
    /// Future payload drops before its scope lease, including never-polled tasks.
    pub(crate) struct Tracked<F> {
        #[pin]
        pub(crate) future: F,
        pub(crate) lease: Lease,
    }
}
impl<F: Future> Future for Tracked<F> {
    type Output = F::Output;
    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        self.project().future.poll(cx)
    }
}

#[cfg(test)]
mod coordinator_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    const LIMIT: Duration = Duration::from_secs(10);
    struct Worker(Arc<AtomicUsize>);
    impl Actor for Worker {
        async fn stopped(&mut self, _: &crate::tasks::Context<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    struct FaultUnit {
        allow_emergency: AtomicBool,
        done: watch::Sender<bool>,
    }
    impl ShutdownUnit for FaultUnit {
        fn close_admission(&self) {}
        fn stop(&self) {
            panic!("injected system coordinator fault");
        }
        fn stop_remaining(&self) {
            assert!(
                self.allow_emergency.load(Ordering::SeqCst),
                "injected emergency stop fault"
            );
            self.done.send_replace(true);
        }
        fn joined(&self) -> BoxFuture<'_, ()> {
            Box::pin(async move {
                wait_true(self.done.subscribe()).await;
            })
        }
        fn is_done(&self) -> bool {
            *self.done.borrow()
        }
        fn failure(&self) -> Option<String> {
            None
        }
    }
    #[tokio::test]
    async fn coordinator_fault_stops_all_targets_and_retains_owned_cleanup() {
        use crate::tasks::ActorStart;
        let system = ActorSystem::new().unwrap();
        let group = system.async_group("coordinator-fault", 1).unwrap();
        let stopped = Arc::new(AtomicUsize::new(0));
        let first = Worker(stopped.clone()).start_on(&group).unwrap();
        let second = Worker(stopped.clone()).start_on(&group).unwrap();
        first.ready().await.unwrap();
        second.ready().await.unwrap();
        let (done, _) = watch::channel(false);
        let fault = Arc::new(FaultUnit {
            allow_emergency: AtomicBool::new(false),
            done,
        });
        system.inner.register_unit(fault.clone()).unwrap();
        let mut report = system.shutdown(Duration::from_millis(30)).await;
        assert!(!report.complete);
        // The report is a snapshot, not an acknowledgement that the coordinator
        // ran. Under full-suite load its 30ms deadline can precede the fault.
        // Refresh with bounded, yielding waits until that diagnostic is visible.
        report = rt::timeout(LIMIT, async move {
            while !report
                .coordinator_failures
                .iter()
                .any(|e| e.contains("injected system coordinator fault"))
            {
                // Yield even if a regressed coordinator closes its done watch:
                // wait() can then return immediately, and LIMIT must still fire.
                rt::sleep(Duration::from_millis(1)).await;
                report = report.wait(Duration::ZERO).await;
            }
            report
        })
        .await
        .expect("coordinator did not report its injected failure");
        assert!(report
            .coordinator_failures
            .iter()
            .any(|e| e.contains("injected system coordinator fault")));
        tokio::time::timeout(LIMIT, first.join()).await.unwrap();
        tokio::time::timeout(LIMIT, second.join()).await.unwrap();
        assert_eq!(stopped.load(Ordering::SeqCst), 2);
        assert!(!*system.inner.control.done.borrow());
        drop(system); // pending report + coordinator retain runtime ownership
        fault.allow_emergency.store(true, Ordering::SeqCst);
        let report = report.wait(LIMIT).await;
        assert!(report.complete);
        assert!(!report.coordinator_failures.is_empty());
        assert!(report.coordinator_failures.len() <= 16);
    }
    #[tokio::test]
    async fn emergency_cleanup_does_not_resignal_an_already_finished_faulting_unit() {
        let system = ActorSystem::new().unwrap();
        let (done, _) = watch::channel(false);
        let fault = Arc::new(FaultUnit {
            allow_emergency: AtomicBool::new(false),
            done,
        });
        system.inner.register_unit(fault.clone()).unwrap();
        let report = system.shutdown(Duration::from_millis(30)).await;
        assert!(!report.complete);
        // Another owner finishes it; its stop method remains permanently broken.
        fault.done.send_replace(true);
        let report = report.wait(LIMIT).await;
        assert!(report.complete);
        assert!(!report.coordinator_failures.is_empty());
    }
}
