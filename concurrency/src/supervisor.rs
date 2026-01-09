//! Supervision trees for automatic process restart and fault tolerance.
//!
//! This module provides OTP-style supervision for managing child processes.
//! Supervisors monitor their children and can automatically restart them
//! according to a configured strategy.
//!
//! # Example
//!
//! ```ignore
//! use spawned_concurrency::supervisor::{Supervisor, SupervisorSpec, ChildSpec, RestartStrategy};
//! use spawned_concurrency::Backend;
//!
//! let spec = SupervisorSpec::new(RestartStrategy::OneForOne)
//!     .max_restarts(3, std::time::Duration::from_secs(5))
//!     .child(ChildSpec::worker("worker", || WorkerServer::new().start(Backend::Async)));
//!
//! let mut supervisor = Supervisor::start(spec);
//! ```

use crate::link::{MonitorRef, SystemMessage};
use crate::pid::{ExitReason, HasPid, Pid};
use crate::{
    Backend, CallResponse, CastResponse, GenServer, GenServerHandle, InitResult,
};
use crate::gen_server::InfoResponse;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Strategy for restarting children when one fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartStrategy {
    /// Restart only the failed child.
    /// Other children are unaffected.
    OneForOne,

    /// Restart all children when one fails.
    /// Children are restarted in the order they were defined.
    OneForAll,

    /// Restart the failed child and all children started after it.
    /// Earlier children are unaffected.
    RestForOne,
}

/// Policy for when a child should be restarted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RestartType {
    /// Always restart the child when it exits.
    #[default]
    Permanent,

    /// Restart only if the child exits abnormally.
    Transient,

    /// Never restart the child.
    Temporary,
}

/// Child shutdown behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shutdown {
    /// Wait indefinitely for the child to terminate.
    Infinity,

    /// Wait up to the specified duration, then force kill.
    Timeout(Duration),

    /// Immediately force kill the child.
    Brutal,
}

impl Default for Shutdown {
    fn default() -> Self {
        Shutdown::Timeout(Duration::from_secs(5))
    }
}

/// Tracks restart intensity to prevent restart storms.
///
/// Records restart timestamps and checks if more restarts are allowed
/// within the configured time window.
#[derive(Debug, Clone)]
pub struct RestartIntensityTracker {
    /// Maximum restarts allowed within the time window.
    max_restarts: u32,
    /// Time window for counting restarts.
    max_seconds: Duration,
    /// Timestamps of recent restarts.
    restart_times: Vec<Instant>,
}

impl RestartIntensityTracker {
    /// Create a new tracker with the given limits.
    pub fn new(max_restarts: u32, max_seconds: Duration) -> Self {
        Self {
            max_restarts,
            max_seconds,
            restart_times: Vec::new(),
        }
    }

    /// Record that a restart occurred.
    pub fn record_restart(&mut self) {
        self.restart_times.push(Instant::now());
    }

    /// Check if another restart is allowed within intensity limits.
    ///
    /// Prunes old restart times and returns true if under the limit.
    pub fn can_restart(&mut self) -> bool {
        let cutoff = Instant::now() - self.max_seconds;
        self.restart_times.retain(|t| *t > cutoff);
        (self.restart_times.len() as u32) < self.max_restarts
    }
}

/// Type of child process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChildType {
    /// A regular worker process.
    #[default]
    Worker,

    /// A supervisor process (for nested supervision trees).
    Supervisor,
}

/// Trait for child handles that can be supervised.
///
/// This provides a type-erased interface for managing child processes,
/// allowing the supervisor to work with any GenServer type.
pub trait ChildHandle: Send + Sync {
    /// Get the process ID of this child.
    fn pid(&self) -> Pid;

    /// Request graceful shutdown of this child.
    fn shutdown(&self);

    /// Check if this child is still alive.
    fn is_alive(&self) -> bool;
}

/// Implementation of ChildHandle for GenServerHandle.
impl<G: GenServer + 'static> ChildHandle for GenServerHandle<G> {
    fn pid(&self) -> Pid {
        HasPid::pid(self)
    }

    fn shutdown(&self) {
        self.cancellation_token().cancel();
    }

    fn is_alive(&self) -> bool {
        !self.cancellation_token().is_cancelled()
    }
}

/// A boxed child handle for type erasure.
pub type BoxedChildHandle = Box<dyn ChildHandle>;

/// Specification for a child process.
///
/// This defines how a child should be started and supervised.
pub struct ChildSpec {
    /// Unique identifier for this child within the supervisor.
    id: String,

    /// Factory function to create and start the child.
    /// Returns a boxed handle to the started process.
    start: Arc<dyn Fn() -> BoxedChildHandle + Send + Sync>,

    /// When the child should be restarted.
    restart: RestartType,

    /// How to shut down the child.
    shutdown: Shutdown,

    /// Type of child (worker or supervisor).
    child_type: ChildType,
}

