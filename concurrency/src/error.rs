//! Error types for spawned-concurrency.
//!
//! This module provides error types with rich context for debugging.

use crate::pid::Pid;

/// Error type for GenServer operations.
#[derive(Debug, thiserror::Error)]
pub enum GenServerError {
    /// A callback (handle_call, handle_cast, handle_info) panicked.
    #[error("callback panicked: {message}")]
    Callback {
        /// Description of what went wrong.
        message: String,
    },

    /// Initialization failed.
    #[error("initialization failed: {reason}")]
    Initialization {
        /// Reason for failure.
        reason: String,
    },

    /// Server encountered an error (e.g., channel closed).
    #[error("server error: {reason}")]
    Server {
        /// Reason for failure.
        reason: String,
    },

    /// Call message not handled by this GenServer.
    #[error("call message not handled by this GenServer")]
    CallMsgUnused,

    /// Cast message not handled by this GenServer.
    #[error("cast message not handled by this GenServer")]
    CastMsgUnused,

    /// Call to GenServer timed out.
    #[error("call timed out after {timeout_ms}ms")]
    CallTimeout {
        /// Timeout in milliseconds.
        timeout_ms: u64,
    },

    /// GenServer is not running.
    #[error("GenServer {pid} is not running")]
    NotRunning {
        /// PID of the GenServer.
        pid: Pid,
    },

    /// Channel was closed unexpectedly.
    #[error("channel closed: {reason}")]
    ChannelClosed {
        /// Reason for closure.
        reason: String,
    },
}

impl GenServerError {
    /// Create a callback error with a message.
    pub fn callback(message: impl Into<String>) -> Self {
        Self::Callback {
            message: message.into(),
        }
    }

    /// Create an initialization error with a reason.
    pub fn initialization(reason: impl Into<String>) -> Self {
        Self::Initialization {
            reason: reason.into(),
        }
    }

    /// Create a server error with a reason.
    pub fn server(reason: impl Into<String>) -> Self {
        Self::Server {
            reason: reason.into(),
        }
    }

    /// Create a call timeout error.
    pub fn call_timeout(timeout_ms: u64) -> Self {
        Self::CallTimeout { timeout_ms }
    }

    /// Create a not running error.
    pub fn not_running(pid: Pid) -> Self {
        Self::NotRunning { pid }
    }

    /// Create a channel closed error.
    pub fn channel_closed(reason: impl Into<String>) -> Self {
        Self::ChannelClosed {
            reason: reason.into(),
        }
    }
}

// Legacy conversion - kept for backwards compatibility
impl Default for GenServerError {
    fn default() -> Self {
        Self::Server {
            reason: "unknown error".to_string(),
        }
    }
}

impl<T> From<spawned_rt::threads::mpsc::SendError<T>> for GenServerError {
    fn from(_value: spawned_rt::threads::mpsc::SendError<T>) -> Self {
        Self::ChannelClosed {
            reason: "send failed - receiver dropped".to_string(),
        }
    }
}

impl<T> From<spawned_rt::tasks::mpsc::SendError<T>> for GenServerError {
    fn from(_value: spawned_rt::tasks::mpsc::SendError<T>) -> Self {
        Self::ChannelClosed {
            reason: "send failed - receiver dropped".to_string(),
        }
    }
}

/// A unified error type that can represent any spawned-concurrency error.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// GenServer error.
    #[error("GenServer error: {0}")]
    GenServer(#[from] GenServerError),

    /// Registry error.
    #[error("Registry error: {0}")]
    Registry(#[from] crate::registry::RegistryError),

    /// Process group error.
    #[error("Process group error: {0}")]
    Pg(#[from] crate::pg::PgError),

    /// Link error.
    #[error("Link error: {0}")]
    Link(#[from] crate::process_table::LinkError),

    /// Supervisor error.
    #[error("Supervisor error: {0}")]
    Supervisor(#[from] crate::supervisor::SupervisorError),

    /// Dynamic supervisor error.
    #[error("Dynamic supervisor error: {0}")]
    DynamicSupervisor(#[from] crate::supervisor::DynamicSupervisorError),

    /// Application error.
    #[error("Application error: {0}")]
    Application(#[from] crate::application::ApplicationError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_into_std_error() {
        let error: &dyn std::error::Error = &GenServerError::callback("test panic");
        assert_eq!(error.to_string(), "callback panicked: test panic");
    }

    #[test]
    fn test_genserver_error_constructors() {
        let e = GenServerError::callback("handler panicked");
        assert!(matches!(e, GenServerError::Callback { .. }));

        let e = GenServerError::initialization("failed to connect");
        assert!(matches!(e, GenServerError::Initialization { .. }));

        let e = GenServerError::server("internal error");
        assert!(matches!(e, GenServerError::Server { .. }));

        let e = GenServerError::call_timeout(5000);
        assert!(matches!(e, GenServerError::CallTimeout { timeout_ms: 5000 }));

        let pid = Pid::new();
        let e = GenServerError::not_running(pid);
        assert!(matches!(e, GenServerError::NotRunning { .. }));

        let e = GenServerError::channel_closed("receiver dropped");
        assert!(matches!(e, GenServerError::ChannelClosed { .. }));
    }

    #[test]
    fn test_error_display() {
        let e = GenServerError::callback("oops");
        assert_eq!(format!("{}", e), "callback panicked: oops");

        let e = GenServerError::initialization("bad config");
        assert_eq!(format!("{}", e), "initialization failed: bad config");

        let e = GenServerError::call_timeout(5000);
        assert_eq!(format!("{}", e), "call timed out after 5000ms");

        let pid = Pid::new();
        let e = GenServerError::not_running(pid);
        assert!(format!("{}", e).contains("is not running"));
    }

    #[test]
    fn test_unified_error() {
        let gs_error = GenServerError::callback("test");
        let unified: Error = gs_error.into();
        assert!(matches!(unified, Error::GenServer(_)));
    }
}
