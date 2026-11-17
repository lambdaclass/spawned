//! Unified start (0.9, design `docs/design/unified-start.md`): the start
//! errors and outcomes and the per-instance startup cell, shared by both
//! execution modes.
//!
//! Every instance has one [`StartupCell`] (in its `SignalTarget`): an
//! immutable commitment (ready, failed, ignored or timed out) written once,
//! by the child at the end of `started()` or by the init timer. A waiting
//! start (`start`, `start_link`, `start_monitor`) adds a waiter to it. The
//! child's start edge (the `start_link`/`start_monitor` parent) carries its
//! notice through the cell: at publication the cell disposes of it by the
//! commitment and the waiter state (design 5.2): delivered, diverted to the
//! waiter (consumed by its error, handed on if the wait is abandoned),
//! applied, or dropped.
//!
//! Lock order (design section 3; this code's ranks): an admission gate and
//! group lock, then link tables, then an actor's exit request, then the
//! registry; the cell lock is always last. Nothing under the cell lock takes
//! another framework lock; deliveries decided under it run after it is
//! released.
use crate::child_handle::ChildHandle;
use crate::error::ExitReason;
use crate::lifecycle_trace as trace;
use crate::link::LinkNotice;
use spawned_rt::time::Instant;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// The result of a start that did not fail: OTP's `{ok, Pid}` or `ignore`.
///
/// `pending` is `Some` when the ignoring instance was not yet published at
/// its cleanup deadline (a hung destructor): it still holds its name and
/// slot until it is (`wait_exit` on the handle reaches its real end).
#[must_use]
#[derive(Debug)]
pub enum StartOutcome<T> {
    /// `started()` returned ready (or a continuation).
    Started(T),
    /// `started()` returned [`Init::ignore`](crate::tasks::Init::ignore): the
    /// actor ended `Normal` without becoming ready.
    Ignored { pending: Option<ChildHandle> },
}

impl<T> StartOutcome<T> {
    /// The started value, or `None` for an ignored start.
    pub fn started(self) -> Option<T> {
        match self {
            Self::Started(value) => Some(value),
            Self::Ignored { .. } => None,
        }
    }
    /// Whether `started()` returned `Init::ignore()`.
    pub fn is_ignored(&self) -> bool {
        matches!(self, Self::Ignored { .. })
    }
    /// Map the started value.
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> StartOutcome<U> {
        match self {
            Self::Started(value) => StartOutcome::Started(f(value)),
            Self::Ignored { pending } => StartOutcome::Ignored { pending },
        }
    }
}

