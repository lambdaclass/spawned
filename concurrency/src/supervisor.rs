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
//!     .child(ChildSpec::new("worker", || WorkerServer::new()));
//!
//! let supervisor = Supervisor::start(spec).await?;
//! ```

use crate::link::MonitorRef;
use crate::pid::{ExitReason, Pid};
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

/// Specification for a child process.
///
/// This defines how a child should be started and supervised.
#[derive(Clone)]
pub struct ChildSpec {
    /// Unique identifier for this child within the supervisor.
    pub id: String,

    /// Factory function to create and start the child.
    /// Returns the Pid of the started process.
    pub start: Arc<dyn Fn() -> Pid + Send + Sync>,

    /// When the child should be restarted.
    pub restart: RestartType,

    /// How to shut down the child.
    pub shutdown: Shutdown,

    /// Type of child (worker or supervisor).
    pub child_type: ChildType,
}

impl ChildSpec {
    /// Create a new child specification.
    ///
    /// # Arguments
    ///
    /// * `id` - Unique identifier for this child
    /// * `start` - Factory function that starts and returns the child's Pid
    pub fn new<F>(id: impl Into<String>, start: F) -> Self
    where
        F: Fn() -> Pid + Send + Sync + 'static,
    {
        Self {
            id: id.into(),
            start: Arc::new(start),
            restart: RestartType::default(),
            shutdown: Shutdown::default(),
            child_type: ChildType::default(),
        }
    }

    /// Set the restart type for this child.
    pub fn restart_type(mut self, restart: RestartType) -> Self {
        self.restart = restart;
        self
    }

    /// Set the shutdown behavior for this child.
    pub fn shutdown(mut self, shutdown: Shutdown) -> Self {
        self.shutdown = shutdown;
        self
    }

    /// Set the child type (worker or supervisor).
    pub fn child_type(mut self, child_type: ChildType) -> Self {
        self.child_type = child_type;
        self
    }

    /// Convenience method to mark this as a permanent child (always restart).
    pub fn permanent(self) -> Self {
        self.restart_type(RestartType::Permanent)
    }

    /// Convenience method to mark this as a transient child (restart on crash).
    pub fn transient(self) -> Self {
        self.restart_type(RestartType::Transient)
    }

    /// Convenience method to mark this as a temporary child (never restart).
    pub fn temporary(self) -> Self {
        self.restart_type(RestartType::Temporary)
    }
}

impl std::fmt::Debug for ChildSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChildSpec")
            .field("id", &self.id)
            .field("restart", &self.restart)
            .field("shutdown", &self.shutdown)
            .field("child_type", &self.child_type)
            .finish()
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
#[derive(Debug, Clone)]
pub struct ChildInfo {
    /// The child's specification.
    pub spec: ChildSpec,

    /// The child's current Pid (None if not running).
    pub pid: Option<Pid>,

    /// Monitor reference for this child.
    pub monitor_ref: Option<MonitorRef>,

    /// Number of times this child has been restarted.
    pub restart_count: u32,
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

/// State for the supervisor.
pub struct SupervisorState {
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
    pub fn new(spec: SupervisorSpec) -> Self {
        Self {
            spec,
            children: HashMap::new(),
            child_order: Vec::new(),
            pid_to_child: HashMap::new(),
            restart_times: Vec::new(),
            shutting_down: false,
        }
    }

    /// Start all children defined in the spec.
    ///
    /// Returns Ok if all children started successfully.
    pub fn start_children(&mut self) -> Result<(), SupervisorError> {
        for child_spec in self.spec.children.clone() {
            self.start_child_internal(child_spec)?;
        }
        Ok(())
    }

    /// Start a specific child.
    fn start_child_internal(&mut self, spec: ChildSpec) -> Result<Pid, SupervisorError> {
        let id = spec.id.clone();

        if self.children.contains_key(&id) {
            return Err(SupervisorError::ChildAlreadyExists(id));
        }

        // Start the child
        let pid = (spec.start)();

        // Create child info
        let info = ChildInfo {
            spec,
            pid: Some(pid),
            monitor_ref: None, // TODO: Set up monitoring
            restart_count: 0,
        };

        self.children.insert(id.clone(), info);
        self.child_order.push(id.clone());
        self.pid_to_child.insert(pid, id);

        Ok(pid)
    }

    /// Dynamically add and start a new child.
    pub fn start_child(&mut self, spec: ChildSpec) -> Result<Pid, SupervisorError> {
        if self.shutting_down {
            return Err(SupervisorError::ShuttingDown);
        }
        self.start_child_internal(spec)
    }

