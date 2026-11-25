use crate::child_handle::ActorId;
use crate::error::{ActorError, ExitReason};
use crate::lifecycle_trace as trace;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// Exit message
// ---------------------------------------------------------------------------

/// Notification delivered to an actor (via `exit_received`) when a linked actor
/// stops, if the receiver has called [`Context::trap_exit(true)`].
///
/// Without trapping, a linked actor's death cancels the receiver's actor
/// instead of delivering this message.
///
/// [`Context::trap_exit(true)`]: crate::threads::Context::trap_exit
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Exit {
    /// The actor that died.
    pub from: ActorId,
    /// Why it stopped.
    pub reason: ExitReason,
}

impl Exit {
    /// An exit notice from `from` (for test doubles and comparisons; the
    /// framework builds the ones it delivers).
    pub fn new(from: ActorId, reason: ExitReason) -> Self {
        Self { from, reason }
    }
}

// ---------------------------------------------------------------------------
// Internal types
// ---------------------------------------------------------------------------

/// Type-erased function that delivers an `Exit` message to a linked actor's
/// mailbox. Captures the peer's typed sender at link time.
pub(crate) type SendExitFn = Arc<dyn Fn(Exit) -> Result<(), ActorError> + Send + Sync>;

/// Queues a signal held during `started()`; `true` for a parent signal.
pub(crate) type SendHeldFn = Arc<dyn Fn(Exit, bool) -> Result<(), ActorError> + Send + Sync>;

/// Per-actor flag controlling how exit signals are handled. `false`
/// (default): a non-`Normal` signal ends the receiver. `true`: the receiver
/// gets an `Exit` message via `Actor::exit_received` (only a kill is
/// untrappable).
pub(crate) type TrapExitFlag = Arc<AtomicBool>;

/// The exit request of one actor instance (design otp-exit-signals.md,
/// "Phases of an exit"): the requested reason, kill provenance, whether
/// `stopped()` is skipped, and the death point D after which nothing changes.
#[derive(Debug, Default)]
pub(crate) struct ExitRequest {
    reason: Option<ExitReason>,
    /// A kill signal (or the supervisor's escalation) arrived before D.
    killed: bool,
    /// The exit was begun by a kill, an untrapped signal or link notice, or
    /// `stopped()` must otherwise not start (decision 2).
    skip_hook: bool,
    hook_begun: bool,
    /// Past the death point D: the reason is final.
    dead: bool,
    /// Phase 0: `started()` is running; non-kill signals are held.
    starting: bool,
    /// Reasons of the signals held during `started()`, in arrival order.
    held: Vec<ExitReason>,
    /// Held signal envelopes become `Exit`s (the actor trapped when
    /// `started()` returned); otherwise they are inert.
    held_trapped: bool,
    /// A parent signal is among the held ones.
    held_parent: bool,
    /// A parent exit is queued behind earlier messages of a trapping actor
    /// (E3): when dispatched it ends the actor whatever its reason.
    parent_exit_queued: bool,
    /// Wakes `Context::stopping` waiters when a parent exit is queued
    /// (every other pending exit cancels the stop token). Created only by a
    /// waiting `stopping()`, so an actor that never calls it pays nothing.
    #[cfg(feature = "async")]
    stopping: Option<spawned_rt::tasks::CancellationToken>,
    /// The first administrative stop that arrived during `started()`: as
    /// OTP's `gen_server:stop` (a system message handled after `init/1`
    /// returns), it is applied after a ready start commits, and `stopped()`
    /// runs (unified start 2.3).
    admin: Option<ExitReason>,
    /// A non-ready startup decision fixed the reason (unified start 2.3):
    /// the resolution and D are one step, so nothing changes it any more.
    frozen: bool,
    /// The `start_link`/`start_monitor` waits of this actor (as a parent):
    /// an exit begun by a kill or an untrapped exit signal abandons them,
    /// as in OTP the caller dies at once.
    waiting_starts: Vec<std::sync::Weak<crate::start::StartupCell>>,
    /// The start-trace role of this instance (0: not recorded).
    #[cfg(feature = "lifecycle-trace")]
    pub(crate) trace: crate::lifecycle_trace::StartRole,
    /// The current reason was imposed by a kill or an untrapped exit signal
    /// (not produced by the actor itself): no crash report (OTP: a process
    /// killed by a signal runs no code).
    signalled: bool,
}