/// Why a start failed.
///
/// Stage 1 (nothing was created): `Closed`, `Capacity`, `AlreadyStarted`,
/// `ParentExited`, `InvalidOptions`, `System`. Stage 2 (the child existed
/// and is gone, or still cleaning up when `pending` is `Some`): `InitFailed`,
/// `InitExited`, `InitTimeout`. A stage-2 error with `pending: None` is
/// returned only after the child's publication: its state is dropped, its
/// notices are sent and its name is free.
#[derive(Debug, Clone, thiserror::Error)]
#[non_exhaustive]
pub enum StartError {
    /// The group, the system or the parent's scope no longer admits actors.
    #[error("the group, system or parent scope no longer admits actors")]
    Closed,
    /// A thread group has no free slot.
    #[error("the execution group has no free actor slot")]
    Capacity,
    /// The start's name is registered. `existing` is the holder's handle, or
    /// `None` for a value that is not an actor (OTP `{already_started, Pid}`).
    #[error("name '{name}' is already registered")]
    AlreadyStarted {
        name: String,
        existing: Option<ChildHandle>,
    },
    /// `start_link`/`start_monitor` through a `Context` whose actor's exit
    /// has begun.
    #[error("the parent's exit has begun")]
    ParentExited,
    /// The options are invalid (validated before anything is created).
    #[error("invalid start options: {0}")]
    InvalidOptions(&'static str),
    /// Executor or runtime construction failed.
    #[cfg(feature = "async")]
    #[error(transparent)]
    System(crate::system::SystemError),
    /// `started()` returned an error (`Err(e)` or `Init::error(e)`); the
    /// actor ended `Normal` (OTP 26 `init_fail/3`).
    #[error("started() failed: {cause}")]
    InitFailed {
        cause: String,
        pending: Option<ChildHandle>,
    },
    /// The actor ended during `started()`: a kill, an administrative stop, an
    /// untrapped exit signal, a panic, `ctx.stop*` or `Init::stop`.
    /// `reason` is the final exit reason (the one `wait_exit` reports).
    #[error("the actor exited during started(): {reason:?}")]
    InitExited {
        reason: ExitReason,
        pending: Option<ChildHandle>,
    },
    /// `started()` did not return within the start's `init_timeout`.
    #[error("started() did not return within the init timeout")]
    InitTimeout { pending: Option<ChildHandle> },
}

#[cfg(feature = "async")]
impl From<crate::system::SystemError> for StartError {
    fn from(error: crate::system::SystemError) -> Self {
        use crate::system::SystemError;
        match error {
            SystemError::Closed => Self::Closed,
            SystemError::Capacity => Self::Capacity,
            other => Self::System(other),
        }
    }
}

/// A bound that may be infinite (`init_timeout`, `startup_timeout`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Timeout {
    After(Duration),
    #[default]
    Infinity,
}

impl From<Duration> for Timeout {
    fn from(duration: Duration) -> Self {
        Self::After(duration)
    }
}

// ---------------------------------------------------------------------------
// The startup decision (design 2.3)
// ---------------------------------------------------------------------------

/// What `started()` returned, mode-independent (the continuation lives in
/// the mode's `Init`).
#[derive(Debug)]
pub(crate) enum Returned {
    /// `started()` was not run: a kill or the init timer came first
    /// (`KilledEarly`), or a managed child's admission closed first.
    NotBegun,
    Panic(String),
    Stop(ExitReason),
    Error(String),
    Ignore,
    Ready,
}

impl Returned {
    #[allow(dead_code)] // trace only
    pub(crate) fn trace_name(&self, continued: bool) -> &'static str {
        match self {
            Self::NotBegun => "none",
            Self::Panic(_) => "panic",
            Self::Stop(ExitReason::Normal) => "stopN",
            Self::Stop(_) => "stopS",
            Self::Error(_) => "error",
            Self::Ignore => "ignore",
            Self::Ready if continued => "continue",
            Self::Ready => "ready",
        }
    }
}

/// The row of the design's 2.3 table that decided a start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Row {
    /// 1: the init timer committed `TimedOut`.
    Timer,
    /// 2: a kill arrived.
    Kill,
    /// 3: an administrative stop or a held untrapped non-`Normal` signal.
    Signal,
    /// 4: `started()` panicked.
    Panic,
    /// 5, 6: `ctx.stop*` in `started()`, or `Init::stop`.
    Stop,
    /// 7: `Init::error`.
    Error,
    /// 8: `Init::ignore`.
    Ignore,
    /// 9: ready, but the child's admission closed.
    Closed,
    /// 10: ready.
    Ready,
}

impl Row {
    pub(crate) fn commit(&self) -> Commit {
        match self {
            Self::Timer => Commit::TimedOut,
            Self::Ignore => Commit::Ignored,
            Self::Ready => Commit::Ready,
            _ => Commit::Failed,
        }
    }
    #[allow(dead_code)] // trace only
    pub(crate) fn trace_name(&self) -> &'static str {
        match self {
            Self::Timer => "timer",
            Self::Kill => "kill",
            Self::Signal => "signal",
            Self::Panic => "panic",
            Self::Stop => "stop",
            Self::Error => "error",
            Self::Ignore => "ignore",
            Self::Closed => "closed",
            Self::Ready => "ready",
        }
    }
}