impl ChildSpec {
    /// Create a new child specification for a worker.
    ///
    /// # Arguments
    ///
    /// * `id` - Unique identifier for this child
    /// * `start` - Factory function that starts and returns a handle to the child
    ///
    /// # Example
    ///
    /// ```ignore
    /// let spec = ChildSpec::worker("my_worker", || MyWorker::new().start(Backend::Async));
    /// ```
    pub fn worker<F, H>(id: impl Into<String>, start: F) -> Self
    where
        F: Fn() -> H + Send + Sync + 'static,
        H: ChildHandle + 'static,
    {
        Self {
            id: id.into(),
            start: Arc::new(move || Box::new(start()) as BoxedChildHandle),
            restart: RestartType::default(),
            shutdown: Shutdown::default(),
            child_type: ChildType::Worker,
        }
    }

    /// Create a new child specification for a supervisor (nested supervision).
    ///
    /// # Arguments
    ///
    /// * `id` - Unique identifier for this child
    /// * `start` - Factory function that starts and returns a handle to the supervisor
    pub fn supervisor<F, H>(id: impl Into<String>, start: F) -> Self
    where
        F: Fn() -> H + Send + Sync + 'static,
        H: ChildHandle + 'static,
    {
        Self {
            id: id.into(),
            start: Arc::new(move || Box::new(start()) as BoxedChildHandle),
            restart: RestartType::default(),
            shutdown: Shutdown::default(),
            child_type: ChildType::Supervisor,
        }
    }

    /// Get the ID of this child spec.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Get the restart type.
    pub fn restart_type(&self) -> RestartType {
        self.restart
    }

    /// Get the shutdown behavior.
    pub fn shutdown_behavior(&self) -> Shutdown {
        self.shutdown
    }

    /// Get the child type.
    pub fn child_type(&self) -> ChildType {
        self.child_type
    }

    /// Set the restart type for this child.
    pub fn with_restart(mut self, restart: RestartType) -> Self {
        self.restart = restart;
        self
    }

    /// Set the shutdown behavior for this child.
    pub fn with_shutdown(mut self, shutdown: Shutdown) -> Self {
        self.shutdown = shutdown;
        self
    }

    /// Convenience method to mark this as a permanent child (always restart).
    pub fn permanent(self) -> Self {
        self.with_restart(RestartType::Permanent)
    }

    /// Convenience method to mark this as a transient child (restart on crash).
    pub fn transient(self) -> Self {
        self.with_restart(RestartType::Transient)
    }

    /// Convenience method to mark this as a temporary child (never restart).
    pub fn temporary(self) -> Self {
        self.with_restart(RestartType::Temporary)
    }

    /// Start this child and return a handle.
    pub(crate) fn start(&self) -> BoxedChildHandle {
        (self.start)()
    }
}

impl std::fmt::Debug for ChildSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChildSpec")
            .field("id", &self.id)
            .field("restart", &self.restart)
            .field("shutdown", &self.shutdown)
            .field("child_type", &self.child_type)
            .finish_non_exhaustive()
    }
}

/// Clone implementation creates a new ChildSpec that shares the same start function.
impl Clone for ChildSpec {
    fn clone(&self) -> Self {
        Self {
            id: self.id.clone(),
            start: Arc::clone(&self.start),
            restart: self.restart,
            shutdown: self.shutdown,
            child_type: self.child_type,
        }
    }
}

/// Specification for a supervisor.
///
/// Defines the restart strategy and child processes.
#[derive(Clone)]
pub struct SupervisorSpec {
    /// Strategy for handling child failures.
    pub strategy: RestartStrategy,

    /// Maximum number of restarts allowed within the time window.
    pub max_restarts: u32,

    /// Time window for counting restarts.
    pub max_seconds: Duration,

    /// Child specifications in start order.
    pub children: Vec<ChildSpec>,

    /// Optional name to register the supervisor under.
    pub name: Option<String>,
}

impl SupervisorSpec {
    /// Create a new supervisor specification with the given strategy.
    pub fn new(strategy: RestartStrategy) -> Self {
        Self {
            strategy,
            max_restarts: 3,
            max_seconds: Duration::from_secs(5),
            children: Vec::new(),
            name: None,
        }
    }

    /// Set the maximum restarts allowed within the time window.
    ///
    /// If more than `max_restarts` occur within `max_seconds`,
    /// the supervisor will shut down.
    pub fn max_restarts(mut self, max_restarts: u32, max_seconds: Duration) -> Self {
        self.max_restarts = max_restarts;
        self.max_seconds = max_seconds;
        self
    }

    /// Add a child to this supervisor.
    pub fn child(mut self, spec: ChildSpec) -> Self {
        self.children.push(spec);
        self
    }

    /// Add multiple children to this supervisor.
    pub fn children(mut self, specs: impl IntoIterator<Item = ChildSpec>) -> Self {
        self.children.extend(specs);
        self
    }

    /// Register the supervisor with a name.
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }
}

