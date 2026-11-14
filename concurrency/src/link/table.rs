//! The link table protocol (docs/design/otp-exit-signals.md E5, E6): edge
//! generations, both records installed in one two-lock transaction,
//! receiver-side fenced delivery and the close at the death point D that
//! takes the notices.
//!
//! Shared with the Loom model (`tests/link_fence_loom.rs`), which supplies
//! `ActorId`, `ExitReason`, `SignalTarget` and the synchronization types
//! through `super`. The protocol never runs user code under a table lock
//! except through the `apply` callback of [`take_record`], which production
//! uses to apply the signal to the receiver. That path takes the receiver's
//! exit-request lock, may enqueue an internal exit envelope (mailbox admission
//! and an executor wake), or cancel its private actor token. These internal
//! callbacks must never acquire a link-table lock or execute user handlers.
//! The outgoing SendGate fence is acquired before the table lock at death;
//! Registry::seal under the table lock only publishes the OnceLock reason.
//! Across these protocols the order is monitor table -> link tables (pointer
//! order) -> exit request -> mailbox admission. Internal wakes must not call
//! user handlers or reacquire a monitor/link table.

use super::sync::{Arc, Mutex, MutexGuard, Weak};
use super::{ActorId, ExitReason, SignalTarget};

/// An entry in an actor's link table, representing one linked peer.
pub(crate) struct LinkEntry {
    /// The peer's unique actor ID.
    pub peer_id: ActorId,
    /// Edge generation: both records of one link share it; a late record has
    /// its own. Delivery applies only to a record with this generation.
    pub gen: u64,
    /// The peer, to deliver this actor's death notice to it.
    pub peer: Arc<SignalTarget>,
    /// Reference to the peer's link table, for receiver-side delivery.
    pub peer_links: Weak<Mutex<LinkState>>,
    /// A pending late `NoProc` notice (the peer was past D at `link`).
    pub late: bool,
}

/// An actor's exit table: its link records and its death point.
#[derive(Default)]
pub(crate) struct LinkState {
    pub(super) entries: Vec<LinkEntry>,
    // Set at the death point D under the same mutex as registration.
    pub(super) exited: Option<ExitReason>,
}
pub(crate) type LinkTable = Arc<Mutex<LinkState>>;

impl LinkState {
    /// Past the death point D.
    pub(crate) fn is_dead(&self) -> bool {
        self.exited.is_some()
    }
}

/// Create a new empty link table.
pub(crate) fn new_link_table() -> LinkTable {
    Arc::new(Mutex::new(LinkState::default()))
}

pub(super) fn lock(table: &Mutex<LinkState>) -> MutexGuard<'_, LinkState> {
    table.lock().unwrap_or_else(|p| p.into_inner())
}

/// Mutate both ends in one transaction. Callbacks and removed entries are
/// handled after releasing both locks. Pointer order is stable while Arcs live.
fn with_pair<R>(
    own: &LinkTable,
    peer: &LinkTable,
    f: impl FnOnce(&mut LinkState, &mut LinkState) -> R,
) -> R {
    if Arc::as_ptr(own) < Arc::as_ptr(peer) {
        let mut a = lock(own);
        let mut b = lock(peer);
        f(&mut a, &mut b)
    } else {
        let mut b = lock(peer);
        let mut a = lock(own);
        f(&mut a, &mut b)
    }
}

/// The outcome of a link attempt.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LinkOutcome {
    Linked,
    /// Nothing to do: self link, an existing record, or the caller's exit
    /// began (inert, as `monitor`).
    Unchanged,
    /// The peer was past D.
    NoProc,
}