// ---------------------------------------------------------------------------
// The startup cell (design 5.1)
// ---------------------------------------------------------------------------

/// The immutable startup commitment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Commit {
    Pending = 0,
    Ready = 1,
    Failed = 2,
    Ignored = 3,
    TimedOut = 4,
}

impl Commit {
    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Ready,
            2 => Self::Failed,
            3 => Self::Ignored,
            4 => Self::TimedOut,
            _ => Self::Pending,
        }
    }
    #[allow(dead_code)] // trace only
    pub(crate) fn trace_name(self) -> &'static str {
        match self {
            Self::Pending => "none",
            Self::Ready => "ready",
            Self::Failed => "failed",
            Self::Ignored => "ignored",
            Self::TimedOut => "timedout",
        }
    }
}

/// The waiter of a waiting start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Waiter {
    /// `spawn` (or a refused start): nobody waits.
    None,
    Waiting,
    /// The waiter took its answer.
    Consumed,
    /// The `Starting` was dropped unanswered.
    #[cfg_attr(not(feature = "async"), allow(dead_code))] // tasks mode only
    Abandoned,
}

/// The kind of a start edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EdgeKind {
    Link,
    Monitor,
}

/// What publication decided for the start edge's notice (design 5.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Disposition {
    /// Ordinary: delivered (applied to a non-trapping parent by the usual
    /// signal rules).
    Deliver,
    /// Held for the waiter: its error consumes it, abandonment hands it on.
    Divert,
    /// Suppressed by a failure the waiter already consumed.
    Drop,
}

impl Disposition {
    #[allow(dead_code)] // trace only
    fn trace_name(self) -> &'static str {
        match self {
            Self::Deliver => "deliver",
            Self::Divert => "divert",
            Self::Drop => "drop",
        }
    }
}

/// A diverted `Down`: the parent's fenced delivery (`true`) or the removal
/// of its monitor entry (`false`). Built by the parent's monitor waiter.
pub(crate) type DownNotice = Box<dyn FnOnce(bool) + Send>;

/// The start edge (design 5.2), set at the commit for `start_link` and
/// `start_monitor`.
pub(crate) struct StartEdge {
    pub(crate) kind: EdgeKind,
    /// The parent, for its trap flag (read at publication).
    pub(crate) parent: Arc<crate::link::SignalTarget>,
}

struct CellInner {
    waiter: Waiter,
    edge: Option<StartEdge>,
    /// `InitFailed`'s diagnostic.
    cause: Option<String>,
    /// The decided (final) reason of a non-ready commitment.
    requested: Option<ExitReason>,
    /// When the commitment was made: anchors the cleanup deadline.
    committed_at: Option<Instant>,
    /// The cleanup interval of the instance's placement.
    cleanup: Duration,
    /// The final reason, once published.
    published: Option<ExitReason>,
    /// Publication has begun (its dispositions are decided): observers wait
    /// for it rather than answer `pending`.
    publishing: bool,
    /// The disposition of the edge's notice at publication.
    disposition: Option<Disposition>,
    /// Diverted notices, waiting for the waiter's answer or abandonment.
    diverted_link: Option<LinkNotice>,
    diverted_down: Option<DownNotice>,
    /// The monitor waiter has not yet run (its `Down` is not yet built).
    down_pending: bool,
    /// The init timer is armed and has not fired.
    timer: Option<Arc<TimerCancel>>,
    /// Trace id of the instance (0 when not recorded).
    trace: u64,
    /// Bumped at every change (blocking observers wait on it).
    version: u64,
    /// The cleanup deadline was observed expired (recorded once).
    deadline_seen: bool,
}

/// Cancels a pending init timer at the commitment.
#[derive(Default)]
pub(crate) struct TimerCancel {
    cancelled: Mutex<bool>,
    wake: Condvar,
}