impl ExitRequest {
    /// The requested reason (final after D).
    pub(crate) fn reason(&self) -> Option<&ExitReason> {
        self.reason.as_ref()
    }
    pub(crate) fn is_dead(&self) -> bool {
        self.dead
    }
    /// Phase 0: `started()` has not returned.
    pub(crate) fn is_starting(&self) -> bool {
        self.starting
    }
    /// The reason was imposed by a kill or an untrapped exit signal.
    pub(crate) fn is_signalled(&self) -> bool {
        self.signalled
    }
    /// `Context::exit_pending`: exit has begun (a reason is requested, which
    /// includes a kill and the dispatch of a parent exit) or a parent exit
    /// is queued for a trapping actor. Held phase-0 signals count once they
    /// are resolved when `started()` returns.
    pub(crate) fn exit_pending(&self) -> bool {
        self.reason.is_some() || self.parent_exit_queued
    }
    /// `Context::stopping`: the waiter for a parent exit queued later, or
    /// `None` when an exit is already pending. Taken under this lock, so a
    /// parent exit queued after it sees the waiter.
    #[cfg(feature = "async")]
    pub(crate) fn stopping_waiter(&mut self) -> Option<spawned_rt::tasks::CancellationToken> {
        if self.exit_pending() {
            return None;
        }
        Some(self.stopping.get_or_insert_with(Default::default).clone())
    }
    /// The `stopping()` waiter to wake once an exit is pending; the caller
    /// cancels it after releasing this lock (no waker runs under it).
    #[cfg(feature = "async")]
    pub(crate) fn take_stopping_wake(&mut self) -> Option<spawned_rt::tasks::CancellationToken> {
        if self.exit_pending() {
            self.stopping.take()
        } else {
            None
        }
    }
    /// The exit was begun by a kill or an untrapped exit signal: in OTP the
    /// process is dead at once (Spawned: once its running turn returns).
    fn exit_by_signal(&self) -> bool {
        self.killed || (self.reason.is_some() && self.skip_hook)
    }
    /// Register a start this actor waits for; `false` if its exit by a
    /// signal already began (the wait is abandoned at once).
    pub(crate) fn add_waiting_start(
        &mut self,
        cell: std::sync::Weak<crate::start::StartupCell>,
    ) -> bool {
        if self.dead || self.exit_by_signal() {
            return false;
        }
        self.waiting_starts.retain(|w| w.strong_count() > 0);
        self.waiting_starts.push(cell);
        true
    }
    /// The start was answered or abandoned: forget it.
    pub(crate) fn remove_waiting_start(&mut self, cell: *const crate::start::StartupCell) {
        self.waiting_starts
            .retain(|w| w.strong_count() > 0 && w.as_ptr() != cell);
    }
    /// The waits to abandon once a signal began this actor's exit.
    fn take_waiting_starts(&mut self) -> Vec<std::sync::Weak<crate::start::StartupCell>> {
        if self.exit_by_signal() {
            std::mem::take(&mut self.waiting_starts)
        } else {
            Vec::new()
        }
    }
    /// What began an exit that skips `stopped()`, for its debug log.
    pub(crate) fn skip_cause(&self) -> &'static str {
        if self.killed {
            "kill"
        } else {
            "exit_signal"
        }
    }
    /// `stopped()` has begun (or was skipped by the run loop).
    pub(crate) fn hook_begun(&self) -> bool {
        self.hook_begun
    }
    /// A self-exit or an administrative stop: the first requested reason
    /// wins. Returns whether the run loop must be woken.
    pub(crate) fn request(&mut self, reason: ExitReason) -> bool {
        if self.dead || self.frozen {
            return false;
        }
        if self.reason.is_none() {
            self.note_exit(&reason);
            self.reason = Some(reason);
        }
        true
    }
    /// An administrative stop (`ChildHandle::stop*`, a scope's stop). One
    /// that arrives while `started()` runs is held: it decides nothing about
    /// the start, and is applied once the start commits ready (OTP: a
    /// `gen_server:stop` waits for `init/1` to return); a failed start
    /// discards it. Returns whether the run loop must be woken.
    pub(crate) fn admin_stop(&mut self, reason: ExitReason) -> bool {
        if self.dead || self.frozen {
            return false;
        }
        if self.starting {
            if self.admin.is_none() && self.reason.is_none() {
                #[cfg(feature = "lifecycle-trace")]
                crate::lifecycle_trace::start_admin(self.trace);
                self.admin = Some(reason);
            }
            return true;
        }
        self.request(reason)
    }
    /// An untrapped non-`Normal` exit signal or link notice: begins exit
    /// without `stopped()`, or replaces the reason of an actor already
    /// exiting (decision 8), unless a kill was received.
    pub(crate) fn untrapped(&mut self, reason: ExitReason) -> bool {
        if self.dead || self.killed || self.frozen {
            return false;
        }
        if self.reason.is_none() && !self.hook_begun {
            self.skip_hook = true;
        }
        if self.reason.is_none() {
            self.note_exit(&reason);
        }
        self.reason = Some(reason);
        self.signalled = true;
        true
    }
    /// An untrappable kill: `Killed` supersedes any reason until D and
    /// skips a `stopped()` that has not begun.
    pub(crate) fn kill(&mut self) -> bool {
        if self.dead || self.frozen {
            return false;
        }
        #[cfg(feature = "lifecycle-trace")]
        crate::lifecycle_trace::start_kill(self.trace, self.starting);
        if self.reason.is_none() {
            self.note_exit(&ExitReason::Killed);
        }
        self.reason = Some(ExitReason::Killed);
        self.killed = true;
        self.signalled = true;
        if !self.hook_begun {
            self.skip_hook = true;
        }
        true
    }
    /// A panic in `started()`, a handler or `stopped()`: begins exit, or
    /// replaces the requested reason, unless a kill was received.
    pub(crate) fn panic(&mut self, reason: ExitReason) {
        if !self.dead && !self.killed && !self.frozen {
            if self.reason.is_none() {
                self.note_exit(&reason);
            }
            self.reason = Some(reason);
            self.signalled = false;
        }
    }
    /// A ready instance's exit begins (or a start-trace parent's).
    #[inline(always)]
    fn note_exit(&self, _reason: &ExitReason) {
        #[cfg(feature = "lifecycle-trace")]
        if !self.starting {
            crate::lifecycle_trace::start_exit(self.trace, _reason);
        }
    }
    /// `stopped()` is about to start: returns whether it runs.
    pub(crate) fn begin_hook(&mut self) -> bool {
        self.hook_begun = true;
        !self.skip_hook
    }
    /// The death point D: the requested reason becomes final.
    pub(crate) fn die(&mut self) -> ExitReason {
        let first = !self.dead;
        self.dead = true;
        let reason = self.reason.get_or_insert(ExitReason::Normal).clone();
        #[cfg(feature = "lifecycle-trace")]
        if first {
            crate::lifecycle_trace::start_die(self.trace, &reason, self.frozen);
        }
        let _ = first;
        reason
    }
    /// The runtime dropped the actor before D (G1): `Killed` unless a
    /// non-`Normal` reason was already requested; then D.
    pub(crate) fn runtime_drop(&mut self) -> ExitReason {
        // A non-ready startup decision fixed the reason for good (L9).
        if !self.dead && !self.frozen {
            self.reason = match self.reason.take() {
                None | Some(ExitReason::Normal) => Some(ExitReason::Killed),
                Some(reason) => Some(reason),
            };
        }
        self.die()
    }
    pub(crate) fn begin_starting(&mut self) {
        self.starting = true;
    }
    /// `started()` returned: resolve the held signals with the trap flag read
    /// now. A non-trapping actor begins exit with the first non-`Normal`
    /// one; a trapping actor dispatches each held envelope as an `Exit` in
    /// its own place in the queue. Returns whether the run loop must stop.
    #[cfg(test)]
    pub(crate) fn end_starting(&mut self, trapping: bool) -> bool {
        self.starting = false;
        let held = std::mem::take(&mut self.held);
        self.held_trapped = trapping;
        if trapping && std::mem::take(&mut self.held_parent) {
            // The held parent envelope is dispatched as a parent exit.
            self.parent_exit_queued = true;
        }
        if !trapping {
            if let Some(reason) = held.into_iter().find(|r| *r != ExitReason::Normal) {
                return self.untrapped(reason);
            }
        }
        false
    }
    pub(crate) fn held_trapped(&self) -> bool {
        self.held_trapped
    }
    /// `started()` has not run yet and a kill came first (`KilledEarly`).
    pub(crate) fn killed_early(&self) -> bool {
        self.killed
    }
    /// The startup decision (unified start, design 2.3), when `started()`
    /// returns (or instead of it). `timed_out`: the init timer committed;
    /// `open`: the child's admission is open (read under its gate). A
    /// non-ready row fixes the reason for good (the resolution and D are one
    /// step); a ready one resolves the held signals as phase 0 always did
    /// (E2, E3: a trapping child queues them, `Normal` ones are dropped).
    pub(crate) fn resolve_start(
        &mut self,
        trapping: bool,
        timed_out: bool,
        returned: &crate::start::Returned,
        open: bool,
    ) -> crate::start::Row {
        use crate::start::{Returned, Row};
        self.starting = false;
        let held = std::mem::take(&mut self.held);
        // Row 3: the first held non-Normal signal to a non-trapping child.
        // (An administrative stop is not a row: it is applied after a ready
        // commitment, below.)
        let row3 = if trapping {
            None
        } else {
            held.iter().find(|r| **r != ExitReason::Normal).cloned()
        };
        let admin = self.admin.take();
        // `signalled`: a reason imposed from outside (the init timer's kill,
        // a kill, a held untrapped signal) gets no crash report (I10).
        let row = if timed_out {
            self.reason = Some(ExitReason::Killed);
            self.signalled = true;
            Row::Timer
        } else if self.killed {
            self.reason = Some(ExitReason::Killed);
            self.signalled = true;
            Row::Kill
        } else if let Some(reason) = row3 {
            self.reason = Some(reason);
            self.signalled = true;
            Row::Signal
        } else {
            match returned {
                Returned::Panic(message) => {
                    self.reason = Some(ExitReason::Panic(format!("panic in started(): {message}")));
                    self.signalled = false;
                    Row::Panic
                }
                _ if self.reason.is_some() => Row::Stop,
                Returned::Stop(reason) => {
                    self.reason = Some(reason.clone());
                    Row::Stop
                }
                Returned::Error(_) => {
                    self.reason = Some(ExitReason::Normal);
                    Row::Error
                }
                Returned::Ignore => {
                    self.reason = Some(ExitReason::Normal);
                    Row::Ignore
                }
                Returned::Ready | Returned::NotBegun if !open => {
                    self.reason = Some(ExitReason::Shutdown(None));
                    Row::Closed
                }
                Returned::Ready | Returned::NotBegun => Row::Ready,
            }
        };
        if row == Row::Ready {
            self.held_trapped = trapping;
            if trapping && std::mem::take(&mut self.held_parent) {
                self.parent_exit_queued = true;
            }
            // A held administrative stop now begins an ordinary exit: the
            // run loop (already woken) dispatches nothing and runs stopped().
            // (Its `SExit` is recorded right after the ready `SResolve`.)
            if let Some(reason) = admin {
                self.reason = Some(reason);
            }
        } else {
            self.frozen = true;
            self.skip_hook = true;
            self.hook_begun = true;
        }
        row
    }
}

