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
pub struct Exit {
    /// The actor that died.
    pub from: ActorId,
    /// Why it stopped.
    pub reason: ExitReason,
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
}

impl ExitRequest {
    /// The requested reason (final after D).
    pub(crate) fn reason(&self) -> Option<&ExitReason> {
        self.reason.as_ref()
    }
    pub(crate) fn is_dead(&self) -> bool {
        self.dead
    }
    /// `Context::exit_pending`: exit has begun (a reason is requested, which
    /// includes a kill and the dispatch of a parent exit) or a parent exit
    /// is queued for a trapping actor. Held phase-0 signals count once they
    /// are resolved when `started()` returns.
    pub(crate) fn exit_pending(&self) -> bool {
        self.reason.is_some() || self.parent_exit_queued
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
        if self.dead {
            return false;
        }
        if self.reason.is_none() {
            self.reason = Some(reason);
        }
        true
    }
    /// An untrapped non-`Normal` exit signal or link notice: begins exit
    /// without `stopped()`, or replaces the reason of an actor already
    /// exiting (decision 8), unless a kill was received.
    pub(crate) fn untrapped(&mut self, reason: ExitReason) -> bool {
        if self.dead || self.killed {
            return false;
        }
        if self.reason.is_none() && !self.hook_begun {
            self.skip_hook = true;
        }
        self.reason = Some(reason);
        true
    }
    /// An untrappable kill: `Killed` supersedes any reason until D and
    /// skips a `stopped()` that has not begun.
    pub(crate) fn kill(&mut self) -> bool {
        if self.dead {
            return false;
        }
        self.reason = Some(ExitReason::Killed);
        self.killed = true;
        if !self.hook_begun {
            self.skip_hook = true;
        }
        true
    }
    /// A panic in `started()`, a handler or `stopped()`: begins exit, or
    /// replaces the requested reason, unless a kill was received.
    pub(crate) fn panic(&mut self, reason: ExitReason) {
        if !self.dead && !self.killed {
            self.reason = Some(reason);
        }
    }
    /// `stopped()` is about to start: returns whether it runs.
    pub(crate) fn begin_hook(&mut self) -> bool {
        self.hook_begun = true;
        !self.skip_hook
    }
    /// The death point D: the requested reason becomes final.
    pub(crate) fn die(&mut self) -> ExitReason {
        self.dead = true;
        self.reason.get_or_insert(ExitReason::Normal).clone()
    }
    /// The runtime dropped the actor before D (G1): `Killed` unless a
    /// non-`Normal` reason was already requested; then D.
    pub(crate) fn runtime_drop(&mut self) -> ExitReason {
        if !self.dead {
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
        if lock_request(&target.request).kill() {
            (target.wake)();
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
        drop(request);
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
        drop(request);
        (target.wake)();
    }
}

mod sync {
    pub(crate) use std::sync::{Arc, Mutex, MutexGuard, Weak};
}
mod table;
pub(crate) use table::{new_link_table, LinkNotices, LinkOutcome, LinkTable};

/// Create a new trap_exit flag (default: false).
pub(crate) fn new_trap_exit_flag() -> TrapExitFlag {
    Arc::new(AtomicBool::new(false))
}

static NEXT_EDGE: AtomicU64 = AtomicU64::new(1);

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
    trace::life_section(own.id.raw(), || {
        link_in_section(own, own_links, peer, peer_links, try_only)
    })
}

fn link_in_section(
    own: &Arc<SignalTarget>,
    own_links: &LinkTable,
    peer: &Arc<SignalTarget>,
    peer_links: &LinkTable,
    try_only: bool,
) -> LinkOutcome {
    // Sender lifetime (design Terms, semantics X2): `link` works only while
    // the caller runs. Once its exit began (or past its D, checked again
    // under the table lock below) the call is inert: no record, no notice,
    // no NoProc. A dead process cannot call link in OTP.
    if lock_request(&own.request).reason().is_some() {
        return LinkOutcome::Unchanged;
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
    if outcome == LinkOutcome::NoProc && !try_only {
        deliver_notice(own_links, own, peer.id, gen, ExitReason::NoProc);
    }
    outcome
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
    trace::life_section(receiver.id.raw(), || {
        deliver_notice_in_section(receiver_links, receiver, from, gen, reason)
    })
}

fn deliver_notice_in_section(
    receiver_links: &LinkTable,
    receiver: &SignalTarget,
    from: ActorId,
    gen: u64,
    reason: ExitReason,
) {
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
        if let Err(payload) = catch_unwind(AssertUnwindSafe(|| {
            deliver_signal(receiver, from, reason, Origin::Link, false)
        })) {
            crate::mailbox::dispose_panic(payload);
            crate::actor_cleanup::log_safely(|| {
                tracing::error!("Panic while delivering a linked exit");
            });
        }
    });
    drop(record);
}