impl std::fmt::Debug for SupervisorSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SupervisorSpec")
            .field("strategy", &self.strategy)
            .field("max_restarts", &self.max_restarts)
            .field("max_seconds", &self.max_seconds)
            .field("children", &self.children)
            .field("name", &self.name)
            .finish()
    }
}

/// Information about a running child.
pub struct ChildInfo {
    /// The child's specification.
    spec: ChildSpec,

    /// The child's current handle (None if not running).
    handle: Option<BoxedChildHandle>,

    /// Monitor reference for this child.
    monitor_ref: Option<MonitorRef>,

    /// Number of times this child has been restarted.
    restart_count: u32,
}

impl ChildInfo {
    /// Get the child's specification.
    pub fn spec(&self) -> &ChildSpec {
        &self.spec
    }

    /// Get the child's current Pid (None if not running).
    pub fn pid(&self) -> Option<Pid> {
        self.handle.as_ref().map(|h| h.pid())
    }

    /// Check if the child is currently running.
    pub fn is_running(&self) -> bool {
        self.handle.as_ref().map(|h| h.is_alive()).unwrap_or(false)
    }

    /// Get the number of times this child has been restarted.
    pub fn restart_count(&self) -> u32 {
        self.restart_count
    }

    /// Get the monitor reference for this child.
    pub fn monitor_ref(&self) -> Option<MonitorRef> {
        self.monitor_ref
    }
}

impl std::fmt::Debug for ChildInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChildInfo")
            .field("spec", &self.spec)
            .field("pid", &self.pid())
            .field("monitor_ref", &self.monitor_ref)
            .field("restart_count", &self.restart_count)
            .finish()
    }
}

/// Error type for supervisor operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SupervisorError {
    /// A child with this ID already exists.
    ChildAlreadyExists(String),

    /// The specified child was not found.
    ChildNotFound(String),

    /// Failed to start a child.
    StartFailed(String, String),

    /// Maximum restart intensity exceeded.
    MaxRestartsExceeded,

    /// The supervisor is shutting down.
    ShuttingDown,
}

impl std::fmt::Display for SupervisorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SupervisorError::ChildAlreadyExists(id) => {
                write!(f, "child '{}' already exists", id)
            }
            SupervisorError::ChildNotFound(id) => {
                write!(f, "child '{}' not found", id)
            }
            SupervisorError::StartFailed(id, reason) => {
                write!(f, "failed to start child '{}': {}", id, reason)
            }
            SupervisorError::MaxRestartsExceeded => {
                write!(f, "maximum restart intensity exceeded")
            }
            SupervisorError::ShuttingDown => {
                write!(f, "supervisor is shutting down")
            }
        }
    }
}

impl std::error::Error for SupervisorError {}

/// Internal state for the supervisor.
struct SupervisorState {
    /// The supervisor specification.
    spec: SupervisorSpec,

    /// Running children indexed by ID.
    children: HashMap<String, ChildInfo>,

    /// Order of children (for restart strategies).
    child_order: Vec<String>,

    /// Pid to child ID mapping.
    pid_to_child: HashMap<Pid, String>,

    /// Restart intensity tracker.
    restart_tracker: RestartIntensityTracker,

    /// Whether we're in the process of shutting down.
    shutting_down: bool,
}

impl SupervisorState {
    /// Create a new supervisor state from a specification.
    fn new(spec: SupervisorSpec) -> Self {
        let restart_tracker = RestartIntensityTracker::new(spec.max_restarts, spec.max_seconds);
        Self {
            spec,
            children: HashMap::new(),
            child_order: Vec::new(),
            pid_to_child: HashMap::new(),
            restart_tracker,
            shutting_down: false,
        }
    }

    /// Start all children defined in the spec and set up monitoring.
    fn start_children(
        &mut self,
        supervisor_handle: &GenServerHandle<Supervisor>,
    ) -> Result<(), SupervisorError> {
        for child_spec in self.spec.children.clone() {
            self.start_child_internal(child_spec, supervisor_handle)?;
        }
        Ok(())
    }

    /// Start a specific child and set up monitoring.
    fn start_child_internal(
        &mut self,
        spec: ChildSpec,
        supervisor_handle: &GenServerHandle<Supervisor>,
    ) -> Result<Pid, SupervisorError> {
        let id = spec.id().to_string();

        if self.children.contains_key(&id) {
            return Err(SupervisorError::ChildAlreadyExists(id));
        }

        // Start the child
        let handle = spec.start();
        let pid = handle.pid();

        // Set up monitoring so we receive DOWN messages when child exits
        let monitor_ref = supervisor_handle
            .monitor(&pid)
            .ok();

        // Create child info
        let info = ChildInfo {
            spec,
            handle: Some(handle),
            monitor_ref,
            restart_count: 0,
        };

        self.children.insert(id.clone(), info);
        self.child_order.push(id.clone());
        self.pid_to_child.insert(pid, id);

        Ok(pid)
    }