/// Per-actor exit request, shared by every handle of the instance (and by its
/// managed scope).
pub(crate) type LinkedExitReason = Arc<Mutex<ExitRequest>>;

/// Create a new empty exit request.
pub(crate) fn new_linked_exit_reason() -> LinkedExitReason {
    Arc::new(Mutex::new(ExitRequest::default()))
}

pub(crate) fn lock_request(request: &LinkedExitReason) -> std::sync::MutexGuard<'_, ExitRequest> {
    request.lock().unwrap_or_else(|p| p.into_inner())
}

/// Everything needed to deliver an exit signal to one actor instance without
/// knowing its type.
pub(crate) struct SignalTarget {
    pub(crate) id: ActorId,
    /// The actor's `start_linked` parent: its signals are parent exits.
    pub(crate) parent: Option<ActorId>,
    pub(crate) trap: TrapExitFlag,
    pub(crate) request: LinkedExitReason,
    /// Wakes the run loop (cancels its token) without recording a reason.
    pub(crate) wake: Arc<dyn Fn() + Send + Sync>,
    /// Queues an `Exit` for `exit_received`.
    pub(crate) send_exit: SendExitFn,
    /// Queues a parent exit: when dispatched, exit begins with its reason and
    /// `stopped()` runs; `exit_received` never sees it (E3).
    pub(crate) send_parent_exit: SendExitFn,
    /// Queues a signal held during `started()` in its place in the mailbox
    /// (the flag marks a parent signal).
    pub(crate) send_held: SendHeldFn,
    /// The instance's startup result (unified start, design 5.1).
    pub(crate) startup: Arc<crate::start::StartupCell>,
    /// Introspection metadata: type, placement, label, the instance probe.
    pub(crate) meta: crate::introspection::Meta,
}

/// Where an exit signal comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Origin {
    /// `ctx.exit` / `ChildHandle::exit` / `kill` (a `Kill` is untrappable).
    Direct,
    /// A death notice of a link, or a late `NoProc` (a `Kill` is trappable).
    Link,
}

/// Deliver an exit signal (OTP `exit_signal/2` semantics, design E1-E3).
/// `from_parent` marks the internal entry point of supervisors and systems.
pub(crate) fn deliver_signal(
    target: &SignalTarget,
    from: ActorId,
    reason: ExitReason,
    origin: Origin,
    from_parent: bool,
) {
    trace::life_section(target.id.raw(), || {
        if origin == Origin::Direct {
            trace::life_emit(target.id.raw(), || {
                trace::LifeEvent::new("Signal", target.id.raw())
                    .s(from.raw())
                    .x(trace::life_reason(&reason))
                    .o(if from_parent { "sup" } else { "" })
            });
        }
        apply_signal(target, from, reason, origin, from_parent)
    })
}

fn apply_signal(
    target: &SignalTarget,
    from: ActorId,
    reason: ExitReason,
    origin: Origin,
    from_parent: bool,
) {
    if origin == Origin::Direct && reason == ExitReason::Kill {
        let mut request = lock_request(&target.request);
        if request.kill() {
            let waits = request.take_waiting_starts();
            drop(request);
            trace::intro(target.id.raw(), "Kill", "");
            (target.wake)();
            abandon_waits(waits);
        }
        return;
    }
    let from_parent = from_parent || target.parent == Some(from);
    let mut request = lock_request(&target.request);
    if request.dead {
        return;
    }
    let exit = Exit { from, reason };
    if request.starting {
        // Phase 0 is not in the lifecycle trace mapping (TraceLifecycle.tla).
        trace::life_emit(target.id.raw(), || {
            trace::LifeEvent::new("Held", target.id.raw()).s(from.raw())
        });
        // Held in arrival order, and as an envelope in its mailbox place.
        request.held.push(exit.reason.clone());
        request.held_parent |= from_parent;
        let _ = (target.send_held)(exit, from_parent);
        return;
    }
    // The trap flag is read when the signal arrives.
    if target.trap.load(Ordering::Acquire) {
        if from_parent {
            // Set before the envelope is queued, under the request lock, so
            // `exit_pending` reports it to every handler that runs before it.
            request.parent_exit_queued = true;
        }
        #[cfg(feature = "async")]
        let stopping = request.take_stopping_wake();
        drop(request);
        #[cfg(feature = "async")]
        if let Some(waiter) = stopping {
            waiter.cancel();
        }
        let send = if from_parent {
            &target.send_parent_exit
        } else {
            &target.send_exit
        };
        let _ = send(exit); // admission may be closed if the peer just died
        return;
    }
    if exit.reason == ExitReason::Normal {
        return;
    }
    if request.untrapped(exit.reason) {
        let waits = request.take_waiting_starts();
        drop(request);
        (target.wake)();
        abandon_waits(waits);
    }
}

