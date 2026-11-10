use crate::child_handle::ActorId;
use crate::error::{ActorError, ExitReason};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

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

/// Per-actor flag controlling how exit signals from linked actors are handled.
/// `false` (default): the receiver is cancelled. `true`: the receiver gets an
/// `Exit` message via `Actor::exit_received`.
pub(crate) type TrapExitFlag = Arc<AtomicBool>;

/// Per-actor slot holding the exit reason of a linked actor whose death
/// triggered cancellation. When a non-trapping actor is cancelled by a link
/// signal, this slot is set so the actor's own exit reason propagates
/// transitively through further links.
pub(crate) type LinkedExitReason = Arc<Mutex<Option<ExitReason>>>;

/// Create a new empty linked-exit-reason slot.
pub(crate) fn new_linked_exit_reason() -> LinkedExitReason {
    Arc::new(Mutex::new(None))
}

/// An entry in an actor's link table, representing one linked peer.
pub(crate) struct LinkEntry {
    /// The peer's unique actor ID.
    pub peer_id: ActorId,
    /// Delivers an exit signal to the peer.
    pub signal: ExitSignalFn,
    /// Reference to the peer's link table, so we can remove ourselves on death.
    pub peer_links: Weak<Mutex<LinkState>>,
}

/// Type-erased function that signals exit to a peer. Decides whether to
/// cancel the peer or send an `Exit` message based on the peer's `trap_exit`
/// flag and the exit reason.
pub(crate) type ExitSignalFn = Arc<dyn Fn(ActorId, ExitReason) + Send + Sync>;

/// An actor's link table — a list of linked peers.
#[derive(Default)]
pub(crate) struct LinkState {
    entries: Vec<LinkEntry>,
    // Published under the same mutex as registration, before edges are drained.
    // Completion channels can be published later without losing a racing link.
    exited: Option<ExitReason>,
}
pub(crate) type LinkTable = Arc<Mutex<LinkState>>;

/// Create a new empty link table.
pub(crate) fn new_link_table() -> LinkTable {
    Arc::new(Mutex::new(LinkState::default()))
}

/// Create a new trap_exit flag (default: false).
pub(crate) fn new_trap_exit_flag() -> TrapExitFlag {
    Arc::new(AtomicBool::new(false))
}

/// Build an `ExitSignalFn` that signals a peer based on its trap_exit flag
/// and a typed-erased send-exit function.
pub(crate) fn make_signal(
    peer_trap_exit: TrapExitFlag,
    peer_cancel: Arc<dyn Fn() + Send + Sync>,
    peer_send_exit: SendExitFn,
    peer_linked_reason: LinkedExitReason,
) -> ExitSignalFn {
    Arc::new(move |sender_id, reason| {
        // A propagated notice is trappable, including Kill. Only a direct
        // stop_with_reason(Kill) bypasses the target's trap_exit flag.
        // Normal exit signals are silently dropped unless the peer is trapping
        let trapping = peer_trap_exit.load(Ordering::Acquire);
        if matches!(reason, ExitReason::Normal) && !trapping {
            return;
        }
        if trapping {
            // Send Exit message to the peer's mailbox
            let exit = Exit {
                from: sender_id,
                reason,
            };
            let _ = peer_send_exit(exit); // mailbox may be closed if peer just died
        } else {
            // Peer is not trapping: record the linked reason then cancel
            let mut slot = peer_linked_reason.lock().unwrap_or_else(|p| p.into_inner());
            if slot.is_none() {
                *slot = Some(reason);
            }
            drop(slot);
            peer_cancel();
        }
    })
}

/// Mutate both ends in one transaction. Callbacks and removed entries are
/// handled after releasing both locks. Pointer order is stable while Arcs live.
fn with_pair<R>(
    own: &LinkTable,
    peer: &LinkTable,
    f: impl FnOnce(&mut LinkState, &mut LinkState) -> R,
) -> R {
    if Arc::as_ptr(own) < Arc::as_ptr(peer) {
        let mut a = own.lock().unwrap_or_else(|p| p.into_inner());
        let mut b = peer.lock().unwrap_or_else(|p| p.into_inner());
        f(&mut a, &mut b)
    } else {
        let mut b = peer.lock().unwrap_or_else(|p| p.into_inner());
        let mut a = own.lock().unwrap_or_else(|p| p.into_inner());
        f(&mut a, &mut b)
    }
}