    /// Dynamically add and start a new child.
    fn start_child(
        &mut self,
        spec: ChildSpec,
        supervisor_handle: &GenServerHandle<Supervisor>,
    ) -> Result<Pid, SupervisorError> {
        if self.shutting_down {
            return Err(SupervisorError::ShuttingDown);
        }
        self.start_child_internal(spec, supervisor_handle)
    }

    /// Terminate a child by ID.
    fn terminate_child(&mut self, id: &str) -> Result<(), SupervisorError> {
        let info = self
            .children
            .get_mut(id)
            .ok_or_else(|| SupervisorError::ChildNotFound(id.to_string()))?;

        if let Some(handle) = info.handle.take() {
            let pid = handle.pid();
            self.pid_to_child.remove(&pid);
            // Actually shut down the child
            handle.shutdown();
        }

        Ok(())
    }

    /// Terminate multiple children by IDs (in reverse order for proper cleanup).
    ///
    /// Note: This is a non-blocking termination. The cancellation token is
    /// cancelled but we don't wait for the child to fully exit. This is a
    /// design trade-off - proper async waiting would require this method
    /// to be async. In practice, the child will exit shortly after and
    /// the supervisor will receive a DOWN message.
    fn terminate_children(&mut self, ids: &[String]) {
        // Terminate in reverse order (last started, first terminated)
        for id in ids.iter().rev() {
            if let Some(info) = self.children.get_mut(id) {
                if let Some(handle) = info.handle.take() {
                    let pid = handle.pid();
                    self.pid_to_child.remove(&pid);
                    handle.shutdown();
                }
            }
        }
    }

    /// Restart a child by ID.
    fn restart_child(
        &mut self,
        id: &str,
        supervisor_handle: &GenServerHandle<Supervisor>,
    ) -> Result<Pid, SupervisorError> {
        if self.shutting_down {
            return Err(SupervisorError::ShuttingDown);
        }

        // Check restart intensity
        if !self.restart_tracker.can_restart() {
            return Err(SupervisorError::MaxRestartsExceeded);
        }

        let info = self
            .children
            .get_mut(id)
            .ok_or_else(|| SupervisorError::ChildNotFound(id.to_string()))?;

        // Remove old pid mapping and shut down old handle
        if let Some(old_handle) = info.handle.take() {
            let old_pid = old_handle.pid();
            self.pid_to_child.remove(&old_pid);
            old_handle.shutdown();
        }

        // Cancel old monitor
        if let Some(old_ref) = info.monitor_ref.take() {
            supervisor_handle.demonitor(old_ref);
        }

        // Start new instance
        let new_handle = info.spec.start();
        let pid = new_handle.pid();

        // Set up new monitoring
        info.monitor_ref = supervisor_handle
            .monitor(&pid)
            .ok();

        info.handle = Some(new_handle);
        info.restart_count += 1;

        self.pid_to_child.insert(pid, id.to_string());
        self.restart_tracker.record_restart();

        Ok(pid)
    }

    /// Delete a child specification (child must be terminated first).
    fn delete_child(&mut self, id: &str) -> Result<(), SupervisorError> {
        let info = self
            .children
            .get(id)
            .ok_or_else(|| SupervisorError::ChildNotFound(id.to_string()))?;

        if info.handle.is_some() {
            // Child is still running, terminate first
            self.terminate_child(id)?;
        }

        self.children.remove(id);
        self.child_order.retain(|c| c != id);

        Ok(())
    }

    /// Handle a child exit (DOWN message received).
    ///
    /// Returns the IDs of children that need to be restarted.
    /// For OneForAll/RestForOne, this also terminates the affected children.
    fn handle_child_exit(
        &mut self,
        pid: Pid,
        reason: &ExitReason,
    ) -> Result<Vec<String>, SupervisorError> {
        if self.shutting_down {
            return Ok(Vec::new());
        }

        let child_id = match self.pid_to_child.remove(&pid) {
            Some(id) => id,
            None => return Ok(Vec::new()), // Unknown child, ignore
        };

        // Update child info - clear the handle since child has exited
        if let Some(info) = self.children.get_mut(&child_id) {
            info.handle = None;
            info.monitor_ref = None;
        }

        // Determine if we should restart based on restart type
        let should_restart = match self.children.get(&child_id) {
            Some(info) => match info.spec.restart {
                RestartType::Permanent => true,
                RestartType::Transient => !reason.is_normal(),
                RestartType::Temporary => false,
            },
            None => false,
        };

        if !should_restart {
            return Ok(Vec::new());
        }

        // Determine which children to restart based on strategy
        let to_restart = match self.spec.strategy {
            RestartStrategy::OneForOne => vec![child_id],
            RestartStrategy::OneForAll => {
                // Terminate all other children first (except the one that crashed)
                let others: Vec<String> = self
                    .child_order
                    .iter()
                    .filter(|id| *id != &child_id)
                    .cloned()
                    .collect();
                self.terminate_children(&others);
                self.child_order.clone()
            }
            RestartStrategy::RestForOne => {
                let idx = self
                    .child_order
                    .iter()
                    .position(|id| id == &child_id)
                    .unwrap_or(0);
                let affected: Vec<String> = self.child_order[idx..].to_vec();
                // Terminate children after the crashed one (they may still be running)
                let to_terminate: Vec<String> = self.child_order[idx + 1..].to_vec();
                self.terminate_children(&to_terminate);
                affected
            }
        };

        Ok(to_restart)
    }