impl TimerCancel {
    fn cancel(&self) {
        *self.cancelled.lock().unwrap_or_else(|p| p.into_inner()) = true;
        self.wake.notify_all();
    }
}

/// One instance's startup result and waiter (design 5.1).
pub(crate) struct StartupCell {
    state: AtomicU8,
    inner: Mutex<CellInner>,
    /// Blocking observers.
    changed: Condvar,
    /// Async observers: bumped at every change.
    version: spawned_rt::sync::watch::Sender<u64>,
}

impl std::fmt::Debug for StartupCell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StartupCell")
            .field("commit", &self.commit())
            .finish_non_exhaustive()
    }
}

/// An observation of the cell by `ready()` or the waiter.
pub(crate) enum Observed {
    /// Nothing to answer yet; wait for a change or the deadline (if any).
    Wait(Option<Instant>),
    Answer(Result<StartOutcome<()>, StartError>),
}

impl StartupCell {
    pub(crate) fn new() -> Arc<Self> {
        let (version, _) = spawned_rt::sync::watch::channel(0);
        Arc::new(Self {
            state: AtomicU8::new(Commit::Pending as u8),
            inner: Mutex::new(CellInner {
                waiter: Waiter::None,
                edge: None,
                cause: None,
                requested: None,
                committed_at: None,
                cleanup: Duration::from_secs(5),
                published: None,
                publishing: false,
                disposition: None,
                diverted_link: None,
                diverted_down: None,
                down_pending: false,
                timer: None,
                trace: 0,
                version: 0,
                deadline_seen: false,
            }),
            changed: Condvar::new(),
            version,
        })
    }

