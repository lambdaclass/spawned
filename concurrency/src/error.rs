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
    /// of `Transient` children (`Permanent` children restart for every reason).
    /// This classification is separate from link-signal handling: only
    /// `Normal` is ignored by a non-trapping linked actor; `Shutdown` ends it.
    ///
    /// | Reason | `is_abnormal()` |
    /// |---|---|
    /// | `Normal` | `false` |
    /// | `Shutdown` | `false` (an ordered stop, as in Erlang) |
    /// | `Panic(_)` | `true` |
    /// | `Kill` | `true` |
    /// | `Killed` | `true` (OTP `killed`: a kill, a supervisor's kill escalation or a runtime drop) |
    /// | `NoProc` | `true` (OTP `noproc`: the target was already gone) |
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

/// The target of `Context::try_link` no longer
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

/// Classification helpers, so callers need not match every variant (the enum
/// is `#[non_exhaustive]`; a variant added later is classified here).
///
/// | Variant | `target_gone` | `is_timeout` | `is_transient` | `exit_reason` |
/// |---|---|---|---|---|
/// | `ActorStopped(r)` | `true` | `false` | `false` | `Some(r)` |
/// | `NoProc` | `true` | `false` | `false` | `None` |
/// | `RequestTimeout` | `false` | `true` | `true` | `None` |
/// | `ReplyDropped` | `false` | `false` | `false` | `None` |
/// | `Overloaded` | `false` | `false` | `true` | `None` |
impl ActorError {
    /// The target actor is gone: it died before answering
    /// (`ActorStopped`) or was already past its death point (`NoProc`).
    /// Retrying the same `ActorRef` cannot succeed; look the actor up again
    /// (a supervisor may have restarted it under a new reference).
    pub fn target_gone(&self) -> bool {
        matches!(self, ActorError::ActorStopped(_) | ActorError::NoProc)
    }

    /// The request was not answered within its timeout
    /// (`RequestTimeout`). The actor may still be running and may still
    /// handle the request: a timeout does not cancel it.
    pub fn is_timeout(&self) -> bool {
        matches!(self, ActorError::RequestTimeout)
    }

    /// Retrying the same target may succeed: the actor was alive but did
    /// not answer in time (`RequestTimeout`) or rejected the request at a
    /// bound (`Overloaded`). `false` for a gone target, and for
    /// `ReplyDropped`, where the actor is alive but its handler chose not to
    /// answer (a decision or a bug, not a load condition): a retry would
    /// usually get the same result. OTP has no such outcome (a `call` the
    /// server never answers times out); the variant reports a drop at once
    /// so a caller does not wait for its deadline, without claiming that the
    /// same request will succeed later. A handler that means "not now, try
    /// again" answers `reply.overloaded()` (or `token.overloaded()`), which is
    /// transient; a caller whose protocol treats a drop as retryable matches
    /// `ReplyDropped` explicitly.
    pub fn is_transient(&self) -> bool {
        matches!(self, ActorError::RequestTimeout | ActorError::Overloaded)
    }

    /// The final exit reason of an actor that died before answering
    /// (`ActorStopped`). `None` otherwise, including `NoProc` (the target
    /// was already gone; its reason is not known to the caller).
    pub fn exit_reason(&self) -> Option<&ExitReason> {
        match self {
            ActorError::ActorStopped(reason) => Some(reason),
            _ => None,
        }
    }
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

    /// Every `ExitReason`; the exhaustive match (allowed inside the crate)
    /// makes adding a variant fail to compile until it is listed here.
    fn all_exit_reasons() -> Vec<ExitReason> {
        let all = vec![
            ExitReason::Normal,
            ExitReason::Shutdown,
            ExitReason::Panic("boom".into()),
            ExitReason::Kill,
            ExitReason::Killed,
            ExitReason::NoProc,
        ];
        for reason in &all {
            match reason {
                ExitReason::Normal
                | ExitReason::Shutdown
                | ExitReason::Panic(_)
                | ExitReason::Kill
                | ExitReason::Killed
                | ExitReason::NoProc => {}
            }
        }
        all
    }

    /// Every `ActorError`, with the same compile-time completeness check.
    fn all_actor_errors() -> Vec<ActorError> {
        let all = vec![
            ActorError::ActorStopped(ExitReason::Killed),
            ActorError::NoProc,
            ActorError::RequestTimeout,
            ActorError::ReplyDropped,
            ActorError::Overloaded,
        ];
        for error in &all {
            match error {
                ActorError::ActorStopped(_)
                | ActorError::NoProc
                | ActorError::RequestTimeout
                | ActorError::ReplyDropped
                | ActorError::Overloaded => {}
            }
        }
        all
    }

    #[test]
    fn exit_reason_is_abnormal_classification() {
        for reason in all_exit_reasons() {
            let expected = match reason {
                ExitReason::Normal | ExitReason::Shutdown => false,
                ExitReason::Panic(_)
                | ExitReason::Kill
                | ExitReason::Killed
                | ExitReason::NoProc => true,
            };
            assert_eq!(reason.is_abnormal(), expected, "{reason}");
        }
    }

    #[test]
    fn actor_error_classification_covers_every_variant() {
        for error in all_actor_errors() {
            // (target_gone, is_timeout, is_transient, exit_reason)
            let expected = match &error {
                ActorError::ActorStopped(r) => (true, false, false, Some(r.clone())),
                ActorError::NoProc => (true, false, false, None),
                ActorError::RequestTimeout => (false, true, true, None),
                ActorError::ReplyDropped => (false, false, false, None),
                ActorError::Overloaded => (false, false, true, None),
            };
            let actual = (
                error.target_gone(),
                error.is_timeout(),
                error.is_transient(),
                error.exit_reason().cloned(),
            );
            assert_eq!(actual, expected, "{error:?}");
        }
    }

    #[test]
    fn actor_stopped_carries_every_reason() {
        for reason in all_exit_reasons() {
            let error = ActorError::ActorStopped(reason.clone());
            assert!(error.target_gone() && !error.is_transient() && !error.is_timeout());
            assert_eq!(error.exit_reason(), Some(&reason));
        }
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