    /// Get the list of child IDs in start order.
    fn which_children(&self) -> Vec<String> {
        self.child_order.clone()
    }

    /// Count the number of active children.
    fn count_children(&self) -> SupervisorCounts {
        let mut counts = SupervisorCounts::default();

        for info in self.children.values() {
            counts.specs += 1;
            if info.is_running() {
                counts.active += 1;
            }
            match info.spec.child_type() {
                ChildType::Worker => counts.workers += 1,
                ChildType::Supervisor => counts.supervisors += 1,
            }
        }

        counts
    }

    /// Begin shutdown sequence - terminates all children in reverse order.
    fn shutdown(&mut self) {
        self.shutting_down = true;
        let all_children = self.child_order.clone();
        self.terminate_children(&all_children);
    }
}


// ============================================================================
// Supervisor GenServer
// ============================================================================

/// Messages that can be sent to a Supervisor via call().
#[derive(Clone, Debug)]
pub enum SupervisorCall {
    /// Start a new child dynamically.
    StartChild(ChildSpec),
    /// Terminate a child by ID.
    TerminateChild(String),
    /// Restart a child by ID.
    RestartChild(String),
    /// Delete a child spec by ID.
    DeleteChild(String),
    /// Get list of child IDs.
    WhichChildren,
    /// Count children by type and state.
    CountChildren,
}

/// Messages that can be sent to a Supervisor via cast().
#[derive(Clone, Debug)]
pub enum SupervisorCast {
    /// No-op placeholder (supervisors mainly use calls).
    _Placeholder,
}

/// Response from Supervisor calls.
#[derive(Clone, Debug)]
pub enum SupervisorResponse {
    /// Child started successfully, returns new Pid.
    Started(Pid),
    /// Operation completed successfully.
    Ok,
    /// Error occurred.
    Error(SupervisorError),
    /// List of child IDs.
    Children(Vec<String>),
    /// Child counts.
    Counts(SupervisorCounts),
}

/// A Supervisor is a GenServer that manages child processes.
///
/// It monitors children and automatically restarts them according to
/// the configured strategy when they exit.
pub struct Supervisor {
    state: SupervisorState,
}

impl Supervisor {
    /// Create a new Supervisor from a specification.
    pub fn new(spec: SupervisorSpec) -> Self {
        Self {
            state: SupervisorState::new(spec),
        }
    }

    /// Start the supervisor and return a handle.
    ///
    /// This starts the supervisor GenServer and all children defined in the spec.
    pub fn start(spec: SupervisorSpec) -> GenServerHandle<Supervisor> {
        Supervisor::new(spec).start_server()
    }

    /// Start as a GenServer (internal use - prefer Supervisor::start).
    fn start_server(self) -> GenServerHandle<Supervisor> {
        GenServer::start(self, Backend::Async)
    }
}

impl GenServer for Supervisor {
    type CallMsg = SupervisorCall;
    type CastMsg = SupervisorCast;
    type OutMsg = SupervisorResponse;
    type Error = SupervisorError;

    async fn init(
        mut self,
        handle: &GenServerHandle<Self>,
    ) -> Result<InitResult<Self>, Self::Error> {
        // Enable trap_exit so we receive EXIT messages from linked children
        handle.trap_exit(true);

        // Start all children defined in the spec
        self.state.start_children(handle)?;

        // Register with name if specified
        if let Some(name) = &self.state.spec.name {
            let _ = handle.register(name.clone());
        }

        Ok(InitResult::Success(self))
    }

    async fn handle_call(
        &mut self,
        message: Self::CallMsg,
        handle: &GenServerHandle<Self>,
    ) -> CallResponse<Self> {
        let response = match message {
            SupervisorCall::StartChild(spec) => {
                match self.state.start_child(spec, handle) {
                    Ok(pid) => SupervisorResponse::Started(pid),
                    Err(e) => SupervisorResponse::Error(e),
                }
            }
            SupervisorCall::TerminateChild(id) => {
                match self.state.terminate_child(&id) {
                    Ok(()) => SupervisorResponse::Ok,
                    Err(e) => SupervisorResponse::Error(e),
                }
            }
            SupervisorCall::RestartChild(id) => {
                match self.state.restart_child(&id, handle) {
                    Ok(pid) => SupervisorResponse::Started(pid),
                    Err(e) => SupervisorResponse::Error(e),
                }
            }
            SupervisorCall::DeleteChild(id) => {
                match self.state.delete_child(&id) {
                    Ok(()) => SupervisorResponse::Ok,
                    Err(e) => SupervisorResponse::Error(e),
                }
            }
            SupervisorCall::WhichChildren => {
                SupervisorResponse::Children(self.state.which_children())
            }
            SupervisorCall::CountChildren => {
                SupervisorResponse::Counts(self.state.count_children())
            }
        };
        CallResponse::Reply(response)
    }

