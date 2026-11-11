//! The observer-side monitor table protocol (docs/design/otp-exit-signals.md
//! E4, E6): registration linearised against the target's death point, the
//! `Down` enqueued in the same critical section that removes the entry, the
//! `demonitor` fence and the observer's own close.
//!
//! Shared with the Loom model (`tests/link_fence_loom.rs`), which supplies
//! the synchronization types through `super::sync`. The callbacks run under
//! the observer's table lock; production uses them only to read the target's
//! death point, record a trace event and enqueue a `Down` (which never takes
//! a monitor table lock).

use super::sync::{Mutex, MutexGuard};
use std::collections::HashMap;
use std::hash::Hash;

/// An observer's monitors: `None` once the observer's exit closed it.
pub(crate) type Entries<K, V> = Mutex<Option<HashMap<K, V>>>;

fn lock<K, V>(table: &Entries<K, V>) -> MutexGuard<'_, Option<HashMap<K, V>>> {
    table.lock().unwrap_or_else(|p| p.into_inner())
}

/// The outcome of a `monitor` call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Registration {
    /// The observer is exiting or closed: no entry, no notice.
    Inert,
    /// The target was past D: the `NoProc` notice was enqueued, no entry.
    Late,
    /// The entry is installed; its waiter delivers the `Down` at publication.
    Active,
}

/// `monitor`: under the observer's lock, inert once `observer_exiting` or the
/// table is closed; past the target's D (`target_dead`, which reads the
/// target's exit table) enqueue the late notice (`late`); otherwise install
/// the entry and run `active`.
pub(crate) fn register<K: Hash + Eq, V>(
    table: &Entries<K, V>,
    key: K,
    value: V,
    observer_exiting: impl FnOnce() -> bool,
    target_dead: impl FnOnce() -> bool,
    late: impl FnOnce(),
    active: impl FnOnce(),
) -> Registration {
    let mut guard = lock(table);
    if observer_exiting() {
        return Registration::Inert;
    }
    let Some(entries) = guard.as_mut() else {
        return Registration::Inert;
    };
    if target_dead() {
        late();
        return Registration::Late;
    }
    entries.insert(key, value);
    active();
    Registration::Active
}

/// The waiter, after the target's exit is published: remove the entry and
/// enqueue the `Down` (`deliver`) in one critical section, so once
/// [`remove`] (a `demonitor`) returns no `Down` of this monitor is enqueued.
/// Returns whether it was delivered.
pub(crate) fn deliver<K: Hash + Eq, V>(
    table: &Entries<K, V>,
    key: &K,
    deliver: impl FnOnce(),
) -> bool {
    let mut guard = lock(table);
    let removed = guard.as_mut().and_then(|entries| entries.remove(key));
    if removed.is_some() {
        deliver();
    }
    drop(guard);
    removed.is_some()
}

/// `demonitor` (and a waiter that is dropped unpolled): remove the entry
/// under the lock, running `removed` there when it was present. The caller
/// wakes the waiter with the returned value after the lock is released.
pub(crate) fn remove<K: Hash + Eq, V>(
    table: &Entries<K, V>,
    key: &K,
    removed: impl FnOnce(),
) -> Option<V> {
    let mut guard = lock(table);
    let value = guard.as_mut().and_then(|entries| entries.remove(key));
    if value.is_some() {
        removed();
    }
    value
}

/// The observer's exit: close the table and take every entry, whose waiters
/// the caller wakes after the lock is released.
pub(crate) fn close<K, V>(table: &Entries<K, V>) -> Option<HashMap<K, V>> {
    lock(table).take()
}