    fn lock(&self) -> MutexGuard<'_, CellInner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn notify(&self) {
        {
            let mut inner = self.lock();
            inner.version = inner.version.wrapping_add(1);
        }
        self.changed.notify_all();
        let next = self.version.borrow().wrapping_add(1);
        self.version.send_replace(next);
    }

    pub(crate) fn commit(&self) -> Commit {
        Commit::from_u8(self.state.load(Ordering::Acquire))
    }

    /// Prepare (stage 1, private): the waiter, the edge, the cleanup
    /// interval and the trace id.
    pub(crate) fn prepare(
        &self,
        waiting: bool,
        edge: Option<StartEdge>,
        cleanup: Duration,
        trace: u64,
    ) {
        let mut inner = self.lock();
        inner.waiter = if waiting {
            Waiter::Waiting
        } else {
            Waiter::None
        };
        inner.down_pending = edge.as_ref().is_some_and(|e| e.kind == EdgeKind::Monitor);
        inner.edge = edge;
        inner.cleanup = cleanup;
        inner.trace = trace;
    }

    /// The cleanup interval may be set after the commit (a managed scope's).
    #[cfg_attr(not(feature = "async"), allow(dead_code))]
    pub(crate) fn set_cleanup(&self, cleanup: Duration) {
        self.lock().cleanup = cleanup;
    }

    #[allow(dead_code)] // trace only
    pub(crate) fn trace_id(&self) -> u64 {
        self.lock().trace
    }

    /// Commit the startup decision (the child, under its exit-request lock
    /// and, when managed, its admission gate; design 5.1). `decide` runs
    /// under the cell lock with whether the init timer already committed
    /// `TimedOut` (row 1), and returns the row, the requested reason and an
    /// `InitFailed` cause. Exactly one commitment wins `Pending`.
    pub(crate) fn decide(
        &self,
        returned: &Returned,
        continued: bool,
        trapping: bool,
        decide: impl FnOnce(bool) -> (Row, Option<ExitReason>, Option<String>),
    ) -> Row {
        let mut inner = self.lock();
        let current = self.commit();
        let (row, reason, cause) = decide(current == Commit::TimedOut);
        if current == Commit::Pending {
            self.state.store(row.commit() as u8, Ordering::Release);
            inner.committed_at = Some(Instant::now());
            inner.requested = reason.clone();
            inner.cause = cause;
            if let Some(timer) = inner.timer.take() {
                timer.cancel();
            }
        }
        let id = inner.trace;
        trace::start_emit(id, || {
            trace::StartEvent::new("SResolve", id)
                .x(returned.trace_name(continued))
                .y(row.trace_name())
                .z(reason.as_ref().map_or("", trace::life_reason))
                .f(trapping)
        });
        drop(inner);
        self.notify();
        row
    }

    /// Arm the init timer (stage 1, after the commit).
    pub(crate) fn arm_timer(&self) -> Option<Arc<TimerCancel>> {
        let mut inner = self.lock();
        if self.commit() != Commit::Pending {
            return None;
        }
        let timer = Arc::new(TimerCancel::default());
        inner.timer = Some(timer.clone());
        Some(timer)
    }

    /// The init timer fired: commit `TimedOut` if nothing is committed
    /// (design 6). Returns whether it won.
    pub(crate) fn time_out(&self) -> bool {
        let mut inner = self.lock();
        inner.timer = None;
        let won = self.commit() == Commit::Pending;
        if won {
            self.state.store(Commit::TimedOut as u8, Ordering::Release);
            inner.committed_at = Some(Instant::now());
            inner.requested = Some(ExitReason::Killed);
        }
        trace::start_emit(inner.trace, || {
            trace::StartEvent::new("STimer", inner.trace).x(if won { "won" } else { "lost" })
        });
        drop(inner);
        if won {
            self.notify();
        }
        won
    }

    /// Publication (design 5.2 step 5), from the child's cleanup: dispose of
    /// the start edge's link notice (taken at D) and decide the `Down`'s
    /// disposition. Returns the link notice to deliver now, if any; the
    /// caller delivers it and the other notices after the lock is released.
    pub(crate) fn publish(
        &self,
        reason: &ExitReason,
        link: Option<LinkNotice>,
    ) -> Option<LinkNotice> {
        let mut inner = self.lock();
        inner.publishing = true;
        let commit = self.commit();
        let mut deliver = link;
        let mut discard = None;
        let mut names = ("none", "");
        if let Some((kind, trapping)) = inner
            .edge
            .as_ref()
            .map(|edge| (edge.kind, edge.parent.trap.load(Ordering::Acquire)))
        {
            let disposition = dispose(commit, inner.waiter, kind, trapping, reason);
            inner.disposition = Some(disposition);
            if kind == EdgeKind::Link {
                match disposition {
                    Disposition::Divert => inner.diverted_link = deliver.take(),
                    Disposition::Drop => discard = deliver.take(),
                    Disposition::Deliver => {}
                }
            }
            names = (
                disposition.trace_name(),
                if kind == EdgeKind::Link {
                    "link"
                } else {
                    "monitor"
                },
            );
        }
        let id = inner.trace;
        trace::start_emit(id, || {
            trace::StartEvent::new("SPublish", id)
                .x(names.0)
                .y(names.1)
                .z(trace::life_reason(reason))
        });
        drop(inner);
        if let Some(notice) = discard {
            notice.discard();
        }
        deliver
    }

    /// `wait_exit` is published: final answers become available.
    pub(crate) fn published(&self, reason: &ExitReason) {
        self.lock().published = Some(reason.clone());
        self.notify();
    }

    /// The parent's monitor waiter, after the child's publication: whether to
    /// deliver the `Down` now; otherwise `down` is parked for the waiter or
    /// discarded.
    pub(crate) fn dispose_down(&self, down: DownNotice) -> Option<DownNotice> {
        let mut inner = self.lock();
        inner.down_pending = false;
        match inner.disposition {
            Some(Disposition::Divert) => match inner.waiter {
                Waiter::Waiting => {
                    inner.diverted_down = Some(down);
                    None
                }
                // The waiter answered before the waiter task ran: consumed.
                Waiter::Consumed => {
                    drop(inner);
                    down(false);
                    None
                }
                // Abandoned since: handed on.
                Waiter::Abandoned | Waiter::None => Some(down),
            },
            Some(Disposition::Drop) => {
                drop(inner);
                down(false);
                None
            }
            _ => Some(down),
        }
    }

    /// Observe the result (design 5.5). `waiter` consumes the answer and the
    /// diverted notices; `handle` builds the instance's handle for `pending`.
    pub(crate) fn observe(&self, waiter: bool, handle: impl FnOnce() -> ChildHandle) -> Observed {
        let mut inner = self.lock();
        let commit = self.commit();
        let id = inner.trace;
        let answer = match commit {
            Commit::Pending => return Observed::Wait(None),
            Commit::Ready => Ok(StartOutcome::Started(())),
            _ => {
                let (pending, reason) = match &inner.published {
                    Some(final_reason) => (None, final_reason.clone()),
                    None if inner.publishing => return Observed::Wait(None),
                    None => {
                        let deadline = inner
                            .committed_at
                            .map(|at| at + inner.cleanup)
                            .expect("a commitment has its instant");
                        if Instant::now() < deadline {
                            return Observed::Wait(Some(deadline));
                        }
                        if !std::mem::replace(&mut inner.deadline_seen, true) {
                            trace::start_emit(id, || trace::StartEvent::new("SDeadline", id));
                        }
                        (
                            Some(handle()),
                            inner.requested.clone().unwrap_or(ExitReason::Killed),
                        )
                    }
                };
                match commit {
                    Commit::Ignored => Ok(StartOutcome::Ignored { pending }),
                    Commit::TimedOut => Err(StartError::InitTimeout { pending }),
                    _ => Err(match &inner.cause {
                        Some(cause) => StartError::InitFailed {
                            cause: cause.clone(),
                            pending,
                        },
                        None => StartError::InitExited { reason, pending },
                    }),
                }
            }
        };
        if !waiter {
            trace::start_emit(id, || {
                trace::StartEvent::new("SReady", id).x(answer_name(&answer))
            });
            return Observed::Answer(answer);
        }
        if inner.waiter != Waiter::Waiting {
            return Observed::Answer(answer);
        }
        inner.waiter = Waiter::Consumed;
        let link = inner.diverted_link.take();
        let down = inner.diverted_down.take();
        trace::start_emit(id, || {
            let (reason, pending) = match &answer {
                Err(StartError::InitExited { reason, pending }) => {
                    (trace::life_reason(reason), pending.is_some())
                }
                Err(StartError::InitFailed { pending, .. })
                | Err(StartError::InitTimeout { pending })
                | Ok(StartOutcome::Ignored { pending }) => ("Normal", pending.is_some()),
                _ => ("", false),
            };
            trace::StartEvent::new("SAnswer", id)
                .x(answer_name(&answer))
                .z(reason)
                .f(pending)
        });
        drop(inner);
        // Consumed: the parent's records of the diverted notices go.
        if let Some(link) = link {
            link.discard();
        }
        if let Some(down) = down {
            down(false);
        }
        Observed::Answer(answer)
    }

    /// The waiter was dropped unanswered (design 5.4): a diverted notice is
    /// handed on as an ordinary one, exactly once, after the lock is released.
    #[cfg_attr(not(feature = "async"), allow(dead_code))]
    pub(crate) fn abandon(&self) {
        let mut inner = self.lock();
        if inner.waiter != Waiter::Waiting {
            return;
        }
        inner.waiter = Waiter::Abandoned;
        let link = inner.diverted_link.take();
        let down = inner.diverted_down.take();
        let id = inner.trace;
        // A Down not yet built by the monitor waiter is handed on by it.
        let handed = link.is_some()
            || down.is_some()
            || (inner.down_pending && inner.disposition == Some(Disposition::Divert));
        trace::start_emit(id, || trace::StartEvent::new("SAbandon", id).f(handed));
        drop(inner);
        if let Some(link) = link {
            link.deliver();
        }
        if let Some(down) = down {
            down(true);
        }
    }

    /// The change counter, read before an observation.
    pub(crate) fn version(&self) -> u64 {
        self.lock().version
    }

    /// Wait (blocking) until the cell changes after `seen`, or `deadline`.
    pub(crate) fn wait_blocking(&self, seen: u64, deadline: Option<Instant>) {
        let mut inner = self.lock();
        while inner.version == seen {
            match deadline {
                None => {
                    inner = self.changed.wait(inner).unwrap_or_else(|p| p.into_inner());
                }
                Some(deadline) => {
                    let now = Instant::now();
                    if deadline <= now {
                        return;
                    }
                    inner = self
                        .changed
                        .wait_timeout(inner, deadline - now)
                        .unwrap_or_else(|p| p.into_inner())
                        .0;
                }
            }
        }
    }

    /// A receiver bumped at every change (async observers).
    #[cfg_attr(not(feature = "async"), allow(dead_code))]
    pub(crate) fn subscribe(&self) -> spawned_rt::sync::watch::Receiver<u64> {
        self.version.subscribe()
    }
}