impl LinkNotices {
    /// Deliver every notice (publication). Each is applied only if the
    /// receiver still holds the record (E6 fence).
    pub(crate) fn deliver(self) {
        self.deliver_with(deliver_notice);
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
    stop_with_reason(id, reason, ExitReason::Normal, cancel);
}

/// A self-exit or administrative stop of actor `id` (recorded as `Stop`).
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
        request_stop(reason, cause, cancel);
    });
}

/// As [`stop_with_reason`], unrecorded: a parent exit being dispatched (part
/// of the model's `Dispatch`) or a managed scope's stop.
pub(crate) fn request_stop(reason: &LinkedExitReason, cause: ExitReason, cancel: impl FnOnce()) {
    if lock_request(reason).request(cause) {
        cancel();
    }
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
        deliver_signal(&t, from, ExitReason::Shutdown, Origin::Direct, false);
        assert_eq!(
            lock_request(&t.request).reason(),
            Some(&ExitReason::Shutdown)
        );
        assert!(!lock_request(&t.request).begin_hook());
        // Trapping: queued as Exit, nothing committed.
        let t = target(&seen);
        t.trap.store(true, Ordering::Release);
        deliver_signal(&t, from, ExitReason::Shutdown, Origin::Direct, false);
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
        deliver_signal(&t, from, ExitReason::Shutdown, Origin::Link, false);
        assert!(!pending(&t));
        deliver_signal(&t, from, ExitReason::Normal, Origin::Direct, true);
        assert!(pending(&t));
        assert_eq!(lock_request(&t.request).reason(), None);
        // Non-trapping: an untrapped signal begins exit; Normal is ignored.
        let t = target(&seen);
        deliver_signal(&t, from, ExitReason::Normal, Origin::Direct, true);
        assert!(!pending(&t));
        deliver_signal(&t, from, ExitReason::Shutdown, Origin::Direct, false);
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
        deliver_signal(&t, from, ExitReason::Shutdown, Origin::Direct, false);
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
    #[test]
    fn reason_precedence_until_death() {
        // A late non-kill signal replaces a requested reason (decision 8);
        // a kill supersedes it and nothing replaces Killed.
        let mut r = ExitRequest::default();
        assert!(r.request(ExitReason::Normal));
        assert!(r.untrapped(ExitReason::Shutdown));
        assert_eq!(r.reason(), Some(&ExitReason::Shutdown));
        assert!(
            r.begin_hook(),
            "a late signal does not skip a self-exit's stopped()"
        );
        r.panic(ExitReason::Panic("stopped".into()));
        assert_eq!(r.reason(), Some(&ExitReason::Panic("stopped".into())));
        assert!(r.kill());
        r.panic(ExitReason::Panic("after kill".into()));
        assert!(!r.untrapped(ExitReason::Shutdown));
        assert!(r.request(ExitReason::Normal));
        assert_eq!(r.die(), ExitReason::Killed);
        assert!(!r.kill() && !r.untrapped(ExitReason::Shutdown));
        // Runtime drop: Killed unless a non-Normal reason was requested.
        let mut r = ExitRequest::default();
        r.request(ExitReason::Normal);
        assert_eq!(r.runtime_drop(), ExitReason::Killed);
        let mut r = ExitRequest::default();
        r.request(ExitReason::Shutdown);
        assert_eq!(r.runtime_drop(), ExitReason::Shutdown);
    }
    #[test]
    fn link_from_an_exiting_or_dead_caller_is_inert() {
        let seen = Arc::new(AtomicUsize::new(0));
        // Exit begun: no record on either side, even to a live peer.
        let (a, at, b, bt) = pair(&seen);
        lock_request(&a.request).request(ExitReason::Normal);
        assert_eq!(
            register_link(&a, &at, &b, &bt, false),
            LinkOutcome::Unchanged
        );
        assert!(at.lock().unwrap().entries.is_empty() && bt.lock().unwrap().entries.is_empty());
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
