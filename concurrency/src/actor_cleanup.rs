//! Shared terminal publication for actor futures and native threads.
use crate::{
    child_handle::ActorId,
    error::ExitReason,
    link::{LinkCleanup, LinkTable, LinkedExitReason},
    monitor::{MonitorCleanup, MonitorTable},
};
use spawned_rt::sync::watch;

/// Constructed before dispatch, so even a never-polled actor has cleanup.
/// Completion owns its sender until links and outgoing monitors are closed.
pub(crate) struct ActorCleanup<C: Fn()> {
    id: ActorId,
    metrics: crate::instrumentation::ActorObservation,
    links: LinkCleanup,
    cause: LinkedExitReason,
    monitors: Option<MonitorCleanup>,
    completion: watch::Sender<Option<ExitReason>>,
    fallback: ExitReason,
    reason: Option<ExitReason>,
    cancel: C,
    mailbox_failures: Option<std::sync::Arc<crate::mailbox::DropFailures>>,
}
impl<C: Fn()> ActorCleanup<C> {
    pub(crate) fn new(
        id: ActorId,
        links: LinkTable,
        cause: LinkedExitReason,
        monitors: MonitorTable,
        completion: watch::Sender<Option<ExitReason>>,
        fallback: ExitReason,
        cancel: C,
    ) -> Self {
        Self {
            id,
            metrics: crate::instrumentation::ActorObservation::new(),
            links: LinkCleanup::new(id, links),
            cause,
            monitors: Some(MonitorCleanup(monitors)),
            completion,
            fallback,
            reason: None,
            cancel,
            mailbox_failures: None,
        }
    }
    pub(crate) fn with_mailbox_failures(
        mut self,
        failures: std::sync::Arc<crate::mailbox::DropFailures>,
    ) -> Self {
        self.mailbox_failures = Some(failures);
        self
    }
    pub(crate) fn complete(&mut self, reason: ExitReason) {
        self.complete_inner(reason, false);
    }
    pub(crate) fn complete_secondary(&mut self, reason: ExitReason) {
        self.complete_inner(reason, true);
    }
    fn complete_inner(&mut self, mut reason: ExitReason, secondary: bool) {
        let mut cause = self.cause.lock().unwrap_or_else(|p| p.into_inner());
        if secondary {
            if let Some(committed) = cause.as_ref().filter(|c| **c != ExitReason::Normal) {
                reason = committed.clone();
            }
        }
        if matches!(reason, ExitReason::Normal) {
            if let Some(linked) = cause.as_ref() {
                reason = linked.clone();
            }
            if reason == ExitReason::Normal {
                if let Some(message) = self.mailbox_failures.as_ref().and_then(|f| f.first()) {
                    reason = ExitReason::Panic(format!("queued message destructor: {message}"));
                }
            }
        }
        // Freeze the completed outcome before any further cancellation callbacks.
        *cause = Some(reason.clone());
        self.reason = Some(reason);
    }
}
impl<C: Fn()> Drop for ActorCleanup<C> {
    fn drop(&mut self) {
        let completed = self.reason.is_some();
        let failure = self.mailbox_failures.as_ref().and_then(|f| f.first());
        let mut reason = self.reason.take().unwrap_or_else(|| self.fallback.clone());
        {
            let mut cause = self.cause.lock().unwrap_or_else(|p| p.into_inner());
            if !completed {
                if let Some(committed) = cause.as_ref().filter(|c| **c != ExitReason::Normal) {
                    reason = committed.clone();
                }
            }
            // Queued destructors are secondary to committed Shutdown/Kill or
            // a linked/handler panic. Only a completed Normal lifecycle gains
            // a new panic outcome; aborted tasks keep their abort outcome.
            if completed && reason == ExitReason::Normal {
                if let Some(message) = &failure {
                    reason = ExitReason::Panic(format!("queued message destructor: {message}"));
                }
            }
            *cause = Some(reason.clone());
        }
        if let Some(message) = failure {
            // Subscriber code must not unwind through a receiver/actor teardown.
            if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                tracing::warn!(event = "mailbox_cleanup_failed", actor = %self.id, %message, %reason,
                    "queued message destructor failed");
            })) {
                crate::mailbox::dispose_panic(payload);
            }
        }
        (self.cancel)();
        drop(self.monitors.take());
        self.links.complete(&reason);
        self.metrics.finish(&reason);
        self.completion.send_replace(Some(reason));
    }
}

/// Destroy state after capturing the lifecycle outcome. A state destructor is
/// secondary to an already observed handler/start/stop panic, even before that
/// outcome has been committed to the shared exit slot.
pub(crate) fn finish_actor_state<A>(
    actor: A,
    primary: Result<ExitReason, ExitReason>,
) -> Result<ExitReason, ExitReason> {
    let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(actor)))
    else {
        return primary;
    };
    let message = crate::error::panic_message(&*payload);
    crate::mailbox::dispose_panic(payload);
    let keep_primary = match &primary {
        Ok(reason) => *reason != ExitReason::Normal,
        Err(_) => true,
    };
    if keep_primary {
        if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            tracing::warn!(event = "actor_state_cleanup_failed", actor_type = std::any::type_name::<A>(),
                ?primary, secondary = %message, "actor state destructor panicked after a terminal outcome");
        })) {
            crate::mailbox::dispose_panic(payload);
        }
        primary
    } else {
        Err(ExitReason::Panic(format!(
            "actor state destructor: {message}"
        )))
    }
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
                    cleanup.complete_secondary(ExitReason::Panic(format!(
                        "actor startup payload destructor: {message}"
                    )));
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
    fn destructor_failure_only_escalates_normal_even_without_a_committed_cause() {
        for primary in [
            Ok(ExitReason::Normal),
            Ok(ExitReason::Shutdown),
            Ok(ExitReason::Kill),
            Ok(ExitReason::Panic("handler failure".into())),
            Err(ExitReason::Panic("lifecycle unwind".into())),
        ] {
            let expected = if primary == Ok(ExitReason::Normal) {
                Err(ExitReason::Panic(
                    "actor state destructor: secondary state failure".into(),
                ))
            } else {
                primary.clone()
            };
            assert_eq!(finish_actor_state(Explodes, primary), expected);
        }
    }
}
