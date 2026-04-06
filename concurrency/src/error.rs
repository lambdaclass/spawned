/// Reason an actor stopped. Used by supervision to decide whether to restart.
#[derive(Debug, Clone, PartialEq)]
pub enum ExitReason {
    /// Clean stop via `ctx.stop()` or channel closure.
    Normal,
    /// Ordered shutdown from a supervisor or linked actor.
    /// Not yet produced — reserved for supervision tree implementation.
    Shutdown,
    /// Actor panicked in `started()`, a handler, or `stopped()`.
    Panic(String),
    /// Untrappable kill signal. Bypasses `trap_exit` and skips `stopped()` cleanup.
    /// Currently produced as a fallback when an async task is aborted externally.
    /// Will also be used by supervision for `ShutdownType::BrutalKill`.
    Kill,
}

impl ExitReason {
    /// Returns `true` for exit reasons that should trigger a restart
    /// of `Transient` or `Permanent` children.
    /// `Shutdown` is considered normal (no restart) matching Erlang semantics.
    pub fn is_abnormal(&self) -> bool {
        match self {
            ExitReason::Normal | ExitReason::Shutdown => false,
            ExitReason::Panic(_) | ExitReason::Kill => true,
        }
    }
}

impl std::fmt::Display for ExitReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExitReason::Normal => write!(f, "normal"),
            ExitReason::Shutdown => write!(f, "shutdown"),
            ExitReason::Panic(msg) => write!(f, "panic: {msg}"),
            ExitReason::Kill => write!(f, "kill"),
        }
    }
}

/// Extract a human-readable message from a panic payload.
pub(crate) fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = panic.downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = panic.downcast_ref::<String>() {
        s.clone()
    } else {
        format!("{panic:?}")
    }
}

/// Errors that can occur when communicating with an actor.
#[derive(Debug, thiserror::Error)]
pub enum ActorError {
    /// The actor has stopped or its mailbox channel is closed.
    ///
    /// Returned by `send()` and `request()` when the actor is no longer running.
    #[error("Actor stopped")]
    ActorStopped,
    /// A request exceeded the timeout duration (default: 5 seconds).
    ///
    /// Returned by `request()` and `request_with_timeout()` when the actor
    /// does not reply in time.
    #[error("Request to Actor timed out")]
    RequestTimeout,
}

impl<T> From<spawned_rt::threads::mpsc::SendError<T>> for ActorError {
    fn from(_value: spawned_rt::threads::mpsc::SendError<T>) -> Self {
        Self::ActorStopped
    }
}

impl<T> From<spawned_rt::tasks::mpsc::SendError<T>> for ActorError {
    fn from(_value: spawned_rt::tasks::mpsc::SendError<T>) -> Self {
        Self::ActorStopped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_into_std_error() {
        let error: &dyn std::error::Error = &ActorError::ActorStopped;
        assert_eq!(error.to_string(), "Actor stopped");
    }

    #[test]
    fn exit_reason_is_abnormal_classification() {
        assert!(!ExitReason::Normal.is_abnormal());
        assert!(!ExitReason::Shutdown.is_abnormal());
        assert!(ExitReason::Panic("boom".into()).is_abnormal());
        assert!(ExitReason::Kill.is_abnormal());
    }

    #[test]
    fn exit_reason_display() {
        assert_eq!(ExitReason::Normal.to_string(), "normal");
        assert_eq!(ExitReason::Shutdown.to_string(), "shutdown");
        assert_eq!(ExitReason::Panic("oops".into()).to_string(), "panic: oops");
        assert_eq!(ExitReason::Kill.to_string(), "kill");
    }

    #[test]
    fn exit_reason_partial_eq() {
        assert_eq!(ExitReason::Normal, ExitReason::Normal);
        assert_ne!(ExitReason::Normal, ExitReason::Kill);
        assert_eq!(ExitReason::Panic("x".into()), ExitReason::Panic("x".into()));
    }
}