/// Register before actor execution starts, or atomically observe a closed peer.
/// Late operations through an already-closed own context are ignored.
pub(crate) fn register_link(
    own_id: ActorId,
    own: &LinkTable,
    own_signal: ExitSignalFn,
    peer_id: ActorId,
    peer: &LinkTable,
    peer_signal: ExitSignalFn,
) {
    if own_id == peer_id || Arc::ptr_eq(own, peer) {
        return;
    }

    let closed = with_pair(own, peer, |a, b| {
        if a.exited.is_some() || a.entries.iter().any(|e| e.peer_id == peer_id) {
            return None;
        }
        if let Some(reason) = &b.exited {
            return Some(reason.clone());
        }
        a.entries.push(LinkEntry {
            peer_id,
            signal: peer_signal.clone(),
            peer_links: Arc::downgrade(peer),
        });
        b.entries.push(LinkEntry {
            peer_id: own_id,
            signal: own_signal.clone(),
            peer_links: Arc::downgrade(own),
        });
        None
    });
    if let Some(reason) = closed {
        deliver(&own_signal, peer_id, reason);
    }
}

fn remove(table: &mut LinkState, id: ActorId) -> Option<LinkEntry> {
    table
        .entries
        .iter()
        .position(|e| e.peer_id == id)
        .map(|i| table.entries.swap_remove(i))
}

/// Unlink is atomic with registration. An exit already taken for delivery may
/// still arrive; unlink does not retract a signal already in flight.
pub(crate) fn unregister_link(
    own_id: ActorId,
    own: &LinkTable,
    peer_id: ActorId,
    peer: &LinkTable,
) {
    if own_id == peer_id || Arc::ptr_eq(own, peer) {
        return;
    }
    let removed = with_pair(own, peer, |a, b| (remove(a, peer_id), remove(b, own_id)));
    drop(removed);
}

fn deliver(signal: &ExitSignalFn, id: ActorId, reason: ExitReason) {
    // One failing internal cancellation callback must not prevent other peers
    // from being notified, or panic again during actor-future destruction.
    if let Err(payload) = catch_unwind(AssertUnwindSafe(|| signal(id, reason))) {
        crate::mailbox::dispose_panic(payload);
        if let Err(payload) = catch_unwind(AssertUnwindSafe(|| {
            tracing::error!("Panic while delivering a linked exit");
        })) {
            crate::mailbox::dispose_panic(payload);
        }
    }
}

/// Close the registration state before draining. Terminal closure is idempotent.
pub(crate) fn propagate_exit(own_id: ActorId, own: &LinkTable, reason: &ExitReason) {
    let entries = {
        let mut state = own.lock().unwrap_or_else(|p| p.into_inner());
        if state.exited.is_some() {
            return;
        }
        state.exited = Some(reason.clone());
        std::mem::take(&mut state.entries)
    };
    for entry in entries {
        let removed = entry.peer_links.upgrade().and_then(|peer| {
            let mut table = peer.lock().unwrap_or_else(|p| p.into_inner());
            remove(&mut table, own_id)
        });
        drop(removed);
        deliver(&entry.signal, own_id, reason.clone());
    }
}

/// Created before constructing the actor future/thread closure, so shutdown
/// before its first poll also closes links. Weak reverse edges avoid Arc cycles.
pub(crate) struct LinkCleanup {
    id: ActorId,
    links: LinkTable,
}
impl LinkCleanup {
    pub(crate) fn new(id: ActorId, links: LinkTable) -> Self {
        Self { id, links }
    }
    pub(crate) fn complete(&self, reason: &ExitReason) {
        propagate_exit(self.id, &self.links, reason);
    }
}
impl Drop for LinkCleanup {
    fn drop(&mut self) {
        self.complete(&ExitReason::Kill);
    }
}

/// A local stop commits its cause before cancelling. A linked failure recorded
/// first wins; a late linked failure cannot relabel an already-requested stop.
pub(crate) fn normal_stop(reason: &LinkedExitReason, cancel: impl FnOnce()) {
    stop_with_reason(reason, ExitReason::Normal, cancel);
}