    async fn handle_cast(
        &mut self,
        _message: Self::CastMsg,
        _handle: &GenServerHandle<Self>,
    ) -> CastResponse {
        CastResponse::NoReply
    }

    async fn handle_info(
        &mut self,
        message: SystemMessage,
        handle: &GenServerHandle<Self>,
    ) -> InfoResponse {
        match message {
            SystemMessage::Down { pid, reason, .. } => {
                // A monitored child has exited
                match self.state.handle_child_exit(pid, &reason) {
                    Ok(to_restart) => {
                        // Restart the affected children
                        for id in to_restart {
                            match self.state.restart_child(&id, handle) {
                                Ok(_) => {
                                    tracing::debug!(child = %id, "Restarted child");
                                }
                                Err(SupervisorError::MaxRestartsExceeded) => {
                                    tracing::error!("Max restart intensity exceeded, supervisor stopping");
                                    return InfoResponse::Stop;
                                }
                                Err(e) => {
                                    tracing::error!(child = %id, error = ?e, "Failed to restart child");
                                }
                            }
                        }
                        InfoResponse::NoReply
                    }
                    Err(e) => {
                        tracing::error!(error = ?e, "Error handling child exit");
                        InfoResponse::NoReply
                    }
                }
            }
            SystemMessage::Exit { pid, reason } => {
                // A linked process has exited (we trap exits)
                tracing::debug!(%pid, ?reason, "Received EXIT from linked process");
                // Treat like a DOWN message
                match self.state.handle_child_exit(pid, &reason) {
                    Ok(to_restart) => {
                        for id in to_restart {
                            match self.state.restart_child(&id, handle) {
                                Ok(_) => {}
                                Err(SupervisorError::MaxRestartsExceeded) => {
                                    return InfoResponse::Stop;
                                }
                                Err(_) => {}
                            }
                        }
                        InfoResponse::NoReply
                    }
                    Err(_) => InfoResponse::NoReply,
                }
            }
            SystemMessage::Timeout { .. } => InfoResponse::NoReply,
        }
    }

    async fn teardown(mut self, _handle: &GenServerHandle<Self>) -> Result<(), Self::Error> {
        // Shut down all children in reverse order
        self.state.shutdown();
        Ok(())
    }
}

/// Counts of children by type and state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SupervisorCounts {
    /// Total number of child specifications.
    pub specs: usize,

    /// Number of actively running children.
    pub active: usize,

    /// Number of worker children.
    pub workers: usize,

    /// Number of supervisor children.
    pub supervisors: usize,
}

// ============================================================================
// DynamicSupervisor - for many dynamic children
// ============================================================================

/// Specification for a DynamicSupervisor.
#[derive(Debug, Clone)]
pub struct DynamicSupervisorSpec {
    /// Maximum number of restarts within the time window.
    pub max_restarts: u32,

    /// Time window for restart intensity.
    pub max_seconds: Duration,

    /// Optional maximum number of children.
    pub max_children: Option<usize>,

    /// Optional name for registration.
    pub name: Option<String>,
}

impl Default for DynamicSupervisorSpec {
    fn default() -> Self {
        Self {
            max_restarts: 3,
            max_seconds: Duration::from_secs(5),
            max_children: None,
            name: None,
        }
    }
}

impl DynamicSupervisorSpec {
    /// Create a new DynamicSupervisorSpec with default values.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the maximum restart intensity.
    pub fn max_restarts(mut self, max_restarts: u32, max_seconds: Duration) -> Self {
        self.max_restarts = max_restarts;
        self.max_seconds = max_seconds;
        self
    }

    /// Set the maximum number of children.
    pub fn max_children(mut self, max: usize) -> Self {
        self.max_children = Some(max);
        self
    }

    /// Set the name for registration.
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }
}

/// Messages that can be sent to a DynamicSupervisor via call().
#[derive(Clone, Debug)]
pub enum DynamicSupervisorCall {
    /// Start a new child. Returns the child's Pid.
    StartChild(ChildSpec),
    /// Terminate a child by Pid.
    TerminateChild(Pid),
    /// Get list of all child Pids.
    WhichChildren,
    /// Count children.
    CountChildren,
}

/// Messages that can be sent to a DynamicSupervisor via cast().
#[derive(Clone, Debug)]
pub enum DynamicSupervisorCast {
    /// Placeholder - dynamic supervisors mainly use calls.
    _Placeholder,
}

