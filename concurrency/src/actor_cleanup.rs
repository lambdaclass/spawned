//! Shared terminal publication for actor futures and native threads.
use crate::lifecycle_trace as trace;
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

/// `stopped()` did not run for this exit (decision 2: a kill, an untrapped
/// exit signal or link notice, or the runtime dropping the actor). Logged at
/// `debug` for every skip, whether or not the actor overrides `stopped()`
/// (there is no cheap way to know): an application whose cleanup must run
/// traps exits (`ctx.trap_exit(true)`) so a parent's shutdown runs it.
pub(crate) fn log_stopped_skipped(
    actor: ActorId,
    actor_type: &'static str,
    cause: &'static str,
    reason: Option<&ExitReason>,
) {
    let reason = reason.cloned().unwrap_or(ExitReason::Killed);
    log_safely(|| {
        tracing::debug!(event = "stopped_skipped", actor = %actor, actor_type, cause,
            reason = %reason, "stopped() skipped: the exit was not begun by the actor, \
            an administrative stop or its parent");
    });
}

/// Constructed before dispatch, so even a never-polled actor has cleanup.
/// [`die`](Self::die) is the death point D; dropping it is publication.
///
/// D is one boundary, whatever drives it (the run loop, or the runtime
/// dropping the actor): exit pending for dropped replies, job admission
/// closed and every job cancelled, outgoing replies and exit signals fenced,
/// the final reason fixed with the link table closed and admission sealed,
/// and every open deferred reply answered. The caller then answers its
/// queued requests (the mode's mailbox discharge) and only then destroys
/// payloads and state; publication comes last, when this is dropped.
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
    /// The owner's jobs: admission closes and they are cancelled at D.
    jobs: Option<std::sync::Arc<crate::reply::jobs::JobSet>>,
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
            jobs: None,
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
    pub(crate) fn with_jobs(mut self, jobs: std::sync::Arc<crate::reply::jobs::JobSet>) -> Self {
        self.jobs = Some(jobs);
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
        self.reach_death(false)
    }

    /// D reached because the runtime dropped the actor before it got there
    /// (G1): `Killed` unless a non-`Normal` reason was already requested.
    pub(crate) fn die_dropped(&mut self) -> ExitReason {
        self.reach_death(true)
    }

    fn reach_death(&mut self, dropped: bool) -> ExitReason {
        if let Some(reason) = &self.reason {
            return reason.clone();
        }
        // Exit pending: dropped replies wait for D; jobs stop being admitted
        // and are cancelled (the job join comes after publication).
        if let Some(replies) = &self.replies {
            replies.exiting();
        }
        if let Some(jobs) = &self.jobs {
            jobs.cancel_all();
        }
        // Fence outgoing replies and exit signals before the final reason is
        // visible (outside any trace section: an in-flight signal may need
        // one). Later ones are refused; those in flight finish first, so a
        // signal sent before death precedes the death notices.
        if let Some(replies) = &self.replies {
            replies.fence();
        }
        let id = self.id.raw();
        let mut skipped = false;
        let reason = trace::life_section(id, || {
            let mut request = crate::link::lock_request(&self.cause);
            let reason = if dropped {
                skipped = !request.hook_begun();
                request.runtime_drop()
            } else {
                request.die()
            };
            drop(request);
            trace::life_emit(id, || {
                trace::LifeEvent::new(if dropped { "Drop" } else { "Die" }, id)
                    .x(trace::life_reason(&reason))
            });
            self.close(reason)
        });
        if skipped {
            log_stopped_skipped(self.id, "", "runtime_drop", Some(&reason));
        }
        fail_point!("actor.at_death");
        // Every open deferred reply (and every exit-pending one) is answered
        // with the final reason, before any destructor runs.
        if let Some(replies) = &self.replies {
            replies.close();
        }
        reason
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
        // Normally D was reached already; otherwise the runtime dropped the
        // actor (G1) and nothing of it is left to destroy.
        let reason = self.die_dropped();
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
        // The trace's `Publish` is this point: destruction is over and the
        // notices are delivered after it.
        let id = self.id.raw();
        trace::life_section(id, || {
            trace::life_emit(id, || trace::LifeEvent::new("Publish", id));
        });
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

/// Captured as one value by the dispatch closure, so a never-polled actor
/// dropped with its runtime still reaches D, then answers its queued
/// requests (`discharge`), then destroys its payload, then publishes,
/// without relying on the compiler's order for disjoint closure captures.
pub(crate) struct ActorStartup<P, C: Fn()> {
    payload: Option<P>,
    cleanup: Option<ActorCleanup<C>>,
    discharge: fn(&mut P),
}
impl<P, C: Fn()> ActorStartup<P, C> {
    pub(crate) fn new(payload: P, cleanup: ActorCleanup<C>, discharge: fn(&mut P)) -> Self {
        Self {
            payload: Some(payload),
            cleanup: Some(cleanup),
            discharge,
        }
    }
    pub(crate) fn into_parts(mut self) -> (P, ActorCleanup<C>) {
        (self.payload.take().unwrap(), self.cleanup.take().unwrap())
    }
}
impl<P, C: Fn()> Drop for ActorStartup<P, C> {
    fn drop(&mut self) {
        if let Some(cleanup) = &mut self.cleanup {
            cleanup.die_dropped();
        }
        if let Some(mut payload) = self.payload.take() {
            (self.discharge)(&mut payload);
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
