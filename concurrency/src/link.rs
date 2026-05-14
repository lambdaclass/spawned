use crate::child_handle::ActorId;
use crate::error::{ActorError, ExitReason};
use std::sync::atomic::{AtomicBool, Ordering};
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
/// [`Context::trap_exit(true)`]: crate::tasks::actor::Context::trap_exit
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
    pub peer_links: LinkTable,
}

/// Type-erased function that signals exit to a peer. Decides whether to
/// cancel the peer or send an `Exit` message based on the peer's `trap_exit`
/// flag and the exit reason.
pub(crate) type ExitSignalFn = Arc<dyn Fn(ActorId, ExitReason) + Send + Sync>;

/// An actor's link table — a list of linked peers.
pub(crate) type LinkTable = Arc<Mutex<Vec<LinkEntry>>>;

/// Create a new empty link table.
pub(crate) fn new_link_table() -> LinkTable {
    Arc::new(Mutex::new(Vec::new()))
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
        // Kill is untrappable — always cancel (with Kill as the linked reason)
        if matches!(reason, ExitReason::Kill) {
            let mut slot = peer_linked_reason.lock().unwrap_or_else(|p| p.into_inner());
            if slot.is_none() {
                *slot = Some(ExitReason::Kill);
            }
            drop(slot);
            peer_cancel();
            return;
        }
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

/// Register a bidirectional link between two actors.
///
/// If a link already exists between this pair (by `ActorId`), this is a no-op.
pub(crate) fn register_link(
    own_id: ActorId,
    own_links: &LinkTable,
    own_signal: ExitSignalFn,
    peer_id: ActorId,
    peer_links: &LinkTable,
    peer_signal: ExitSignalFn,
) {
    {
        let mut table = own_links.lock().unwrap_or_else(|p| p.into_inner());
        if !table.iter().any(|e| e.peer_id == peer_id) {
            table.push(LinkEntry {
                peer_id,
                signal: peer_signal,
                peer_links: peer_links.clone(),
            });
        }
    }
    {
        let mut table = peer_links.lock().unwrap_or_else(|p| p.into_inner());
        if !table.iter().any(|e| e.peer_id == own_id) {
            table.push(LinkEntry {
                peer_id: own_id,
                signal: own_signal,
                peer_links: own_links.clone(),
            });
        }
    }
}

/// Remove a bidirectional link between two actors.
pub(crate) fn unregister_link(
    own_id: ActorId,
    own_links: &LinkTable,
    peer_id: ActorId,
    peer_links: &LinkTable,
) {
    {
        let mut table = own_links.lock().unwrap_or_else(|p| p.into_inner());
        table.retain(|e| e.peer_id != peer_id);
    }
    {
        let mut table = peer_links.lock().unwrap_or_else(|p| p.into_inner());
        table.retain(|e| e.peer_id != own_id);
    }
}

/// Propagate exit signals to all linked actors when an actor dies.
/// Drains the link table, signals each peer, and removes self from each
/// peer's table.
pub(crate) fn propagate_exit(own_id: ActorId, own_links: &LinkTable, reason: &ExitReason) {
    let entries: Vec<LinkEntry> = {
        let mut table = own_links.lock().unwrap_or_else(|p| p.into_inner());
        std::mem::take(&mut *table)
    };

    for entry in &entries {
        // Remove ourselves from the peer's link table so they don't try to
        // signal us back (we're dead).
        let mut peer_table = entry.peer_links.lock().unwrap_or_else(|p| p.into_inner());
        peer_table.retain(|e| e.peer_id != own_id);
        drop(peer_table);
        // Deliver the exit signal to the peer.
        (entry.signal)(own_id, reason.clone());
    }
}

/// Atomically remove `own_id` from `peer_links`. Returns `true` if an entry
/// was actually removed. Used by `ctx.link()` to detect whether the peer's
/// `propagate_exit` has already drained the table.
pub(crate) fn take_self_from_peer_table(own_id: ActorId, peer_links: &LinkTable) -> bool {
    let mut table = peer_links.lock().unwrap_or_else(|p| p.into_inner());
    let len_before = table.len();
    table.retain(|e| e.peer_id != own_id);
    len_before != table.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_is_clone_and_eq() {
        let e1 = Exit {
            from: ActorId::next(),
            reason: ExitReason::Normal,
        };
        let e2 = e1.clone();
        assert_eq!(e1, e2);
    }

    #[test]
    fn empty_table_is_default() {
        let table = new_link_table();
        assert!(table.lock().unwrap().is_empty());
    }

    #[test]
    fn trap_exit_flag_defaults_to_false() {
        let flag = new_trap_exit_flag();
        assert!(!flag.load(Ordering::Acquire));
    }

    #[test]
    fn take_self_from_peer_table_returns_true_when_present() {
        let peer_links = new_link_table();
        let own_id = ActorId::next();
        // Insert a fake entry for own_id
        let dummy_signal: ExitSignalFn = Arc::new(|_, _| {});
        peer_links.lock().unwrap().push(LinkEntry {
            peer_id: own_id,
            signal: dummy_signal,
            peer_links: new_link_table(),
        });
        // First call: present, returns true and removes
        assert!(take_self_from_peer_table(own_id, &peer_links));
        // Second call: gone, returns false
        assert!(!take_self_from_peer_table(own_id, &peer_links));
        assert!(peer_links.lock().unwrap().is_empty());
    }

    #[test]
    fn take_self_from_peer_table_returns_false_when_absent() {
        let peer_links = new_link_table();
        let own_id = ActorId::next();
        // Empty table — no entry for own_id
        assert!(!take_self_from_peer_table(own_id, &peer_links));
    }
}