/// Response from DynamicSupervisor calls.
#[derive(Clone, Debug)]
pub enum DynamicSupervisorResponse {
    /// Child started successfully.
    Started(Pid),
    /// Operation completed successfully.
    Ok,
    /// Error occurred.
    Error(DynamicSupervisorError),
    /// List of child Pids.
    Children(Vec<Pid>),
    /// Child count.
    Count(usize),
}

/// Error type for DynamicSupervisor operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DynamicSupervisorError {
    /// Child with this Pid not found.
    ChildNotFound(Pid),
    /// Maximum restart intensity exceeded.
    MaxRestartsExceeded,
    /// Maximum children limit reached.
    MaxChildrenReached,
    /// Supervisor is shutting down.
    ShuttingDown,
}

impl std::fmt::Display for DynamicSupervisorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DynamicSupervisorError::ChildNotFound(pid) => {
                write!(f, "child with pid {} not found", pid)
            }
            DynamicSupervisorError::MaxRestartsExceeded => {
                write!(f, "maximum restart intensity exceeded")
            }
            DynamicSupervisorError::MaxChildrenReached => {
                write!(f, "maximum number of children reached")
            }
            DynamicSupervisorError::ShuttingDown => {
                write!(f, "dynamic supervisor is shutting down")
            }
        }
    }
}

impl std::error::Error for DynamicSupervisorError {}

/// Internal state for DynamicSupervisor.
struct DynamicSupervisorState {
    /// The supervisor specification.
    spec: DynamicSupervisorSpec,

    /// Running children indexed by Pid.
    children: HashMap<Pid, DynamicChildInfo>,

    /// Restart intensity tracker.
    restart_tracker: RestartIntensityTracker,

    /// Whether we're shutting down.
    shutting_down: bool,
}

/// Information about a dynamically started child.
struct DynamicChildInfo {
    /// The child's specification (for restart).
    spec: ChildSpec,

    /// The child's current handle.
    handle: BoxedChildHandle,

    /// Number of restarts for this child.
    restart_count: u32,
}

impl DynamicSupervisorState {
    fn new(spec: DynamicSupervisorSpec) -> Self {
        let restart_tracker = RestartIntensityTracker::new(spec.max_restarts, spec.max_seconds);
        Self {
            spec,
            children: HashMap::new(),
            restart_tracker,
            shutting_down: false,
        }
    }

    fn start_child(
        &mut self,
        spec: ChildSpec,
        supervisor_handle: &GenServerHandle<DynamicSupervisor>,
    ) -> Result<Pid, DynamicSupervisorError> {
        if self.shutting_down {
            return Err(DynamicSupervisorError::ShuttingDown);
        }

        // Check max children limit
        if let Some(max) = self.spec.max_children {
            if self.children.len() >= max {
                return Err(DynamicSupervisorError::MaxChildrenReached);
            }
        }

        // Start the child
        let handle = spec.start();
        let pid = handle.pid();

        // Set up monitoring (we don't store the ref as we track children by pid)
        let _ = supervisor_handle.monitor(&pid);

        let info = DynamicChildInfo {
            spec,
            handle,
            restart_count: 0,
        };

        self.children.insert(pid, info);
        Ok(pid)
    }

    fn terminate_child(&mut self, pid: Pid) -> Result<(), DynamicSupervisorError> {
        let info = self
            .children
            .remove(&pid)
            .ok_or(DynamicSupervisorError::ChildNotFound(pid))?;

        info.handle.shutdown();
        Ok(())
    }

    fn handle_child_exit(
        &mut self,
        pid: Pid,
        reason: &ExitReason,
        supervisor_handle: &GenServerHandle<DynamicSupervisor>,
    ) -> Result<(), DynamicSupervisorError> {
        if self.shutting_down {
            self.children.remove(&pid);
            return Ok(());
        }

        let info = match self.children.remove(&pid) {
            Some(info) => info,
            None => return Ok(()), // Unknown child, ignore
        };

        // Determine if we should restart based on restart type
        let should_restart = match info.spec.restart {
            RestartType::Permanent => true,
            RestartType::Transient => !reason.is_normal(),
            RestartType::Temporary => false,
        };

        if !should_restart {
            return Ok(());
        }

        // Check restart intensity
        if !self.restart_tracker.can_restart() {
            return Err(DynamicSupervisorError::MaxRestartsExceeded);
        }

        // Restart the child
        let new_handle = info.spec.start();
        let new_pid = new_handle.pid();
        let _ = supervisor_handle.monitor(&new_pid);

        let new_info = DynamicChildInfo {
            spec: info.spec,
            handle: new_handle,
            restart_count: info.restart_count + 1,
        };

        self.children.insert(new_pid, new_info);
        self.restart_tracker.record_restart();

        Ok(())
    }

    fn which_children(&self) -> Vec<Pid> {
        self.children.keys().copied().collect()
    }

    fn count_children(&self) -> usize {
        self.children.len()
    }