/// The actor's exit by a signal began: its pending start waits are
/// abandoned (outside its exit-request lock).
fn abandon_waits(waits: Vec<std::sync::Weak<crate::start::StartupCell>>) {
    for cell in waits {
        if let Some(cell) = cell.upgrade() {
            cell.parent_exit();
        }
    }
}

mod sync {
    pub(crate) use std::sync::{Arc, Mutex, MutexGuard, Weak};
    pub(crate) type LinkRecordAlive =
        crate::instrumentation::Live<crate::instrumentation::gauge::LinkRecords>;
}
mod table;
pub(crate) use table::{new_link_table, LinkNotices, LinkOutcome, LinkTable};

/// Create a new trap_exit flag (default: false).
pub(crate) fn new_trap_exit_flag() -> TrapExitFlag {
    Arc::new(AtomicBool::new(false))
}

static NEXT_EDGE: AtomicU64 = AtomicU64::new(1);

/// The edge of a `start_link`, in the start's commit (see
/// [`table::install_start_edge`]). Returns whether it was installed.
pub(crate) fn install_start_edge(
    parent: &Arc<SignalTarget>,
    parent_links: &LinkTable,
    child: &Arc<SignalTarget>,
    child_links: &LinkTable,
    admit: impl FnOnce() -> bool,
) -> bool {
    let gen = NEXT_EDGE.fetch_add(1, Ordering::Relaxed);
    table::install_start_edge(parent, parent_links, child, child_links, gen, admit)
}

/// Link `own` and `peer` (OTP `link/1`). An existing record (live or
/// pending) makes it a no-op. Past the peer's D, `try_only` returns
/// `NoProc` without a record; otherwise a pending late record is created and
/// its `NoProc` notice is delivered through the receiver-side step, so a
/// concurrent `unlink` still fences it.
pub(crate) fn register_link(
    own: &Arc<SignalTarget>,
    own_links: &LinkTable,
    peer: &Arc<SignalTarget>,
    peer_links: &LinkTable,
    try_only: bool,
) -> LinkOutcome {
    if own.id == peer.id || Arc::ptr_eq(own_links, peer_links) {
        return LinkOutcome::Unchanged;
    }
    let (outcome, delivery) = trace::life_section(own.id.raw(), || {
        link_in_section(own, own_links, peer, peer_links, try_only)
    });
    // Late-link delivery shares Link's trace step. Dispose its record and
    // any caught panic only after that outermost step has released its lock.
    delivery.dispose();
    outcome
}

fn link_in_section(
    own: &Arc<SignalTarget>,
    own_links: &LinkTable,
    peer: &Arc<SignalTarget>,
    peer_links: &LinkTable,
    try_only: bool,
) -> (LinkOutcome, DeliveryCleanup) {
    // Sender lifetime (design Terms, semantics X2): `link` works only while
    // the caller runs. A caller already exiting at this check is inert:
    // no record, no notice, no NoProc. A concurrent exit can linearize after
    // this check; the table transaction below still rechecks D, so it cannot
    // install an edge past the caller's death. A dead OTP process cannot link.
    if lock_request(&own.request).reason().is_some() {
        return (LinkOutcome::Unchanged, DeliveryCleanup::default());
    }
    fail_point!("link.before_install");
    let gen = NEXT_EDGE.fetch_add(1, Ordering::Relaxed);
    let outcome = table::install(own, own_links, peer, peer_links, gen, try_only);
    if outcome != LinkOutcome::Unchanged {
        trace::life_emit(own.id.raw(), || {
            trace::LifeEvent::new("Link", own.id.raw())
                .s(peer.id.raw())
                .x(match (&outcome, try_only) {
                    (LinkOutcome::Linked, _) => "linked",
                    (_, false) => "noproc",
                    (_, true) => "try",
                })
        });
    }
    let delivery = if outcome == LinkOutcome::NoProc && !try_only {
        deliver_notice_in_section(own_links, own, peer.id, gen, ExitReason::NoProc)
    } else {
        DeliveryCleanup::default()
    };
    (outcome, delivery)
}

/// Unlink removes both records, live or pending: once it returns, no notice
/// of that edge takes effect (E6). An `Exit` already queued stays.
pub(crate) fn unregister_link(
    own_id: ActorId,
    own: &LinkTable,
    peer_id: ActorId,
    peer: &LinkTable,
) {
    if own_id == peer_id || Arc::ptr_eq(own, peer) {
        return;
    }
    let removed = trace::life_section(own_id.raw(), || {
        let removed = table::remove_pair(own_id, own, peer_id, peer);
        if removed.0.is_some() || removed.1.is_some() {
            trace::life_emit(own_id.raw(), || {
                trace::LifeEvent::new("Unlink", own_id.raw()).s(peer_id.raw())
            });
            #[cfg(feature = "lifecycle-trace")]
            trace::start_unlink(own_id.raw(), peer_id.raw());
        }
        removed
    });
    drop(removed);
}

/// Receiver-side delivery: under the receiver's table lock, apply the notice
/// only if the record with that peer and generation is still present, and
/// remove it. `unlink` removes records under the same lock (the fence).
fn deliver_notice(
    receiver_links: &LinkTable,
    receiver: &SignalTarget,
    from: ActorId,
    gen: u64,
    reason: ExitReason,
) {
    let cleanup = trace::life_section(receiver.id.raw(), || {
        deliver_notice_in_section(receiver_links, receiver, from, gen, reason)
    });
    cleanup.dispose();
}

/// Effects of a fenced delivery that may run foreign code when destroyed.
/// They leave both the receiver table and the outermost trace section first.
#[derive(Default)]
#[must_use = "dispose delivery effects after the outermost trace section"]
struct DeliveryCleanup {
    record: Option<table::LinkEntry>,
    panic: Option<Box<dyn std::any::Any + Send>>,
}
impl DeliveryCleanup {
    fn dispose(self) {
        if let Some(payload) = self.panic {
            crate::mailbox::dispose_panic(payload);
            crate::actor_cleanup::log_safely(|| {
                tracing::error!("Panic while delivering a linked exit");
            });
        }
        drop(self.record);
    }
}