    /// Terminate a child by ID.
    pub fn terminate_child(&mut self, id: &str) -> Result<(), SupervisorError> {
        let info = self
            .children
            .get_mut(id)
            .ok_or_else(|| SupervisorError::ChildNotFound(id.to_string()))?;

        if let Some(pid) = info.pid.take() {
            self.pid_to_child.remove(&pid);
            // In a real implementation, we would send an exit signal to the child
            // For now, we just remove the tracking
        }

        Ok(())
    }

    /// Restart a child by ID.
    pub fn restart_child(&mut self, id: &str) -> Result<Pid, SupervisorError> {
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

        // Remove old pid mapping
        if let Some(old_pid) = info.pid.take() {
            self.pid_to_child.remove(&old_pid);
        }

        // Start new instance
        let pid = (info.spec.start)();
        info.pid = Some(pid);
        info.restart_count += 1;

        self.pid_to_child.insert(pid, id.to_string());
        self.restart_times.push(Instant::now());

        Ok(pid)
    }

    /// Delete a child specification (child must be terminated first).
    pub fn delete_child(&mut self, id: &str) -> Result<(), SupervisorError> {
        let info = self
            .children
            .get(id)
            .ok_or_else(|| SupervisorError::ChildNotFound(id.to_string()))?;

        if info.pid.is_some() {
            // Child is still running, terminate first
            self.terminate_child(id)?;
        }

        self.children.remove(id);
        self.child_order.retain(|c| c != id);

        Ok(())
    }