/// Install the records of edge `gen` between `own` and `peer`. An existing
/// record (live or pending) or a closed own table makes it a no-op. Past the
/// peer's D, `try_only` installs nothing; otherwise a pending late record is
/// installed on `own`, whose `NoProc` notice the caller delivers through
/// [`take_record`] (so a concurrent `unlink` still fences it).
pub(crate) fn install(
    own: &Arc<SignalTarget>,
    own_links: &LinkTable,
    peer: &Arc<SignalTarget>,
    peer_links: &LinkTable,
    gen: u64,
    try_only: bool,
) -> LinkOutcome {
    with_pair(own_links, peer_links, |a, b| {
        if a.exited.is_some() || a.entries.iter().any(|e| e.peer_id == peer.id) {
            return LinkOutcome::Unchanged;
        }
        if b.exited.is_some() {
            if !try_only {
                a.entries.push(LinkEntry {
                    peer_id: peer.id,
                    gen,
                    peer: peer.clone(),
                    peer_links: Arc::downgrade(peer_links),
                    late: true,
                });
            }
            return LinkOutcome::NoProc;
        }
        a.entries.push(LinkEntry {
            peer_id: peer.id,
            gen,
            peer: peer.clone(),
            peer_links: Arc::downgrade(peer_links),
            late: false,
        });
        b.entries.push(LinkEntry {
            peer_id: own.id,
            gen,
            peer: own.clone(),
            peer_links: Arc::downgrade(own_links),
            late: false,
        });
        LinkOutcome::Linked
    })
}

fn remove(table: &mut LinkState, id: ActorId) -> Option<LinkEntry> {
    table
        .entries
        .iter()
        .position(|e| e.peer_id == id)
        .map(|i| table.entries.swap_remove(i))
}

/// Remove both records of the edge, live or pending, in one transaction:
/// once this returns no notice of that edge takes effect (E6). The caller
/// drops the removed records after every lock is released.
pub(crate) fn remove_pair(
    own_id: ActorId,
    own: &LinkTable,
    peer_id: ActorId,
    peer: &LinkTable,
) -> (Option<LinkEntry>, Option<LinkEntry>) {
    with_pair(own, peer, |a, b| (remove(a, peer_id), remove(b, own_id)))
}

/// Receiver-side delivery: under the receiver's table lock, apply the notice
/// only if the record with that peer and generation is still present, and
/// remove it. `unlink` and the receiver's own close at D remove records
/// under the same lock (the fence). Returns the removed record, which the
/// caller drops after the lock is released.
pub(crate) fn take_record(
    receiver_links: &LinkTable,
    from: ActorId,
    gen: u64,
    apply: impl FnOnce(&LinkEntry),
) -> Option<LinkEntry> {
    let mut table = lock(receiver_links);
    let i = table
        .entries
        .iter()
        .position(|e| e.peer_id == from && e.gen == gen)?;
    let record = table.entries.swap_remove(i);
    apply(&record);
    drop(table);
    Some(record)
}

/// Death notices taken at D, delivered at publication.
#[derive(Default)]
pub(crate) struct LinkNotices {
    from: Option<ActorId>,
    reason: Option<ExitReason>,
    entries: Vec<LinkEntry>,
}

impl LinkNotices {
    /// Deliver every notice (publication) through `notice(receiver table,
    /// receiver, from, gen, reason)`, which applies it only if the receiver
    /// still holds the record (E6 fence). Late records carry no notice of
    /// this actor's death: their `NoProc` was this actor's own.
    pub(crate) fn deliver_with(
        self,
        mut notice: impl FnMut(&LinkTable, &SignalTarget, ActorId, u64, ExitReason),
    ) {
        let (Some(from), Some(reason)) = (self.from, self.reason) else {
            return;
        };
        for entry in self.entries {
            if entry.late {
                continue;
            }
            if let Some(peer_links) = entry.peer_links.upgrade() {
                notice(&peer_links, &entry.peer, from, entry.gen, reason.clone());
            }
        }
    }
}

/// The exit-table part of the death point D: set the final reason and take
/// one notice per link, taking only this actor's own lock. `at_death` runs
/// inside the same critical section (it must not run user code). Terminal
/// closure is idempotent: a second close takes nothing.
pub(crate) fn close(
    own_id: ActorId,
    own: &LinkTable,
    reason: &ExitReason,
    at_death: impl FnOnce(&ExitReason),
) -> LinkNotices {
    let mut state = lock(own);
    if state.exited.is_some() {
        return LinkNotices::default();
    }
    at_death(reason);
    state.exited = Some(reason.clone());
    LinkNotices {
        from: Some(own_id),
        reason: Some(reason.clone()),
        entries: std::mem::take(&mut state.entries),
    }
}