fn deliver_notice_in_section(
    receiver_links: &LinkTable,
    receiver: &SignalTarget,
    from: ActorId,
    gen: u64,
    reason: ExitReason,
) -> DeliveryCleanup {
    let mut panic = None;
    let record = table::take_record(receiver_links, from, gen, |record| {
        trace::life_emit(receiver.id.raw(), || {
            trace::LifeEvent::new("Deliver", receiver.id.raw())
                .s(from.raw())
                .x(trace::life_reason(&reason))
                .k("link")
                .o(if record.late { "late" } else { "death" })
        });
        // One failing internal callback must not prevent other peers from
        // being notified, or panic again during actor-future destruction.
        panic = catch_unwind(AssertUnwindSafe(|| {
            trace::start_notice(receiver.id.raw(), from.raw(), "link", &reason, || {
                deliver_signal(receiver, from, reason.clone(), Origin::Link, false)
            })
        }))
        .err();
    });
    DeliveryCleanup { record, panic }
}

impl LinkNotices {
    /// Deliver every notice (publication). Each is applied only if the
    /// receiver still holds the record (E6 fence).
    pub(crate) fn deliver(self) {
        self.deliver_with(deliver_notice);
    }
    /// Take the notice to `peer` (a unified start's parent edge) out of the
    /// set, for its startup cell to dispose of.
    pub(crate) fn take_notice(&mut self, peer: ActorId) -> Option<LinkNotice> {
        self.take_peer(peer)
            .map(|(from, reason, entry)| LinkNotice {
                from,
                reason,
                entry,
            })
    }
}

/// One death notice taken at D (a unified start's parent edge), held by the
/// child's startup cell until it is disposed of (design 5.2).
pub(crate) struct LinkNotice {
    from: ActorId,
    reason: ExitReason,
    entry: table::LinkEntry,
}

impl LinkNotice {
    /// Deliver it as an ordinary notice (fenced by the receiver's record).
    pub(crate) fn deliver(self) {
        if let Some(peer_links) = self.entry.peer_links.upgrade() {
            deliver_notice(
                &peer_links,
                &self.entry.peer,
                self.from,
                self.entry.gen,
                self.reason,
            );
        }
    }
    /// Consumed by a failed start's answer: the receiver's record goes, so
    /// no notice of this edge takes effect.
    pub(crate) fn discard(self) {
        if let Some(peer_links) = self.entry.peer_links.upgrade() {
            let removed = table::take_record(&peer_links, self.from, self.entry.gen, |_| {});
            drop(removed);
        }
    }
}

/// Close the exit table at D: set the final reason and take one notice per
/// link. Takes only this actor's own lock. Terminal closure is idempotent.
#[cfg(test)]
pub(crate) fn close_links(own_id: ActorId, own: &LinkTable, reason: &ExitReason) -> LinkNotices {
    close_links_at_death(own_id, own, reason, |_| {})
}

/// The exit-table part of the death point D. `at_death` runs inside the same
/// critical section (it must not run user code): the reply registry seals
/// the final reason there, so a registration that finds the table closed
/// (`NoProc`) and a send that finds the registry sealed (`NoProc`) agree.
pub(crate) fn close_links_at_death(
    own_id: ActorId,
    own: &LinkTable,
    reason: &ExitReason,
    at_death: impl FnOnce(&ExitReason),
) -> LinkNotices {
    table::close(own_id, own, reason, at_death)
}

/// Close and deliver at once (tests and paths with nothing in between).
#[cfg(test)]
pub(crate) fn propagate_exit(own_id: ActorId, own: &LinkTable, reason: &ExitReason) {
    close_links(own_id, own, reason).deliver();
}

/// A self-exit or administrative stop commits its cause before waking. The
/// first requested reason wins over later self-exits and stops; a later
/// untrapped signal or kill can still replace it until D.
pub(crate) fn normal_stop(id: ActorId, reason: &LinkedExitReason, cancel: impl FnOnce()) {
    admin_stop_with_reason(id, reason, ExitReason::Normal, cancel);
}

/// A self-exit of actor `id` (recorded as `Stop`).
pub(crate) fn stop_with_reason(
    id: ActorId,
    reason: &LinkedExitReason,
    cause: ExitReason,
    cancel: impl FnOnce(),
) {
    trace::life_section(id.raw(), || {
        trace::life_emit(id.raw(), || {
            trace::LifeEvent::new("Stop", id.raw()).x(trace::life_reason(&cause))
        });
        if lock_request(reason).request(cause) {
            trace::intro(id.raw(), "Stop", "");
            cancel();
        }
    });
}

/// An administrative stop of actor `id` (`ChildHandle::stop*`, recorded as
/// `Stop`): during `started()` it decides the start (unified start row 3).
pub(crate) fn admin_stop_with_reason(
    id: ActorId,
    reason: &LinkedExitReason,
    cause: ExitReason,
    cancel: impl FnOnce(),
) {
    trace::life_section(id.raw(), || {
        trace::life_emit(id.raw(), || {
            trace::LifeEvent::new("Stop", id.raw()).x(trace::life_reason(&cause))
        });
        if lock_request(reason).admin_stop(cause) {
            trace::intro(id.raw(), "Stop", "");
            cancel();
        }
    });
}

/// As [`admin_stop_with_reason`], unrecorded: a parent exit being
/// dispatched (part of the model's `Dispatch`) or a managed scope's stop.
pub(crate) fn request_stop(reason: &LinkedExitReason, cause: ExitReason, cancel: impl FnOnce()) {
    if lock_request(reason).admin_stop(cause) {
        cancel();
    }
}

/// The peers of this actor's live links (not pending late `NoProc`
/// records), sorted by id (introspection, out of band).
pub(crate) fn live_links(table: &LinkTable) -> Vec<ActorId> {
    table::live_peers(table)
}

