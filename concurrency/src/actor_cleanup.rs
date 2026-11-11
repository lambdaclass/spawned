//! Shared terminal publication for actor futures and native threads.
use crate::{
    child_handle::ActorId,
    error::ExitReason,
    link::{LinkNotices, LinkTable, LinkedExitReason},
    monitor::{MonitorCleanup, MonitorTable},
};
use spawned_rt::sync::watch;

/// Subscriber code must not replace a lifecycle failure or interrupt recovery.
pub(crate) fn log_safely(log: impl FnOnce()) {
    if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(log)) {
        crate::mailbox::dispose_panic(payload);
    }
}

/// Constructed before dispatch, so even a never-polled actor has cleanup.
/// [`die`](Self::die) is the death point D; dropping it is publication.
pub(crate) struct ActorCleanup<C: Fn()> {
    id: ActorId,
    metrics: crate::instrumentation::ActorObservation,
    links: LinkTable,
    cause: LinkedExitReason,
    monitors: Option<MonitorCleanup>,
    completion: watch::Sender<Option<ExitReason>>,
    reason: Option<ExitReason>,
    notices: Option<LinkNotices>,
    cancel: C,
    failures: Option<std::sync::Arc<crate::mailbox::DropFailures>>,
    /// The reply registry, sealed with the final reason at D.
    replies: Option<std::sync::Arc<crate::reply::registry::Registry>>,
}
impl<C: Fn()> ActorCleanup<C> {
    pub(crate) fn new(
        id: ActorId,
        links: LinkTable,
        cause: LinkedExitReason,
        monitors: MonitorTable,
        completion: watch::Sender<Option<ExitReason>>,
        cancel: C,
    ) -> Self {
        Self {
            id,
            metrics: crate::instrumentation::ActorObservation::new(),
            links,
            cause,
            monitors: Some(MonitorCleanup(monitors)),
            completion,
            reason: None,
            notices: None,
            cancel,
            failures: None,
            replies: None,
        }
    }
    /// Seal `replies` with the final reason at D, inside the exit-table
    /// critical section: callers then get `ActorStopped(final reason)` and
    /// later sends `NoProc`.
    pub(crate) fn with_replies(
        mut self,
        replies: std::sync::Arc<crate::reply::registry::Registry>,
    ) -> Self {
        self.replies = Some(replies);
        self
    }
    pub(crate) fn with_mailbox_failures(
        mut self,
        failures: std::sync::Arc<crate::mailbox::DropFailures>,
    ) -> Self {
        self.failures = Some(failures);
        self
    }
    /// The death point D: the requested reason becomes final and the link
    /// table closes, taking one notice per link (delivered at publication).
    /// Idempotent; returns the final reason.
    pub(crate) fn die(&mut self) -> ExitReason {
        if let Some(reason) = &self.reason {
            return reason.clone();
        }
        let reason = crate::link::lock_request(&self.cause).die();
        self.close(reason)
    }
    fn close(&mut self, reason: ExitReason) -> ExitReason {
        let replies = self.replies.as_deref();
        self.notices = Some(crate::link::close_links_at_death(
            self.id,
            &self.links,
            &reason,
            |reason| {
                if let Some(replies) = replies {
                    replies.seal(reason);
                }
            },
        ));
        self.reason = Some(reason.clone());
        reason
    }
    /// Record a cleanup failure after D (state or queued-envelope
    /// destructor): it never changes the final reason.
    pub(crate) fn cleanup_failed(&self, message: String) {
        if let Some(failures) = &self.failures {
            failures.record(message);
        }
    }
}
impl<C: Fn()> Drop for ActorCleanup<C> {
    fn drop(&mut self) {
        // Never reached D: the runtime dropped the actor (G1).
        let reason = match self.reason.clone() {
            Some(reason) => reason,
            None => {
                let reason = crate::link::lock_request(&self.cause).runtime_drop();
                self.close(reason)
            }
        };
        if let Some(message) = self.failures.as_ref().and_then(|f| f.first()) {
            // Subscriber code must not unwind through a receiver/actor teardown.
            log_safely(|| {
                tracing::warn!(event = "actor_cleanup_failed", actor = %self.id, %message, %reason,
                    "destructor failed after the death point");
            });
        }
        (self.cancel)();
        drop(self.monitors.take());
        self.metrics.finish(&reason);
        // Publication: the link notices taken at D are delivered, then
        // wait_exit, join and monitors observe the final reason. Delivering
        // first keeps v0.7's guarantee that once `wait_exit` returns, every
        // linked peer's `Exit` is already queued (or its exit requested).
        if let Some(notices) = self.notices.take() {
            notices.deliver();
        }
        self.completion.send_replace(Some(reason));
    }
}

/// Destroy state after D. A destructor panic never changes the final reason;
/// it is returned as a cleanup failure.
pub(crate) fn finish_actor_state<A>(actor: A) -> Option<String> {
    let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(actor)))
    else {
        return None;
    };
    let message = crate::error::panic_message(&*payload);
    crate::mailbox::dispose_panic(payload);
    log_safely(|| {
        tracing::warn!(event = "actor_state_cleanup_failed", actor_type = std::any::type_name::<A>(),
            secondary = %message, "actor state destructor panicked after the death point");
    });
    Some(format!("actor state destructor: {message}"))
}

/// Captured as one value by the dispatch closure. Declaration order makes
/// never-polled payload destruction precede completion publication, without
/// relying on the compiler's order for disjoint closure captures.
pub(crate) struct ActorStartup<P, C: Fn()> {
    payload: Option<P>,
    cleanup: Option<ActorCleanup<C>>,
}
impl<P, C: Fn()> ActorStartup<P, C> {
    pub(crate) fn new(payload: P, cleanup: ActorCleanup<C>) -> Self {
        Self {
            payload: Some(payload),
            cleanup: Some(cleanup),
        }
    }
    pub(crate) fn into_parts(mut self) -> (P, ActorCleanup<C>) {
        (self.payload.take().unwrap(), self.cleanup.take().unwrap())
    }
}
impl<P, C: Fn()> Drop for ActorStartup<P, C> {
    fn drop(&mut self) {
        if let Some(payload) = self.payload.take() {
            if let Err(panic) =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(payload)))
            {
                let message = crate::error::panic_message(&*panic);
                crate::mailbox::dispose_panic(panic);
                if let Some(cleanup) = &mut self.cleanup {
                    cleanup.cleanup_failed(format!("actor startup payload destructor: {message}"));
                }
            }
        }
    }
}

#[cfg(test)]
mod state_drop_tests {
    use super::*;
    struct Explodes;
    impl Drop for Explodes {
        fn drop(&mut self) {
            panic!("secondary state failure");
        }
    }
    #[test]
    fn destructor_failure_is_a_cleanup_failure_not_a_reason() {
        assert_eq!(
            finish_actor_state(Explodes),
            Some("actor state destructor: secondary state failure".into())
        );
        assert_eq!(finish_actor_state(()), None);
    }
}