#[allow(dead_code)] // trace only
fn answer_name(answer: &Result<StartOutcome<()>, StartError>) -> &'static str {
    match answer {
        Ok(StartOutcome::Started(())) => "ok",
        Ok(StartOutcome::Ignored { .. }) => "ignored",
        Err(StartError::InitTimeout { .. }) => "timeout",
        Err(_) => "failed",
    }
}

/// The disposition table of design 5.2.
fn dispose(
    commit: Commit,
    waiter: Waiter,
    kind: EdgeKind,
    trapping: bool,
    reason: &ExitReason,
) -> Disposition {
    if commit == Commit::Ready || matches!(waiter, Waiter::None | Waiter::Abandoned) {
        return Disposition::Deliver;
    }
    match waiter {
        Waiter::Waiting => match kind {
            EdgeKind::Monitor => Disposition::Divert,
            EdgeKind::Link if trapping || commit == Commit::TimedOut => Disposition::Divert,
            // A non-trapping parent: applied if non-Normal (OTP: the link
            // kills the caller); a Normal one is ignored, so it is dropped
            // here (the model's legitimate drop) rather than delivered.
            EdgeKind::Link if *reason == ExitReason::Normal => Disposition::Drop,
            EdgeKind::Link => Disposition::Deliver,
        },
        // Answered at the cleanup deadline before publication.
        _ => {
            if kind == EdgeKind::Link
                && !trapping
                && commit == Commit::Failed
                && *reason != ExitReason::Normal
            {
                Disposition::Deliver
            } else {
                Disposition::Drop
            }
        }
    }
}

/// The init timer (design 6): a framework-owned thread that commits
/// `TimedOut` at expiry unless the child committed first, then kills it.
/// Only a start with `init_timeout` pays for it.
pub(crate) fn spawn_init_timer(cell: &Arc<StartupCell>, handle: ChildHandle, after: Duration) {
    let Some(cancel) = cell.arm_timer() else {
        return;
    };
    let cell = Arc::downgrade(cell);
    let run = move || {
        let deadline = std::time::Instant::now() + after;
        let mut cancelled = cancel.cancelled.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            if *cancelled {
                return;
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                break;
            }
            cancelled = cancel
                .wake
                .wait_timeout(cancelled, deadline - now)
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
        drop(cancelled);
        let Some(cell) = cell.upgrade() else {
            return;
        };
        if cell.time_out() {
            // The kill is applied when started() returns (row 1); the
            // timer's commitment already decided the start.
            crate::link::lock_request(&handle.signal_target().request).kill();
            (handle.signal_target().wake)();
        }
    };
    if std::thread::Builder::new()
        .name("spawned-init-timer".into())
        .spawn(run)
        .is_err()
    {
        tracing::warn!("init timer thread could not be spawned; the start has no init timeout");
    }
}