#[cfg(test)]
pub(crate) fn has_link(table: &LinkTable, peer: ActorId) -> bool {
    table
        .lock()
        .unwrap()
        .entries
        .iter()
        .any(|entry| entry.peer_id == peer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    #[test]
    fn delivery_contains_callback_payload_and_subscriber_panics() {
        crate::drop_subprocess::run(
            "link::tests::delivery_panic_child",
            "validated linked delivery panic containment",
        );
    }

    #[test]
    #[ignore = "subprocess helper exercised by the enabled parent"]
    fn delivery_panic_child() {
        if std::env::var_os("SPAWNED_MAILBOX_DROP_CHILD").is_none() {
            return;
        }
        static SECONDARY_DROPS: AtomicUsize = AtomicUsize::new(0);
        struct Secondary;
        impl Drop for Secondary {
            fn drop(&mut self) {
                SECONDARY_DROPS.fetch_add(1, Ordering::SeqCst);
            }
        }
        struct Payload(Arc<AtomicUsize>);
        impl Drop for Payload {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
                // dispose_panic intentionally forgets this secondary payload
                // to bound recursive destructor panics. A zero-sized payload
                // exercises that path without an intentional heap leak, so
                // LSan remains enabled in this subprocess as well as its parent.
                std::panic::panic_any(Secondary);
            }
        }
        struct PanicSubscriber(Arc<AtomicUsize>);
        impl tracing::Subscriber for PanicSubscriber {
            fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
                true
            }
            fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
                tracing::span::Id::from_u64(1)
            }
            fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
            fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
            fn event(&self, _: &tracing::Event<'_>) {
                std::panic::panic_any(Payload(self.0.clone()));
            }
            fn enter(&self, _: &tracing::span::Id) {}
            fn exit(&self, _: &tracing::span::Id) {}
        }
        let drops = Arc::new(AtomicUsize::new(0));
        let callback_drops = drops.clone();
        let mut receiver = target(&Arc::new(AtomicUsize::new(0)));
        receiver.wake = Arc::new(move || std::panic::panic_any(Payload(callback_drops.clone())));
        let receiver = Arc::new(receiver);
        let sender = Arc::new(target(&Arc::new(AtomicUsize::new(0))));
        let (a, b) = (new_link_table(), new_link_table());
        register_link(&receiver, &a, &sender, &b, false);
        let result = catch_unwind(AssertUnwindSafe(|| {
            tracing::subscriber::with_default(PanicSubscriber(drops.clone()), || {
                propagate_exit(sender.id, &b, &ExitReason::Kill);
            });
        }));
        assert!(result.is_ok(), "delivery unwound through its caller");
        assert_eq!(drops.load(Ordering::SeqCst), 2);
        assert_eq!(
            SECONDARY_DROPS.load(Ordering::SeqCst),
            0,
            "secondary payloads must be forgotten to bound destructor recursion"
        );
        println!("validated linked delivery panic containment");
    }

    #[test]
    fn delivery_diagnostics_run_outside_protocol_locks() {
        crate::drop_subprocess::run(
            "link::tests::delivery_lock_child",
            "validated linked delivery lock release",
        );
    }

    #[test]
    #[ignore = "subprocess helper exercised by the enabled parent"]
    fn delivery_lock_child() {
        if std::env::var_os("SPAWNED_MAILBOX_DROP_CHILD").is_none() {
            return;
        }
        #[derive(Clone)]
        struct Probe {
            table: LinkTable,
            results: Arc<Mutex<Vec<bool>>>,
        }
        impl Probe {
            fn check(&self) {
                let table_free = self.table.try_lock().is_ok();
                let (sent, received) = std::sync::mpsc::channel();
                let worker = std::thread::spawn(move || {
                    trace::atomic(|| {
                        let _ = sent.send(());
                    });
                });
                let trace_free = received
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .is_ok();
                if trace_free {
                    worker.join().unwrap();
                }
                // On failure the subprocess exits after the assertion. Do
                // not join a probe still blocked on the lock being tested.
                self.results.lock().unwrap().push(table_free && trace_free);
            }
        }
        struct Payload(Probe);
        impl Drop for Payload {
            fn drop(&mut self) {
                self.0.check();
            }
        }
        struct Subscriber(Probe);
        impl tracing::Subscriber for Subscriber {
            fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
                true
            }
            fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
                tracing::span::Id::from_u64(1)
            }
            fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
            fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
            fn event(&self, _: &tracing::Event<'_>) {
                self.0.check();
            }
            fn enter(&self, _: &tracing::span::Id) {}
            fn exit(&self, _: &tracing::span::Id) {}
        }
        #[cfg(feature = "lifecycle-trace")]
        trace::record_lifecycle();
        for late in [false, true] {
            let (a, b) = (new_link_table(), new_link_table());
            let results = Arc::new(Mutex::new(Vec::new()));
            let probe = Probe {
                table: a.clone(),
                results: results.clone(),
            };
            let mut receiver = target(&Arc::new(AtomicUsize::new(0)));
            let callback_probe = probe.clone();
            receiver.wake =
                Arc::new(move || std::panic::panic_any(Payload(callback_probe.clone())));
            let receiver = Arc::new(receiver);
            let sender = Arc::new(target(&Arc::new(AtomicUsize::new(0))));
            #[cfg(feature = "lifecycle-trace")]
            {
                trace::life_spawn(receiver.id.raw(), 0);
                trace::life_spawn(sender.id.raw(), 0);
            }
            if late {
                propagate_exit(sender.id, &b, &ExitReason::Normal);
            }
            tracing::subscriber::with_default(Subscriber(probe), || {
                register_link(&receiver, &a, &sender, &b, false);
                if !late {
                    propagate_exit(sender.id, &b, &ExitReason::Kill);
                }
            });
            assert_eq!(*results.lock().unwrap(), vec![true, true], "late={late}");
        }
        #[cfg(feature = "lifecycle-trace")]
        let _ = trace::take_lifecycle();
        println!("validated linked delivery lock release");
    }

    #[test]
    fn exit_is_clone_and_eq() {
        let e = Exit {
            from: ActorId::next(),
            reason: ExitReason::Normal,
        };
        assert_eq!(e.clone(), e);
    }
    #[test]
    fn new_state_is_open_and_empty() {
        let state = new_link_table();
        let state = state.lock().unwrap();
        assert!(state.entries.is_empty() && state.exited.is_none());
    }
    #[test]
    fn trap_exit_defaults_false() {
        assert!(!new_trap_exit_flag().load(Ordering::Acquire));
    }
    /// A receiver whose wake and Exit deliveries are counted.
    fn target(counter: &Arc<AtomicUsize>) -> SignalTarget {
        let (c1, c2, c3) = (counter.clone(), counter.clone(), counter.clone());
        SignalTarget {
            id: ActorId::next(),
            parent: None,
            trap: new_trap_exit_flag(),
            request: new_linked_exit_reason(),
            wake: Arc::new(move || {
                c1.fetch_add(1, Ordering::SeqCst);
            }),
            send_exit: Arc::new(move |_| {
                c2.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }),
            send_parent_exit: Arc::new(move |_| {
                c3.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }),
            send_held: Arc::new(|_, _| Ok(())),
            startup: crate::start::StartupCell::new(),
            meta: Default::default(),
        }
    }
    fn pair(
        seen: &Arc<AtomicUsize>,
    ) -> (Arc<SignalTarget>, LinkTable, Arc<SignalTarget>, LinkTable) {
        (
            Arc::new(target(seen)),
            new_link_table(),
            Arc::new(target(seen)),
            new_link_table(),
        )
    }
    #[test]
    fn closure_is_terminal_and_delivers_each_edge_once() {
        let seen = Arc::new(AtomicUsize::new(0));
        let (a, at, b, bt) = pair(&seen);
        assert_eq!(register_link(&a, &at, &b, &bt, false), LinkOutcome::Linked);
        propagate_exit(a.id, &at, &ExitReason::Panic("x".into()));
        propagate_exit(a.id, &at, &ExitReason::Panic("x".into()));
        assert_eq!(seen.load(Ordering::SeqCst), 1);
        assert!(bt.lock().unwrap().entries.is_empty());
        // b links to the dead a: a late NoProc notice, delivered and removed.
        // b is exiting (the notice began its exit): its link is inert
        // (model: Link requires a running caller).
        assert_eq!(
            register_link(&b, &bt, &a, &at, false),
            LinkOutcome::Unchanged
        );
        assert_eq!(seen.load(Ordering::SeqCst), 1);
        // A running c linking to the dead a gets the late NoProc notice.
        let c = Arc::new(target(&seen));
        let ct = new_link_table();
        assert_eq!(register_link(&c, &ct, &a, &at, false), LinkOutcome::NoProc);
        assert_eq!(seen.load(Ordering::SeqCst), 2);
        assert_eq!(lock_request(&c.request).reason(), Some(&ExitReason::NoProc));
        assert!(ct.lock().unwrap().entries.is_empty());
        assert!(at.lock().unwrap().entries.is_empty() && bt.lock().unwrap().entries.is_empty());
    }
    #[test]
    fn try_link_to_a_dead_actor_keeps_no_record() {
        let seen = Arc::new(AtomicUsize::new(0));
        let (a, at, b, bt) = pair(&seen);
        propagate_exit(b.id, &bt, &ExitReason::Normal);
        assert_eq!(register_link(&a, &at, &b, &bt, true), LinkOutcome::NoProc);
        assert_eq!(seen.load(Ordering::SeqCst), 0);
        assert!(at.lock().unwrap().entries.is_empty());
        assert_eq!(lock_request(&a.request).reason(), None);
    }
    #[test]
    fn unlink_fences_a_notice_taken_at_death() {
        let seen = Arc::new(AtomicUsize::new(0));
        let (a, at, b, bt) = pair(&seen);
        register_link(&a, &at, &b, &bt, false);
        let notices = close_links(a.id, &at, &ExitReason::Panic("x".into()));
        unregister_link(b.id, &bt, a.id, &at);
        notices.deliver();
        assert_eq!(
            seen.load(Ordering::SeqCst),
            0,
            "unlink returned before delivery"
        );
        // A relink while the notice is pending is a no-op, as an existing record.
        let (c, ct, d, dt) = pair(&seen);
        register_link(&c, &ct, &d, &dt, false);
        let notices = close_links(c.id, &ct, &ExitReason::Panic("y".into()));
        assert_eq!(
            register_link(&d, &dt, &c, &ct, false),
            LinkOutcome::Unchanged
        );
        notices.deliver();
        assert_eq!(seen.load(Ordering::SeqCst), 1);
    }
    #[test]
    fn signal_rules_follow_exit_signal_2() {
        let seen = Arc::new(AtomicUsize::new(0));
        let t = target(&seen);
        let from = ActorId::next();
        // Normal is ignored by a non-trapping target.
        deliver_signal(&t, from, ExitReason::Normal, Origin::Direct, false);
        assert_eq!(lock_request(&t.request).reason(), None);
        // Any other reason begins exit without stopped().
        deliver_signal(&t, from, ExitReason::Shutdown(None), Origin::Direct, false);
        assert_eq!(
            lock_request(&t.request).reason(),
            Some(&ExitReason::Shutdown(None))
        );
        assert!(!lock_request(&t.request).begin_hook());
        // Trapping: queued as Exit, nothing committed.
        let t = target(&seen);
        t.trap.store(true, Ordering::Release);
        deliver_signal(&t, from, ExitReason::Shutdown(None), Origin::Direct, false);
        assert_eq!(lock_request(&t.request).reason(), None);
        // A Kill signal is untrappable and ends Killed; a Kill link notice is trappable.
        deliver_signal(&t, from, ExitReason::Kill, Origin::Link, false);
        assert_eq!(lock_request(&t.request).reason(), None);
        deliver_signal(&t, from, ExitReason::Kill, Origin::Direct, false);
        assert_eq!(lock_request(&t.request).reason(), Some(&ExitReason::Killed));
    }
    #[test]
    fn exit_pending_counts_begun_exits_and_queued_parent_exits() {
        let seen = Arc::new(AtomicUsize::new(0));
        let from = ActorId::next();
        let pending = |t: &SignalTarget| lock_request(&t.request).exit_pending();
        // Trapping: a peer's Exit is not pending; a parent's signal is.
        let t = target(&seen);
        t.trap.store(true, Ordering::Release);
        deliver_signal(&t, from, ExitReason::Shutdown(None), Origin::Link, false);
        assert!(!pending(&t));
        deliver_signal(&t, from, ExitReason::Normal, Origin::Direct, true);
        assert!(pending(&t));
        assert_eq!(lock_request(&t.request).reason(), None);
        // Non-trapping: an untrapped signal begins exit; Normal is ignored.
        let t = target(&seen);
        deliver_signal(&t, from, ExitReason::Normal, Origin::Direct, true);
        assert!(!pending(&t));
        deliver_signal(&t, from, ExitReason::Shutdown(None), Origin::Direct, false);
        assert!(pending(&t));
        // Phase 0: held signals count when started() returns.
        let t = target(&seen);
        lock_request(&t.request).begin_starting();
        deliver_signal(&t, from, ExitReason::Normal, Origin::Direct, true);
        assert!(!pending(&t));
        lock_request(&t.request).end_starting(true);
        assert!(pending(&t), "a held parent exit of a trapping actor");
        let t = target(&seen);
        lock_request(&t.request).begin_starting();
        deliver_signal(&t, from, ExitReason::Normal, Origin::Direct, true);
        deliver_signal(&t, from, ExitReason::Shutdown(None), Origin::Direct, false);
        lock_request(&t.request).end_starting(false);
        assert!(pending(&t), "a held untrapped signal begins exit");
        let t = target(&seen);
        lock_request(&t.request).begin_starting();
        deliver_signal(&t, from, ExitReason::Normal, Origin::Direct, true);
        lock_request(&t.request).end_starting(false);
        assert!(
            !pending(&t),
            "a held Normal parent signal is ignored untrapped"
        );
    }
    #[cfg(feature = "async")]
    #[test]
    fn stopping_waiter_wakes_on_a_queued_parent_exit() {
        let seen = Arc::new(AtomicUsize::new(0));
        let from = ActorId::next();
        let waiter = |t: &SignalTarget| lock_request(&t.request).stopping_waiter();
        // A queued parent exit to a trapping actor cancels the waiter; a
        // trapped peer Exit does not.
        let t = target(&seen);
        t.trap.store(true, Ordering::Release);
        let w = waiter(&t).unwrap();
        deliver_signal(&t, from, ExitReason::Shutdown(None), Origin::Link, false);
        assert!(!w.is_cancelled());
        deliver_signal(&t, from, ExitReason::Shutdown(None), Origin::Direct, true);
        assert!(w.is_cancelled());
        assert!(waiter(&t).is_none(), "pending: no new waiter");
        // A held parent exit queued by the ready startup decision: the run
        // loop takes the wake out of the same lock.
        let t = target(&seen);
        lock_request(&t.request).begin_starting();
        let w = waiter(&t).unwrap();
        deliver_signal(&t, from, ExitReason::Normal, Origin::Direct, true);
        let mut request = lock_request(&t.request);
        assert!(request.take_stopping_wake().is_none());
        let row = request.resolve_start(true, false, &crate::start::Returned::Ready, true);
        assert_eq!(row, crate::start::Row::Ready);
        assert!(request.take_stopping_wake().is_some_and(|wake| {
            wake.cancel();
            w.is_cancelled()
        }));
    }
    #[test]
    fn reason_precedence_until_death() {
        crate::test_wait::bounded(
            concat!(module_path!(), "::reason_precedence_until_death"),
            move || {
                // A late non-kill signal replaces a requested reason (decision 8);
                // a kill supersedes it and nothing replaces Killed.
                let mut r = ExitRequest::default();
                assert!(r.request(ExitReason::Normal));
                assert!(r.untrapped(ExitReason::Shutdown(None)));
                assert_eq!(r.reason(), Some(&ExitReason::Shutdown(None)));
                assert!(
                    r.begin_hook(),
                    "a late signal does not skip a self-exit's stopped()"
                );
                r.panic(ExitReason::Panic("stopped".into()));
                assert_eq!(r.reason(), Some(&ExitReason::Panic("stopped".into())));
                assert!(r.kill());
                r.panic(ExitReason::Panic("after kill".into()));
                assert!(!r.untrapped(ExitReason::Shutdown(None)));
                assert!(r.request(ExitReason::Normal));
                assert_eq!(r.die(), ExitReason::Killed);
                assert!(!r.kill() && !r.untrapped(ExitReason::Shutdown(None)));
                // Runtime drop: Killed unless a non-Normal reason was requested.
                let mut r = ExitRequest::default();
                r.request(ExitReason::Normal);
                assert_eq!(r.runtime_drop(), ExitReason::Killed);
                let mut r = ExitRequest::default();
                r.request(ExitReason::Shutdown(None));
                assert_eq!(r.runtime_drop(), ExitReason::Shutdown(None));
                // Review #291 item 8: a non-ready startup decision froze the
                // reason (Init::error: Normal); a runtime drop before D keeps it.
                let mut r = ExitRequest::default();
                r.begin_starting();
                let row = r.resolve_start(
                    false,
                    false,
                    &crate::start::Returned::Error("no config".into()),
                    true,
                );
                assert_eq!(row, crate::start::Row::Error);
                assert_eq!(r.runtime_drop(), ExitReason::Normal);
            },
        );
    }
    #[test]
    fn link_from_an_exiting_or_dead_caller_is_inert() {
        crate::test_wait::bounded(
            concat!(
                module_path!(),
                "::link_from_an_exiting_or_dead_caller_is_inert"
            ),
            move || {
                let seen = Arc::new(AtomicUsize::new(0));
                // Exit begun: no record on either side, even to a live peer.
                let (a, at, b, bt) = pair(&seen);
                lock_request(&a.request).request(ExitReason::Normal);
                assert_eq!(
                    register_link(&a, &at, &b, &bt, false),
                    LinkOutcome::Unchanged
                );
                assert!(
                    at.lock().unwrap().entries.is_empty() && bt.lock().unwrap().entries.is_empty()
                );
                // Past D, even to a dead peer: no NoProc notice, no try_link error.
                let (c, ct, d, dt) = pair(&seen);
                lock_request(&c.request).die();
                propagate_exit(c.id, &ct, &ExitReason::Normal);
                propagate_exit(d.id, &dt, &ExitReason::Normal);
                assert_eq!(
                    register_link(&c, &ct, &d, &dt, false),
                    LinkOutcome::Unchanged
                );
                assert_eq!(
                    register_link(&c, &ct, &d, &dt, true),
                    LinkOutcome::Unchanged
                );
                assert_eq!(seen.load(Ordering::SeqCst), 0);
            },
        );
    }
    #[test]
    fn self_link_is_noop() {
        let seen = Arc::new(AtomicUsize::new(0));
        let a = Arc::new(target(&seen));
        let at = new_link_table();
        register_link(&a, &at, &a, &at, false);
        assert!(at.lock().unwrap().entries.is_empty());
        assert_eq!(Arc::strong_count(&at), 1);
    }
    #[test]
    fn weak_edges_do_not_keep_link_tables_alive() {
        let c = Arc::new(AtomicUsize::new(0));
        let (a, at, b, bt) = pair(&c);
        let weak_a = Arc::downgrade(&at);
        let weak_b = Arc::downgrade(&bt);
        register_link(&a, &at, &b, &bt, false);
        drop(at);
        drop(bt);
        assert!(weak_a.upgrade().is_none() && weak_b.upgrade().is_none());
    }
    #[test]
    fn concurrent_link_unlink_preserves_symmetry() {
        let c = Arc::new(AtomicUsize::new(0));
        let (a, at, b, bt) = pair(&c);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                for _ in 0..2000 {
                    register_link(&a, &at, &b, &bt, false);
                    unregister_link(a.id, &at, b.id, &bt);
                }
            });
            scope.spawn(|| {
                for _ in 0..2000 {
                    register_link(&b, &bt, &a, &at, false);
                    unregister_link(b.id, &bt, a.id, &at);
                }
            });
        });
        assert_eq!(
            at.lock().unwrap().entries.len(),
            bt.lock().unwrap().entries.len()
        );
        assert!(at.lock().unwrap().entries.len() <= 1);
    }
}
