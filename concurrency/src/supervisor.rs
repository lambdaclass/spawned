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
//!
//! let spec = SupervisorSpec::new(RestartStrategy::OneForOne)
//!     .max_restarts(3, std::time::Duration::from_secs(5))
//!     .child(ChildSpec::worker("worker", || WorkerServer::new().start()));
//!
//! let supervisor = Supervisor::start(spec);
//! ```

use crate::link::{MonitorRef, SystemMessage};
use crate::pid::{ExitReason, HasPid, Pid};
use crate::tasks::{
    CallResponse, CastResponse, GenServer, GenServerHandle, InfoResponse, InitResult,
};
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
    /// let spec = ChildSpec::worker("my_worker", || MyWorker::new().start());
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

    /// Restart timestamps for rate limiting.
    restart_times: Vec<Instant>,

    /// Whether we're in the process of shutting down.
    shutting_down: bool,
}

impl SupervisorState {
    /// Create a new supervisor state from a specification.
    fn new(spec: SupervisorSpec) -> Self {
        Self {
            spec,
            children: HashMap::new(),
            child_order: Vec::new(),
            pid_to_child: HashMap::new(),
            restart_times: Vec::new(),
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
            .monitor(&ChildPidWrapper(pid))
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
        if !self.check_restart_intensity() {
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
            .monitor(&ChildPidWrapper(pid))
            .ok();

        info.handle = Some(new_handle);
        info.restart_count += 1;

        self.pid_to_child.insert(pid, id.to_string());
        self.restart_times.push(Instant::now());

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

    /// Check if we're within restart intensity limits.
    fn check_restart_intensity(&mut self) -> bool {
        let now = Instant::now();
        let cutoff = now - self.spec.max_seconds;

        // Remove old restart times
        self.restart_times.retain(|t| *t > cutoff);

        // Check if we've exceeded the limit
        (self.restart_times.len() as u32) < self.spec.max_restarts
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

/// Wrapper to implement HasPid for a raw Pid (for monitoring).
struct ChildPidWrapper(Pid);

impl HasPid for ChildPidWrapper {
    fn pid(&self) -> Pid {
        self.0
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
        GenServer::start(self)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    // Mock child handle for testing
    struct MockChildHandle {
        pid: Pid,
        alive: Arc<AtomicBool>,
    }

    impl MockChildHandle {
        fn new() -> Self {
            Self {
                pid: Pid::new(),
                alive: Arc::new(AtomicBool::new(true)),
            }
        }
    }

    impl ChildHandle for MockChildHandle {
        fn pid(&self) -> Pid {
            self.pid
        }

        fn shutdown(&self) {
            self.alive.store(false, Ordering::SeqCst);
        }

        fn is_alive(&self) -> bool {
            self.alive.load(Ordering::SeqCst)
        }
    }

    // Helper to create a mock child spec
    fn mock_worker(id: &str) -> ChildSpec {
        ChildSpec::worker(id, MockChildHandle::new)
    }

    // Helper with a counter to track starts
    fn counted_worker(id: &str, counter: Arc<AtomicU32>) -> ChildSpec {
        ChildSpec::worker(id, move || {
            counter.fetch_add(1, Ordering::SeqCst);
            MockChildHandle::new()
        })
    }

    #[test]
    fn test_child_spec_creation() {
        let spec = mock_worker("worker1");
        assert_eq!(spec.id(), "worker1");
        assert_eq!(spec.restart_type(), RestartType::Permanent);
        assert_eq!(spec.child_type(), ChildType::Worker);
    }

    #[test]
    fn test_child_spec_builder() {
        let spec = mock_worker("worker1")
            .transient()
            .with_shutdown(Shutdown::Brutal);

        assert_eq!(spec.restart_type(), RestartType::Transient);
        assert_eq!(spec.shutdown_behavior(), Shutdown::Brutal);
        assert_eq!(spec.child_type(), ChildType::Worker);
    }

    #[test]
    fn test_supervisor_child_spec() {
        let spec = ChildSpec::supervisor("sub_sup", MockChildHandle::new);
        assert_eq!(spec.child_type(), ChildType::Supervisor);
    }

    #[test]
    fn test_supervisor_spec_creation() {
        let spec = SupervisorSpec::new(RestartStrategy::OneForOne)
            .max_restarts(5, Duration::from_secs(10))
            .name("my_supervisor")
            .child(mock_worker("worker1"))
            .child(mock_worker("worker2"));

        assert_eq!(spec.strategy, RestartStrategy::OneForOne);
        assert_eq!(spec.max_restarts, 5);
        assert_eq!(spec.max_seconds, Duration::from_secs(10));
        assert_eq!(spec.name, Some("my_supervisor".to_string()));
        assert_eq!(spec.children.len(), 2);
    }

    #[test]
    fn test_restart_strategy_values() {
        assert_eq!(RestartStrategy::OneForOne, RestartStrategy::OneForOne);
        assert_ne!(RestartStrategy::OneForOne, RestartStrategy::OneForAll);
        assert_ne!(RestartStrategy::OneForAll, RestartStrategy::RestForOne);
    }

    #[test]
    fn test_restart_type_default() {
        assert_eq!(RestartType::default(), RestartType::Permanent);
    }

    #[test]
    fn test_shutdown_default() {
        assert_eq!(Shutdown::default(), Shutdown::Timeout(Duration::from_secs(5)));
    }

    #[test]
    fn test_child_type_default() {
        assert_eq!(ChildType::default(), ChildType::Worker);
    }

    #[test]
    fn test_supervisor_error_display() {
        assert_eq!(
            SupervisorError::ChildAlreadyExists("foo".to_string()).to_string(),
            "child 'foo' already exists"
        );
        assert_eq!(
            SupervisorError::ChildNotFound("bar".to_string()).to_string(),
            "child 'bar' not found"
        );
        assert_eq!(
            SupervisorError::StartFailed("baz".to_string(), "oops".to_string()).to_string(),
            "failed to start child 'baz': oops"
        );
        assert_eq!(
            SupervisorError::MaxRestartsExceeded.to_string(),
            "maximum restart intensity exceeded"
        );
        assert_eq!(
            SupervisorError::ShuttingDown.to_string(),
            "supervisor is shutting down"
        );
    }

    #[test]
    fn test_child_info_methods() {
        let spec = mock_worker("test");
        let handle = spec.start();
        let pid = handle.pid();

        let info = ChildInfo {
            spec: mock_worker("test"),
            handle: Some(handle),
            monitor_ref: None,
            restart_count: 5,
        };

        assert_eq!(info.pid(), Some(pid));
        assert!(info.is_running());
        assert_eq!(info.restart_count(), 5);
        assert_eq!(info.monitor_ref(), None);
    }

    #[test]
    fn test_supervisor_counts_default() {
        let counts = SupervisorCounts::default();
        assert_eq!(counts.specs, 0);
        assert_eq!(counts.active, 0);
        assert_eq!(counts.workers, 0);
        assert_eq!(counts.supervisors, 0);
    }

    #[test]
    fn test_child_handle_shutdown() {
        let handle = MockChildHandle::new();
        assert!(handle.is_alive());
        handle.shutdown();
        assert!(!handle.is_alive());
    }

    #[test]
    fn test_child_spec_start_creates_new_handles() {
        let counter = Arc::new(AtomicU32::new(0));
        let spec = counted_worker("worker1", counter.clone());

        // Each call to start() should create a new handle
        let _h1 = spec.start();
        assert_eq!(counter.load(Ordering::SeqCst), 1);

        let _h2 = spec.start();
        assert_eq!(counter.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn test_supervisor_spec_multiple_children() {
        let spec = SupervisorSpec::new(RestartStrategy::OneForAll)
            .children(vec![
                mock_worker("w1"),
                mock_worker("w2"),
                mock_worker("w3"),
            ]);

        assert_eq!(spec.children.len(), 3);
        assert_eq!(spec.strategy, RestartStrategy::OneForAll);
    }

    #[test]
    fn test_child_spec_clone() {
        let spec1 = mock_worker("worker1").transient();
        let spec2 = spec1.clone();

        assert_eq!(spec1.id(), spec2.id());
        assert_eq!(spec1.restart_type(), spec2.restart_type());
    }
}
