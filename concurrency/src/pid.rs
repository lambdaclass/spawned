//! Process Identity types for spawned actors.
//!
//! This module provides the foundational types for process identification:
//! - `Pid`: A unique identifier for each actor/process
//! - `ExitReason`: Why a process terminated
//!
//! Unlike `GenServerHandle<G>`, `Pid` is:
//! - Type-erased (can reference any actor)
//! - Serializable (for future distribution support)
//! - Lightweight (just a u64 + generation counter)

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

/// Global counter for generating unique Pids.
/// Monotonically increasing, never reused.
static NEXT_PID_ID: AtomicU64 = AtomicU64::new(1);

/// A unique process identifier.
///
/// Each actor in the system has a unique `Pid` that identifies it.
/// Pids are cheap to copy and compare.
///
/// # Example
///
/// ```ignore
/// let handle = MyServer::new().start();
/// let pid = handle.pid();
/// println!("Started server with pid: {}", pid);
/// ```
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Pid {
    /// Unique identifier on this node.
    /// Monotonically increasing, never reused within a process lifetime.
    id: u64,
}

impl Pid {
    /// Create a new unique Pid.
    ///
    /// This is called internally when starting a new GenServer.
    /// Each call returns a Pid with a unique id.
    pub(crate) fn new() -> Self {
        Self {
            id: NEXT_PID_ID.fetch_add(1, Ordering::Relaxed),
        }
    }

    /// Get the raw numeric ID.
    ///
    /// Useful for debugging and logging.
    pub fn id(&self) -> u64 {
        self.id
    }
}

impl fmt::Debug for Pid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Pid({})", self.id)
    }
}

impl fmt::Display for Pid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<0.{}>", self.id)
    }
}

/// The reason why a process exited.
///
/// This is used by supervision trees and process linking to understand
/// how a process terminated and whether it should be restarted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExitReason {
    /// Normal termination - the process completed successfully.
    /// Supervisors typically don't restart processes that exit normally.
    Normal,

    /// Graceful shutdown requested.
    /// The process was asked to stop and did so cleanly.
    Shutdown,

    /// The process was forcefully killed.
    Kill,

    /// The process crashed with an error.
    Error(String),

    /// The process exited because a linked process exited.
    /// Contains the pid of the linked process and its exit reason.
    Linked {
        pid: Pid,
        reason: Box<ExitReason>,
    },
}

impl ExitReason {
    /// Returns true if this is a "normal" exit (Normal or Shutdown).
    ///
    /// Used by supervisors to decide whether to restart a child.
    pub fn is_normal(&self) -> bool {
        matches!(self, ExitReason::Normal | ExitReason::Shutdown)
    }

    /// Returns true if this exit reason indicates an error/crash.
    pub fn is_error(&self) -> bool {
        !self.is_normal()
    }

    /// Create an error exit reason from any error type.
    pub fn from_error<E: std::error::Error>(err: E) -> Self {
        ExitReason::Error(err.to_string())
    }

    /// Create an error exit reason from a string.
    pub fn error(msg: impl Into<String>) -> Self {
        ExitReason::Error(msg.into())
    }
}

impl fmt::Display for ExitReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExitReason::Normal => write!(f, "normal"),
            ExitReason::Shutdown => write!(f, "shutdown"),
            ExitReason::Kill => write!(f, "killed"),
            ExitReason::Error(msg) => write!(f, "error: {}", msg),
            ExitReason::Linked { pid, reason } => {
                write!(f, "linked process {} exited: {}", pid, reason)
            }
        }
    }
}

impl std::error::Error for ExitReason {}

/// Trait for types that have an associated Pid.
///
/// Implemented by `GenServerHandle<G>` and other handle types.
pub trait HasPid {
    /// Get the Pid of the associated process.
    fn pid(&self) -> Pid;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pid_uniqueness() {
        let pid1 = Pid::new();
        let pid2 = Pid::new();
        let pid3 = Pid::new();

        assert_ne!(pid1, pid2);
        assert_ne!(pid2, pid3);
        assert_ne!(pid1, pid3);

        // IDs should be monotonically increasing
        assert!(pid1.id() < pid2.id());
        assert!(pid2.id() < pid3.id());
    }

    #[test]
    fn pid_clone_equality() {
        let pid1 = Pid::new();
        let pid2 = pid1;

        assert_eq!(pid1, pid2);
        assert_eq!(pid1.id(), pid2.id());
    }

    #[test]
    fn pid_display() {
        let pid = Pid::new();
        let display = format!("{}", pid);
        assert!(display.starts_with("<0."));
        assert!(display.ends_with(">"));
    }

    #[test]
    fn exit_reason_is_normal() {
        assert!(ExitReason::Normal.is_normal());
        assert!(ExitReason::Shutdown.is_normal());
        assert!(!ExitReason::Kill.is_normal());
        assert!(!ExitReason::Error("oops".to_string()).is_normal());
        assert!(!ExitReason::Linked {
            pid: Pid::new(),
            reason: Box::new(ExitReason::Kill),
        }
        .is_normal());
    }

    #[test]
    fn exit_reason_display() {
        assert_eq!(format!("{}", ExitReason::Normal), "normal");
        assert_eq!(format!("{}", ExitReason::Shutdown), "shutdown");
        assert_eq!(format!("{}", ExitReason::Kill), "killed");
        assert_eq!(
            format!("{}", ExitReason::Error("connection lost".to_string())),
            "error: connection lost"
        );
    }
}
