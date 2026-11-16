//! The supervisor core: restart budget, failure records, the shared `Core` state and its restart, shutdown and cleanup bookkeeping.
use super::*;

pub(super) struct RetryBudget {
    pub(super) attempts: VecDeque<Instant>,
    pub(super) reserved: usize,
    pub(super) max: usize,
    pub(super) window: Duration,
}
impl RetryBudget {
    pub(super) fn available(&self, now: Instant) -> bool {
        self.attempts
            .iter()
            .filter(|&&time| now.saturating_duration_since(time) < self.window)
            .count()
            .saturating_add(self.reserved)
            < self.max
    }
    pub(super) fn take(&mut self, now: Instant) -> bool {
        admit_in_window(
            &mut self.attempts,
            self.max.saturating_sub(self.reserved),
            self.window,
            now,
            |now, t| now.saturating_duration_since(t),
        )
    }
}

/// Rolling-window rate limit: forget stamps at least `window` old, then admit
/// `now` only while fewer than `max` remain. `since(now, t)` is the elapsed
/// time from `t` to `now`, generic so the window can be checked without clocks.
pub(crate) fn admit_in_window<T: Copy>(
    attempts: &mut VecDeque<T>,
    max: usize,
    window: Duration,
    now: T,
    since: impl Fn(T, T) -> Duration,
) -> bool {
    while attempts.front().is_some_and(|t| since(now, *t) >= window) {
        attempts.pop_front();
    }
    if attempts.len() >= max {
        return false;
    }
    attempts.push_back(now);
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FailureKind {
    Other,
    /// A child's first start failed (S1).
    FirstStart,
    Cleanup,
    Coordinator,
    RestartBudget,
}
#[derive(Debug, Clone)]
pub(super) struct FailureOrigin {
    pub(super) child: String,
    pub(super) generation: u64,
    pub(super) reason: Option<ExitReason>,
    pub(super) last_error: Option<String>,
}
#[derive(Debug, Clone)]
pub(super) struct Failure {
    pub(super) kind: FailureKind,
    pub(super) message: String,
    pub(super) origin: Option<FailureOrigin>,
    pub(super) logged: bool,
}
impl Failure {
    pub(super) fn new(kind: FailureKind, message: String, origin: Option<FailureOrigin>) -> Self {
        Self {
            kind,
            message,
            origin,
            logged: false,
        }
    }
}
impl std::ops::Deref for Failure {
    type Target = str;
    fn deref(&self) -> &str {
        &self.message
    }
}

pub(super) struct Core {
    pub(super) recovery: Mutex<RecoveryQueue>,
    pub(super) recovery_changed: watch::Sender<u64>,
    pub(super) gate: Arc<Admission>,
    pub(super) cancel: CancellationToken,
    pub(super) admission_closed: CancellationToken,
    pub(super) done: watch::Sender<bool>,
    pub(super) ready: watch::Sender<bool>,
    pub(super) failure: Mutex<Option<Failure>>,
    pub(super) failed: Arc<std::sync::atomic::AtomicBool>,
    pub(super) cleanup_failure: Mutex<Option<String>>,
    /// Every recorded cleanup failure, for the rollback list of a failed
    /// start (S2).
    pub(super) cleanup_failures: Mutex<Vec<String>>,
    pub(super) children: Mutex<Vec<Arc<dyn ChildControl>>>,
    pub(super) budget: Mutex<RetryBudget>,
    pub(super) config: SupervisorConfig,
    pub(super) trace: TraceId,
    #[cfg(test)]
    pub(super) initial_ready_pause: Mutex<Option<(CancellationToken, watch::Sender<bool>)>>,
    #[cfg(test)]
    pub(super) retry_admission_pause: Mutex<Option<(CancellationToken, watch::Sender<bool>)>>,
    #[cfg(feature = "lifecycle-trace")]
    pub(super) deadline_recorded: std::sync::atomic::AtomicBool,
}
impl Core {
    pub(super) fn close_admission(&self) {
        trace::atomic(|| {
            if self.gate.is_open() {
                self.cohort(|| Event::sup("CClose"));
                trace::tree(self.trace, |id| TreeEvent::new("TClose", id));
            }
            #[cfg(feature = "lifecycle-trace")]
            if self.gate.is_open() && !trace::close_suppressed() {
                trace::emit(self.trace, || Event::sup("Close"));
            }
            self.gate.close();
        });
        self.admission_closed.cancel();
        let children = system::lock(&self.children).clone();
        for child in children {
            child.close_admission();
        }
    }
    pub(super) fn close(&self) {
        trace::atomic(|| {
            self.close_admission();
            if !self.cancel.is_cancelled() {
                trace::tree(self.trace, |id| TreeEvent::new("TCancel", id));
                self.cancel.cancel();
            }
        });
    }
    pub(super) fn cancel_unready_startup(&self) {
        trace::atomic(|| {
            let cancel = self.gate.close_unless(
                || *self.ready.borrow(),
                || {
                    self.cohort(|| Event::sup("CClose"));
                    trace::tree(self.trace, |id| TreeEvent::new("TClose", id));
                    #[cfg(feature = "lifecycle-trace")]
                    if !trace::close_suppressed() {
                        trace::emit(self.trace, || Event::sup("Close"));
                    }
                },
            );
            if cancel {
                self.close();
            }
        });
    }
    pub(super) fn stop_remaining(&self) {
        trace::atomic(|| {
            self.close();
            #[cfg(feature = "lifecycle-trace")]
            if !*self.done.borrow()
                && !self
                    .deadline_recorded
                    .swap(true, std::sync::atomic::Ordering::Relaxed)
            {
                trace::emit(self.trace, || Event::sup("ShutdownDeadline"));
            }
        });
        let children = system::lock(&self.children).clone();
        for child in children.iter().rev() {
            child.stop();
        }
    }
    #[cfg(test)]
    pub(super) fn fail(&self, error: String) {
        self.fail_kind(FailureKind::Other, error);
    }
    pub(super) fn fail_kind(&self, kind: FailureKind, error: String) {
        self.fail_record(Failure::new(kind, error, None));
    }
    pub(super) fn fail_record(&self, record: Failure) {
        let mut failure = system::lock(&self.failure);
        if failure.is_none() {
            *failure = Some(record);
        }
        self.failed
            .store(true, std::sync::atomic::Ordering::Release);
        drop(failure);
        self.close();
    }
    #[inline(always)]
    pub(super) fn cohort(&self, make: impl FnOnce() -> Event) {
        #[cfg(feature = "lifecycle-trace")]
        if self.config.strategy != SupervisorStrategy::OneForOne {
            trace::emit(self.trace, make);
        }
        #[cfg(not(feature = "lifecycle-trace"))]
        let _ = make;
    }
    pub(super) fn record_cleanup_failure(&self, error: &str) {
        system::lock(&self.cleanup_failures).push(error.to_string());
        let mut failure = system::lock(&self.cleanup_failure);
        if failure.is_none() {
            *failure = Some(error.to_string());
        }
    }
    pub(super) fn fail_cleanup(&self, error: String) {
        self.record_cleanup_failure(&error);
        self.fail_kind(FailureKind::Cleanup, error);
    }
    pub(super) fn fail_cleanup_for(
        &self,
        snapshot: ChildSnapshot,
        error: String,
        event: impl FnOnce() -> Option<Event>,
    ) {
        self.record_cleanup_failure(&error);
        let record = snapshot_failure(snapshot, FailureKind::Cleanup, error);
        trace::failing(|| {
            trace::emit_some(self.trace, event);
            self.fail_record(record);
        });
    }
    pub(super) fn request_recovery(&self, index: usize, after: u64) {
        trace::atomic(|| {
            self.gate.if_open(|| {
                let mut queue = system::lock(&self.recovery);
                let accepted = queue.request(index, after);
                self.cohort(|| {
                    Event::new("CRequest", index + 1, trace::flag(accepted)).round(after)
                });
                if accepted {
                    self.recovery_changed
                        .send_modify(|n| *n = n.wrapping_add(1));
                }
            })
        });
    }
    /// `fail` as part of the model action `event` (whose `Fail` closes the
    /// gate), so the gate flip is not traced as a separate `Close`.
    pub(super) fn fail_as_kind(
        &self,
        kind: FailureKind,
        error: String,
        event: impl FnOnce() -> Option<Event>,
    ) {
        trace::failing(|| {
            trace::emit_some(self.trace, event);
            self.fail_kind(kind, error);
        });
    }
    pub(super) fn log_failure_once(&self) {
        let failure = {
            let mut slot = system::lock(&self.failure);
            let Some(failure) = slot.as_mut() else {
                return;
            };
            if failure.logged {
                return;
            }
            failure.logged = true;
            failure.clone()
        };
        let (child, generation, reason, last_error) = failure
            .origin
            .as_ref()
            .map(|origin| {
                (
                    origin.child.as_str(),
                    origin.generation,
                    origin.reason.as_ref(),
                    origin.last_error.as_deref(),
                )
            })
            .unwrap_or(("<supervisor>", 0, None, None));
        let event = if failure.kind == FailureKind::RestartBudget {
            "restart_budget_exhausted"
        } else {
            "supervision_give_up"
        };
        if let Err(payload) = std::panic::catch_unwind(AssertUnwindSafe(|| {
            tracing::warn!(event, child, generation, ?reason, ?last_error,
                kind = ?failure.kind, error = %failure.message, "supervisor failed");
        })) {
            crate::mailbox::dispose_panic(payload);
        }
    }
    /// A concurrent ordinary stop wins over a later child failure. The gate
    /// lock, rather than optional trace instrumentation, commits this decision.
    #[cfg(test)]
    pub(super) fn fail_if_open_as(
        &self,
        error: String,
        event: impl FnOnce() -> Option<Event>,
    ) -> bool {
        self.fail_record_if_open_as(Failure::new(FailureKind::Other, error, None), event)
    }
    pub(super) fn fail_record_if_open_as(
        &self,
        record: Failure,
        event: impl FnOnce() -> Option<Event>,
    ) -> bool {
        trace::failing(|| {
            let committed = self.gate.close_if_open(|| {
                trace::emit_some(self.trace, event);
                self.cohort(|| Event::sup("CClose"));
                trace::tree(self.trace, |id| TreeEvent::new("TClose", id));
                let mut failure = system::lock(&self.failure);
                if failure.is_none() {
                    *failure = Some(record);
                }
                self.failed
                    .store(true, std::sync::atomic::Ordering::Release);
            });
            if committed {
                self.close();
            }
            committed
        })
    }
    pub(super) fn reserve_retry(&self, child: usize) -> Option<RetryReservation<'_>> {
        trace::atomic(|| {
            self.gate
                .if_open(|| {
                    let mut budget = system::lock(&self.budget);
                    let now = Instant::now();
                    // Pending backoffs reserve capacity, so siblings cannot all sleep
                    // against the same final permit. Only actual grants enter the window.
                    while budget
                        .attempts
                        .front()
                        .is_some_and(|time| now.saturating_duration_since(*time) >= budget.window)
                    {
                        budget.attempts.pop_front();
                    }
                    if !budget.available(now) {
                        return None;
                    }
                    budget.reserved += 1;
                    trace::emit(self.trace, || {
                        Event::new("RetryReserve", child, "")
                            .count(budget.attempts.len() + budget.reserved)
                    });
                    Some(RetryReservation {
                        core: self,
                        child,
                        held: true,
                    })
                })
                .flatten()
        })
    }
    /// One OneForAll/RestForOne recovery round is one restart action (OTP's
    /// `add_restart` runs once per restart, before the cohort is rebuilt):
    /// take its grant now. The round's controllers then rebuild without a
    /// grant of their own. Pending per-child reservations (retries during
    /// the initial start) keep their capacity. Returns whether it was taken.
    pub(super) fn charge_round(&self) -> bool {
        let mut budget = system::lock(&self.budget);
        budget.take(Instant::now())
    }
    /// Caller holds the admission gate and slot lock; charge atomically with
    /// generation admission, so placement failure or supersession refunds it.
    pub(super) fn retry_under_gate(&self, reservation: &mut RetryReservation<'_>) -> bool {
        let mut budget = system::lock(&self.budget);
        budget.reserved -= 1;
        reservation.held = false;
        let taken = budget.take(Instant::now());
        debug_assert!(taken, "reserved retry lost its capacity");
        if taken {
            trace::emit(self.trace, || {
                Event::new("BackoffDone", reservation.child, "retry")
                    .count(budget.attempts.len() + budget.reserved)
            });
        }
        taken
    }
    #[cfg(test)]
    pub(super) fn retry(&self, reservation: &mut RetryReservation<'_>) -> bool {
        self.gate
            .if_open(|| self.retry_under_gate(reservation))
            .unwrap_or(false)
    }
}
pub(super) struct RetryReservation<'a> {
    pub(super) core: &'a Core,
    pub(super) child: usize,
    pub(super) held: bool,
}
impl Drop for RetryReservation<'_> {
    fn drop(&mut self) {
        if self.held {
            trace::atomic(|| {
                let mut budget = system::lock(&self.core.budget);
                budget.reserved -= 1;
                trace::emit(self.core.trace, || {
                    Event::new("RetryRelease", self.child, "")
                        .count(budget.attempts.len() + budget.reserved)
                });
            });
        }
    }
}
impl ShutdownUnit for Core {
    fn close_admission(&self) {
        Core::close_admission(self);
    }
    fn stop(&self) {
        Core::close(self);
    }
    fn stop_remaining(&self) {
        Core::stop_remaining(self);
    }
    fn joined(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let _ = system::wait_true(self.done.subscribe()).await;
        })
    }
    fn is_done(&self) -> bool {
        *self.done.borrow()
    }
    fn failure(&self) -> Option<String> {
        system::lock(&self.failure)
            .as_ref()
            .map(|f| f.message.clone())
    }
}

pub(super) trait Runner: Send {
    fn run(self: Box<Self>, core: Arc<Core>) -> BoxFuture<'static, ()>;
}
pub(super) struct TypedRunner<A: Actor> {
    pub(super) spec: ChildSpec<A>,
    pub(super) view: Arc<View<A>>,
}
impl<A: Actor> Runner for TypedRunner<A> {
    fn run(self: Box<Self>, core: Arc<Core>) -> BoxFuture<'static, ()> {
        let view = self.view.clone();
        Box::pin(async move {
            if let Err(payload) = AssertUnwindSafe(run_child(self.spec, self.view, core.clone()))
                .catch_unwind()
                .await
            {
                let message = format!(
                    "child controller panicked: {}",
                    crate::error::panic_message(&*payload)
                );
                crate::mailbox::dispose_panic(payload);
                core.fail_cleanup_for(view.snapshot(), message, || {
                    Some(Event::new(
                        "Unmodelled",
                        view.trace_index,
                        "ControllerPanicked",
                    ))
                });
                let scope = system::lock(&view.data).scope.clone();
                if let Some(scope) = scope {
                    scope.stop(ExitReason::Shutdown);
                    scope.joined().await;
                }
                view.finish(true);
            }
        })
    }
}