pub(crate) fn stop_with_reason(
    reason: &LinkedExitReason,
    cause: ExitReason,
    cancel: impl FnOnce(),
) {
    let mut slot = reason.lock().unwrap_or_else(|p| p.into_inner());
    if slot.is_none() {
        *slot = Some(cause);
    }
    drop(slot);
    cancel();
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
        struct Payload(Arc<AtomicUsize>);
        impl Drop for Payload {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
                panic!("panic payload destructor");
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
        let signal: ExitSignalFn =
            Arc::new(move |_, _| std::panic::panic_any(Payload(callback_drops.clone())));
        let result = catch_unwind(AssertUnwindSafe(|| {
            tracing::subscriber::with_default(PanicSubscriber(drops.clone()), || {
                deliver(&signal, ActorId::next(), ExitReason::Kill);
            });
        }));
        assert!(result.is_ok(), "delivery unwound through its caller");
        assert_eq!(drops.load(Ordering::SeqCst), 2);
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
    fn signal(counter: &Arc<AtomicUsize>) -> ExitSignalFn {
        let c = counter.clone();
        Arc::new(move |_, _| {
            c.fetch_add(1, Ordering::SeqCst);
        })
    }
    #[test]
    fn closure_is_terminal_and_delivers_each_edge_once() {
        let a = new_link_table();
        let b = new_link_table();
        let aid = ActorId::next();
        let bid = ActorId::next();
        let seen = Arc::new(AtomicUsize::new(0));
        register_link(aid, &a, signal(&seen), bid, &b, signal(&seen));
        propagate_exit(aid, &a, &ExitReason::Kill);
        propagate_exit(aid, &a, &ExitReason::Kill);
        assert_eq!(seen.load(Ordering::SeqCst), 1);
        assert!(b.lock().unwrap().entries.is_empty());
        register_link(bid, &b, signal(&seen), aid, &a, signal(&seen));
        assert_eq!(seen.load(Ordering::SeqCst), 2);
        assert!(a.lock().unwrap().entries.is_empty() && b.lock().unwrap().entries.is_empty());
    }
    #[test]
    fn self_link_is_noop() {
        let a = new_link_table();
        let id = ActorId::next();
        let seen = Arc::new(AtomicUsize::new(0));
        register_link(id, &a, signal(&seen), id, &a, signal(&seen));
        assert!(a.lock().unwrap().entries.is_empty());
        assert_eq!(Arc::strong_count(&a), 1);
    }
    #[test]
    fn weak_edges_do_not_keep_link_tables_alive() {
        let a = new_link_table();
        let b = new_link_table();
        let weak_a = Arc::downgrade(&a);
        let weak_b = Arc::downgrade(&b);
        let c = Arc::new(AtomicUsize::new(0));
        register_link(
            ActorId::next(),
            &a,
            signal(&c),
            ActorId::next(),
            &b,
            signal(&c),
        );
        drop(a);
        drop(b);
        assert!(weak_a.upgrade().is_none() && weak_b.upgrade().is_none());
    }
    #[test]
    fn concurrent_link_unlink_preserves_symmetry() {
        let a = new_link_table();
        let b = new_link_table();
        let aid = ActorId::next();
        let bid = ActorId::next();
        let c = Arc::new(AtomicUsize::new(0));
        std::thread::scope(|scope| {
            scope.spawn(|| {
                for _ in 0..2000 {
                    register_link(aid, &a, signal(&c), bid, &b, signal(&c));
                    unregister_link(aid, &a, bid, &b);
                }
            });
            scope.spawn(|| {
                for _ in 0..2000 {
                    register_link(bid, &b, signal(&c), aid, &a, signal(&c));
                    unregister_link(bid, &b, aid, &a);
                }
            });
        });
        assert_eq!(
            a.lock().unwrap().entries.len(),
            b.lock().unwrap().entries.len()
        );
        assert!(a.lock().unwrap().entries.len() <= 1);
    }
    #[test]
    fn normal_stop_cause_is_not_replaced_by_late_failure() {
        let reason = new_linked_exit_reason();
        normal_stop(&reason, || {});
        let signal = make_signal(
            new_trap_exit_flag(),
            Arc::new(|| {}),
            Arc::new(|_| Ok(())),
            reason.clone(),
        );
        signal(ActorId::next(), ExitReason::Panic("late".into()));
        assert_eq!(*reason.lock().unwrap(), Some(ExitReason::Normal));
    }
}