    fn shutdown(&mut self) {
        self.shutting_down = true;
        for (_, info) in self.children.drain() {
            info.handle.shutdown();
        }
    }
}

/// A DynamicSupervisor manages a dynamic set of children.
///
/// Unlike the regular Supervisor which has predefined children,
/// DynamicSupervisor is optimized for cases where children are
/// frequently started and stopped at runtime.
///
/// Key differences from Supervisor:
/// - No predefined children - all started via `start_child`
/// - Children identified by Pid, not by string ID
/// - Always uses OneForOne strategy (each child independent)
/// - Optimized for many children of the same type
///
/// # Example
///
/// ```ignore
/// use spawned_concurrency::Backend;
///
/// let sup = DynamicSupervisor::start(DynamicSupervisorSpec::new());
///
/// // Start children dynamically
/// let child_spec = ChildSpec::worker("conn", || ConnectionHandler::new().start(Backend::Async));
/// if let DynamicSupervisorResponse::Started(pid) =
///     sup.call(DynamicSupervisorCall::StartChild(child_spec)).await.unwrap()
/// {
///     println!("Started child with pid: {}", pid);
/// }
/// ```
pub struct DynamicSupervisor {
    state: DynamicSupervisorState,
}

impl DynamicSupervisor {
    /// Create a new DynamicSupervisor.
    pub fn new(spec: DynamicSupervisorSpec) -> Self {
        Self {
            state: DynamicSupervisorState::new(spec),
        }
    }

    /// Start the DynamicSupervisor and return a handle.
    pub fn start(spec: DynamicSupervisorSpec) -> GenServerHandle<DynamicSupervisor> {
        DynamicSupervisor::new(spec).start_server()
    }

    fn start_server(self) -> GenServerHandle<DynamicSupervisor> {
        GenServer::start(self, Backend::Async)
    }
}

impl GenServer for DynamicSupervisor {
    type CallMsg = DynamicSupervisorCall;
    type CastMsg = DynamicSupervisorCast;
    type OutMsg = DynamicSupervisorResponse;
    type Error = DynamicSupervisorError;

    async fn init(
        self,
        handle: &GenServerHandle<Self>,
    ) -> Result<InitResult<Self>, Self::Error> {
        handle.trap_exit(true);

        if let Some(name) = &self.state.spec.name {
            let _ = handle.register(name.clone());
        }

        Ok(InitResult::Success(self))
    }

    async fn handle_call(
        &mut self,
        message: Self::CallMsg,
        handle: &GenServerHandle<Self>,
    ) -> CallResponse<Self> {
        let response = match message {
            DynamicSupervisorCall::StartChild(spec) => {
                match self.state.start_child(spec, handle) {
                    Ok(pid) => DynamicSupervisorResponse::Started(pid),
                    Err(e) => DynamicSupervisorResponse::Error(e),
                }
            }
            DynamicSupervisorCall::TerminateChild(pid) => {
                match self.state.terminate_child(pid) {
                    Ok(()) => DynamicSupervisorResponse::Ok,
                    Err(e) => DynamicSupervisorResponse::Error(e),
                }
            }
            DynamicSupervisorCall::WhichChildren => {
                DynamicSupervisorResponse::Children(self.state.which_children())
            }
            DynamicSupervisorCall::CountChildren => {
                DynamicSupervisorResponse::Count(self.state.count_children())
            }
        };
        CallResponse::Reply(response)
    }

    async fn handle_cast(
        &mut self,
        _message: Self::CastMsg,
        _handle: &GenServerHandle<Self>,
    ) -> CastResponse {
        CastResponse::NoReply
    }

    async fn handle_info(
        &mut self,
        message: SystemMessage,
        handle: &GenServerHandle<Self>,
    ) -> InfoResponse {
        match message {
            SystemMessage::Down { pid, reason, .. } => {
                match self.state.handle_child_exit(pid, &reason, handle) {
                    Ok(()) => InfoResponse::NoReply,
                    Err(DynamicSupervisorError::MaxRestartsExceeded) => {
                        tracing::error!("DynamicSupervisor: max restart intensity exceeded");
                        InfoResponse::Stop
                    }
                    Err(e) => {
                        tracing::error!("DynamicSupervisor error: {:?}", e);
                        InfoResponse::NoReply
                    }
                }
            }
            SystemMessage::Exit { pid, reason } => {
                match self.state.handle_child_exit(pid, &reason, handle) {
                    Ok(()) => InfoResponse::NoReply,
                    Err(DynamicSupervisorError::MaxRestartsExceeded) => InfoResponse::Stop,
                    Err(_) => InfoResponse::NoReply,
                }
            }
            SystemMessage::Timeout { .. } => InfoResponse::NoReply,
        }
    }

    async fn teardown(mut self, _handle: &GenServerHandle<Self>) -> Result<(), Self::Error> {
        self.state.shutdown();
        Ok(())
    }
}
