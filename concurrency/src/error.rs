/// Reason an actor stopped. Used by supervision to decide whether to restart.
#[derive(Debug, Clone)]
pub enum ExitReason {
    /// Clean stop via `ctx.stop()` or channel closure.
    Normal,
    /// Ordered shutdown from a supervisor or linked actor.
    Shutdown,
    /// Actor panicked in `started()`, a handler, or `stopped()`.
    Panic(String),
    /// Untrappable kill signal.
    Kill,
}

impl ExitReason {
    /// Returns `true` for exit reasons that should trigger a restart
    /// of `Transient` or `Permanent` children.
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
}
