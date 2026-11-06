//! Shared terminal publication for actor futures and native threads.
use crate::{
    child_handle::ActorId,
    error::ExitReason,
    link::{LinkCleanup, LinkTable, LinkedExitReason},
    monitor::{MonitorCleanup, MonitorTable},
};
use spawned_rt::tasks::watch;

/// Constructed before dispatch, so even a never-polled actor has cleanup.
/// Completion owns its sender until links and outgoing monitors are closed.
pub(crate) struct ActorCleanup<C: Fn()> {
    metrics: crate::instrumentation::ActorObservation,
    links: LinkCleanup,
    cause: LinkedExitReason,
    monitors: Option<MonitorCleanup>,
    completion: watch::Sender<Option<ExitReason>>,
    fallback: ExitReason,
    reason: Option<ExitReason>,
    cancel: C,
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
            metrics: crate::instrumentation::ActorObservation::new(),
            links: LinkCleanup::new(id, links),
            cause,
            monitors: Some(MonitorCleanup(monitors)),
            completion,
            fallback,
            reason: None,
            cancel,
        }
    }
    pub(crate) fn complete(&mut self, mut reason: ExitReason) {
        let mut cause = self.cause.lock().unwrap_or_else(|p| p.into_inner());
        if matches!(reason, ExitReason::Normal) {
            if let Some(linked) = cause.as_ref() {
                reason = linked.clone();
            }
        }
        // Freeze the completed outcome before any further cancellation callbacks.
        *cause = Some(reason.clone());
        self.reason = Some(reason);
    }
}
impl<C: Fn()> Drop for ActorCleanup<C> {
    fn drop(&mut self) {
        let reason = self.reason.take().unwrap_or_else(|| self.fallback.clone());
        *self.cause.lock().unwrap_or_else(|p| p.into_inner()) = Some(reason.clone());
        (self.cancel)();
        drop(self.monitors.take());
        self.links.complete(&reason);
        self.metrics.finish(&reason);
        self.completion.send_replace(Some(reason));
    }
}

/// Captured as one value by the dispatch closure. Declaration order makes
/// never-polled payload destruction precede completion publication, without
/// relying on the compiler's order for disjoint closure captures.
pub(crate) struct ActorStartup<P, C: Fn()> {
    payload: P,
    cleanup: ActorCleanup<C>,
}
impl<P, C: Fn()> ActorStartup<P, C> {
    pub(crate) fn new(payload: P, cleanup: ActorCleanup<C>) -> Self {
        Self { payload, cleanup }
    }
    pub(crate) fn into_parts(self) -> (P, ActorCleanup<C>) {
        (self.payload, self.cleanup)
    }
}