    /// Handle a child exit.
    ///
    /// Returns the IDs of children that should be restarted.
    pub fn handle_child_exit(
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

        // Update child info
        if let Some(info) = self.children.get_mut(&child_id) {
            info.pid = None;
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
            RestartStrategy::OneForAll => self.child_order.clone(),
            RestartStrategy::RestForOne => {
                let idx = self
                    .child_order
                    .iter()
                    .position(|id| id == &child_id)
                    .unwrap_or(0);
                self.child_order[idx..].to_vec()
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
    pub fn which_children(&self) -> Vec<String> {
        self.child_order.clone()
    }

    /// Get information about a specific child.
    pub fn get_child_info(&self, id: &str) -> Option<&ChildInfo> {
        self.children.get(id)
    }

    /// Count the number of active children.
    pub fn count_children(&self) -> SupervisorCounts {
        let mut counts = SupervisorCounts::default();

        for info in self.children.values() {
            counts.specs += 1;
            if info.pid.is_some() {
                counts.active += 1;
            }
            match info.spec.child_type {
                ChildType::Worker => counts.workers += 1,
                ChildType::Supervisor => counts.supervisors += 1,
            }
        }

        counts
    }

    /// Begin shutdown sequence.
    pub fn begin_shutdown(&mut self) {
        self.shutting_down = true;
    }

    /// Get the supervisor's strategy.
    pub fn strategy(&self) -> RestartStrategy {
        self.spec.strategy
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
    use std::sync::atomic::{AtomicU32, Ordering};

    // Helper to create a mock start function that returns unique Pids
    fn mock_start() -> Pid {
        Pid::new()
    }

    // Helper with a counter to track starts
    fn counted_start(counter: Arc<AtomicU32>) -> impl Fn() -> Pid + Send + Sync {
        move || {
            counter.fetch_add(1, Ordering::SeqCst);
            Pid::new()
        }
    }

    #[test]
    fn test_child_spec_creation() {
        let spec = ChildSpec::new("worker1", mock_start);
        assert_eq!(spec.id, "worker1");
        assert_eq!(spec.restart, RestartType::Permanent);
        assert_eq!(spec.child_type, ChildType::Worker);
    }

    #[test]
    fn test_child_spec_builder() {
        let spec = ChildSpec::new("worker1", mock_start)
            .transient()
            .shutdown(Shutdown::Brutal)
            .child_type(ChildType::Supervisor);

        assert_eq!(spec.restart, RestartType::Transient);
        assert_eq!(spec.shutdown, Shutdown::Brutal);
        assert_eq!(spec.child_type, ChildType::Supervisor);
    }

    #[test]
    fn test_supervisor_spec_creation() {
        let spec = SupervisorSpec::new(RestartStrategy::OneForOne)
            .max_restarts(5, Duration::from_secs(10))
            .name("my_supervisor")
            .child(ChildSpec::new("worker1", mock_start))
            .child(ChildSpec::new("worker2", mock_start));

        assert_eq!(spec.strategy, RestartStrategy::OneForOne);
        assert_eq!(spec.max_restarts, 5);
        assert_eq!(spec.max_seconds, Duration::from_secs(10));
        assert_eq!(spec.name, Some("my_supervisor".to_string()));
        assert_eq!(spec.children.len(), 2);
    }

    #[test]
    fn test_supervisor_state_start_children() {
        let spec = SupervisorSpec::new(RestartStrategy::OneForOne)
            .child(ChildSpec::new("worker1", mock_start))
            .child(ChildSpec::new("worker2", mock_start));

        let mut state = SupervisorState::new(spec);
        assert!(state.start_children().is_ok());

        let counts = state.count_children();
        assert_eq!(counts.specs, 2);
        assert_eq!(counts.active, 2);
        assert_eq!(counts.workers, 2);
    }

    #[test]
    fn test_supervisor_which_children() {
        let spec = SupervisorSpec::new(RestartStrategy::OneForOne)
            .child(ChildSpec::new("first", mock_start))
            .child(ChildSpec::new("second", mock_start))
            .child(ChildSpec::new("third", mock_start));

        let mut state = SupervisorState::new(spec);
        state.start_children().unwrap();

        let children = state.which_children();
        assert_eq!(children, vec!["first", "second", "third"]);
    }

    #[test]
    fn test_supervisor_terminate_child() {
        let spec = SupervisorSpec::new(RestartStrategy::OneForOne)
            .child(ChildSpec::new("worker1", mock_start));

        let mut state = SupervisorState::new(spec);
        state.start_children().unwrap();

        assert!(state.terminate_child("worker1").is_ok());
        let counts = state.count_children();
        assert_eq!(counts.active, 0);
        assert_eq!(counts.specs, 1); // Spec still exists
    }

    #[test]
    fn test_supervisor_delete_child() {
        let spec = SupervisorSpec::new(RestartStrategy::OneForOne)
            .child(ChildSpec::new("worker1", mock_start));

        let mut state = SupervisorState::new(spec);
        state.start_children().unwrap();

        assert!(state.delete_child("worker1").is_ok());
        let counts = state.count_children();
        assert_eq!(counts.specs, 0);
    }

    #[test]
    fn test_supervisor_restart_child() {
        let counter = Arc::new(AtomicU32::new(0));
        let start_fn = counted_start(counter.clone());

        let spec = SupervisorSpec::new(RestartStrategy::OneForOne)
            .child(ChildSpec::new("worker1", start_fn));

        let mut state = SupervisorState::new(spec);
        state.start_children().unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 1);

        state.restart_child("worker1").unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 2);

        let info = state.get_child_info("worker1").unwrap();
        assert_eq!(info.restart_count, 1);
    }

    #[test]
    fn test_supervisor_dynamic_child() {
        let spec = SupervisorSpec::new(RestartStrategy::OneForOne);

        let mut state = SupervisorState::new(spec);
        state.start_children().unwrap();

        let pid = state
            .start_child(ChildSpec::new("dynamic1", mock_start))
            .unwrap();
        assert!(state.pid_to_child.contains_key(&pid));

        let counts = state.count_children();
        assert_eq!(counts.specs, 1);
        assert_eq!(counts.active, 1);
    }

    #[test]
    fn test_supervisor_one_for_one_strategy() {
        let spec = SupervisorSpec::new(RestartStrategy::OneForOne)
            .child(ChildSpec::new("worker1", mock_start))
            .child(ChildSpec::new("worker2", mock_start))
            .child(ChildSpec::new("worker3", mock_start));

        let mut state = SupervisorState::new(spec);
        state.start_children().unwrap();

        // Get worker2's pid
        let worker2_pid = state.get_child_info("worker2").unwrap().pid.unwrap();

        // Simulate worker2 crash
        let to_restart = state
            .handle_child_exit(worker2_pid, &ExitReason::Error("crash".to_string()))
            .unwrap();

        // Only worker2 should be restarted
        assert_eq!(to_restart, vec!["worker2"]);
    }

    #[test]
    fn test_supervisor_one_for_all_strategy() {
        let spec = SupervisorSpec::new(RestartStrategy::OneForAll)
            .child(ChildSpec::new("worker1", mock_start))
            .child(ChildSpec::new("worker2", mock_start))
            .child(ChildSpec::new("worker3", mock_start));

        let mut state = SupervisorState::new(spec);
        state.start_children().unwrap();

        let worker2_pid = state.get_child_info("worker2").unwrap().pid.unwrap();

        let to_restart = state
            .handle_child_exit(worker2_pid, &ExitReason::Error("crash".to_string()))
            .unwrap();

        // All children should be restarted
        assert_eq!(to_restart, vec!["worker1", "worker2", "worker3"]);
    }

    #[test]
    fn test_supervisor_rest_for_one_strategy() {
        let spec = SupervisorSpec::new(RestartStrategy::RestForOne)
            .child(ChildSpec::new("worker1", mock_start))
            .child(ChildSpec::new("worker2", mock_start))
            .child(ChildSpec::new("worker3", mock_start));

        let mut state = SupervisorState::new(spec);
        state.start_children().unwrap();

        let worker2_pid = state.get_child_info("worker2").unwrap().pid.unwrap();

        let to_restart = state
            .handle_child_exit(worker2_pid, &ExitReason::Error("crash".to_string()))
            .unwrap();

        // worker2 and worker3 should be restarted (not worker1)
        assert_eq!(to_restart, vec!["worker2", "worker3"]);
    }

    #[test]
    fn test_supervisor_transient_normal_exit() {
        let spec = SupervisorSpec::new(RestartStrategy::OneForOne)
            .child(ChildSpec::new("worker1", mock_start).transient());

        let mut state = SupervisorState::new(spec);
        state.start_children().unwrap();

        let worker1_pid = state.get_child_info("worker1").unwrap().pid.unwrap();

        // Normal exit should not restart transient child
        let to_restart = state
            .handle_child_exit(worker1_pid, &ExitReason::Normal)
            .unwrap();

        assert!(to_restart.is_empty());
    }

    #[test]
    fn test_supervisor_transient_abnormal_exit() {
        let spec = SupervisorSpec::new(RestartStrategy::OneForOne)
            .child(ChildSpec::new("worker1", mock_start).transient());

        let mut state = SupervisorState::new(spec);
        state.start_children().unwrap();

        let worker1_pid = state.get_child_info("worker1").unwrap().pid.unwrap();

        // Abnormal exit should restart transient child
        let to_restart = state
            .handle_child_exit(worker1_pid, &ExitReason::Error("crash".to_string()))
            .unwrap();

        assert_eq!(to_restart, vec!["worker1"]);
    }

    #[test]
    fn test_supervisor_temporary_never_restart() {
        let spec = SupervisorSpec::new(RestartStrategy::OneForOne)
            .child(ChildSpec::new("worker1", mock_start).temporary());

        let mut state = SupervisorState::new(spec);
        state.start_children().unwrap();

        let worker1_pid = state.get_child_info("worker1").unwrap().pid.unwrap();

        // Temporary children are never restarted
        let to_restart = state
            .handle_child_exit(worker1_pid, &ExitReason::Error("crash".to_string()))
            .unwrap();

        assert!(to_restart.is_empty());
    }

    #[test]
    fn test_supervisor_restart_intensity() {
        let spec = SupervisorSpec::new(RestartStrategy::OneForOne)
            .max_restarts(2, Duration::from_secs(10))
            .child(ChildSpec::new("worker1", mock_start));

        let mut state = SupervisorState::new(spec);
        state.start_children().unwrap();

        // First restart should work
        assert!(state.restart_child("worker1").is_ok());

        // Second restart should work
        assert!(state.restart_child("worker1").is_ok());

        // Third restart should fail (exceeded max_restarts=2)
        let result = state.restart_child("worker1");
        assert_eq!(result, Err(SupervisorError::MaxRestartsExceeded));
    }

    #[test]
    fn test_supervisor_child_not_found() {
        let spec = SupervisorSpec::new(RestartStrategy::OneForOne);
        let mut state = SupervisorState::new(spec);

        assert_eq!(
            state.terminate_child("nonexistent"),
            Err(SupervisorError::ChildNotFound("nonexistent".to_string()))
        );
    }

    #[test]
    fn test_supervisor_duplicate_child() {
        let spec = SupervisorSpec::new(RestartStrategy::OneForOne)
            .child(ChildSpec::new("worker1", mock_start));

        let mut state = SupervisorState::new(spec);
        state.start_children().unwrap();

        let result = state.start_child(ChildSpec::new("worker1", mock_start));
        assert_eq!(
            result,
            Err(SupervisorError::ChildAlreadyExists("worker1".to_string()))
        );
    }

    #[test]
    fn test_supervisor_shutdown_prevents_operations() {
        let spec = SupervisorSpec::new(RestartStrategy::OneForOne)
            .child(ChildSpec::new("worker1", mock_start));

        let mut state = SupervisorState::new(spec);
        state.start_children().unwrap();

        state.begin_shutdown();

        // Operations should fail during shutdown
        assert_eq!(
            state.start_child(ChildSpec::new("new_worker", mock_start)),
            Err(SupervisorError::ShuttingDown)
        );

        assert_eq!(
            state.restart_child("worker1"),
            Err(SupervisorError::ShuttingDown)
        );
    }

    #[test]
    fn test_supervisor_counts() {
        let spec = SupervisorSpec::new(RestartStrategy::OneForOne)
            .child(ChildSpec::new("worker1", mock_start))
            .child(
                ChildSpec::new("sub_supervisor", mock_start).child_type(ChildType::Supervisor),
            );

        let mut state = SupervisorState::new(spec);
        state.start_children().unwrap();

        let counts = state.count_children();
        assert_eq!(counts.specs, 2);
        assert_eq!(counts.active, 2);
        assert_eq!(counts.workers, 1);
        assert_eq!(counts.supervisors, 1);
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
}
