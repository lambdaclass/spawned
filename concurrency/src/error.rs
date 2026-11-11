/// Reason an actor stopped. Used by supervision to decide whether to restart.
///
/// Non-exhaustive, as OTP exit reasons are open terms: match with a wildcard
/// arm.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum ExitReason {
    /// Clean stop via `ctx.stop()` or `ChildHandle::stop()`. The run loop holds
    /// a sender to its own mailbox, so the mailbox never closes while it runs.
    Normal,
    /// Ordered shutdown from a supervisor or linked actor.
    Shutdown,
    /// Actor panicked in `started()`, a handler, or `stopped()`.
    Panic(String),
    /// A self-exit with `Kill` (`ctx.stop_with_reason(Kill)`, OTP `exit(kill)`),
    /// an administrative `stop_with_reason(Kill)`, or a `Kill` link notice.
    /// Propagated over links as `Kill` (trappable), not converted to `Killed`.
    Kill,
    /// Killed by an untrappable kill signal ([`ChildHandle::kill`],
    /// `exit(Kill)`), or dropped by its runtime before it finished (OTP
    /// `killed`). Links receive `Killed`, which is trappable.
    ///
    /// [`ChildHandle::kill`]: crate::ChildHandle::kill
    Killed,
    /// The target no longer existed when a link, monitor, send or request
    /// was made (OTP `noproc`).
    NoProc,
}

impl ExitReason {
    /// Returns `true` for exit reasons that should trigger a restart
    /// of `Transient` or `Permanent` children.
    /// `Shutdown` is considered normal (no restart) matching Erlang semantics.
    pub fn is_abnormal(&self) -> bool {
        match self {
            ExitReason::Normal | ExitReason::Shutdown => false,
            ExitReason::Panic(_) | ExitReason::Kill | ExitReason::Killed | ExitReason::NoProc => {
                true
            }
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
            ExitReason::Killed => write!(f, "killed"),
            ExitReason::NoProc => write!(f, "noproc"),
        }
    }
}

/// The target of [`try_link`](crate::tasks::Context::try_link) no longer
/// existed (it was past its death point), as OTP's `noproc`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("noproc")]
pub struct NoProc;

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
///
/// Non-exhaustive: later releases may add errors. Match with a wildcard arm.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ActorError {
    /// The actor died before answering, with its final exit reason (OTP:
    /// `gen_server:call` exits with `{Reason, {gen_server, call, ..}}`).
    ///
    /// A request that was queued, deferred or being handled when the actor
    /// died is answered at its death point with the reason that `wait_exit`,
    /// `Down` and linked actors see: `Killed` after a kill, the signal's
    /// reason after an untrapped exit signal, `Panic(..)` after a crash.
    #[error("Actor stopped: {0}")]
    ActorStopped(ExitReason),
    /// The actor no longer existed when the message or request was sent: it
    /// was past its death point (OTP `noproc`). A send or request made while
    /// the actor is still exiting is admitted instead, and a request is then
    /// answered `ActorStopped(reason)` at its death.
    #[error("noproc")]
    NoProc,
    /// A request exceeded the timeout duration (default: 5 seconds).
    ///
    /// Returned by `request()` and `request_with_timeout()` when the actor
    /// does not reply in time.
    #[error("Request to Actor timed out")]
    RequestTimeout,
    /// The handler dropped the request's reply obligation (a `PendingReply`
    /// or a deferred `ReplyTo`) without answering it.
    #[error("Reply dropped by the actor")]
    ReplyDropped,
    /// The actor rejected the request at a bound (for example its deferred
    /// replies are at `MAX_OUTSTANDING`).
    #[error("Actor overloaded")]
    Overloaded,
}

impl<T> From<spawned_rt::threads::mpsc::SendError<T>> for ActorError {
    fn from(_value: spawned_rt::threads::mpsc::SendError<T>) -> Self {
        Self::NoProc
    }
}

#[cfg(feature = "async")]
impl<T> From<spawned_rt::tasks::mpsc::SendError<T>> for ActorError {
    fn from(_value: spawned_rt::tasks::mpsc::SendError<T>) -> Self {
        Self::NoProc
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_into_std_error() {
        let error: &dyn std::error::Error = &ActorError::ActorStopped(ExitReason::Killed);
        assert_eq!(error.to_string(), "Actor stopped: killed");
        assert_eq!(ActorError::NoProc.to_string(), "noproc");
    }

    #[test]
    fn exit_reason_is_abnormal_classification() {
        assert!(!ExitReason::Normal.is_abnormal());
        assert!(!ExitReason::Shutdown.is_abnormal());
        assert!(ExitReason::Panic("boom".into()).is_abnormal());
        assert!(ExitReason::Kill.is_abnormal());
        assert!(ExitReason::Killed.is_abnormal());
        assert!(ExitReason::NoProc.is_abnormal());
    }

    #[test]
    fn exit_reason_display() {
        assert_eq!(ExitReason::Normal.to_string(), "normal");
        assert_eq!(ExitReason::Shutdown.to_string(), "shutdown");
        assert_eq!(ExitReason::Panic("oops".into()).to_string(), "panic: oops");
        assert_eq!(ExitReason::Kill.to_string(), "kill");
        assert_eq!(ExitReason::Killed.to_string(), "killed");
        assert_eq!(ExitReason::NoProc.to_string(), "noproc");
    }

    #[test]
    fn exit_reason_partial_eq() {
        assert_eq!(ExitReason::Normal, ExitReason::Normal);
        assert_ne!(ExitReason::Normal, ExitReason::Kill);
        assert_eq!(ExitReason::Panic("x".into()), ExitReason::Panic("x".into()));
    }
}
