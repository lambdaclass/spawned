//! The observer-side monitor table protocol (docs/design/otp-exit-signals.md
//! E4, E6): registration linearised against the target's death point, the
//! `Down` enqueued in the same critical section that marks the entry
//! queued, the `demonitor` fence and flush (OTP `demonitor(Ref, [flush])`)
//! and the observer's own close.
//!
//! Shared with the Loom model (`tests/link_fence_loom.rs`), which supplies
//! the synchronization types through `super::sync`. The callbacks run under
//! the observer's table lock; production uses them only to read the target's
//! death point, record a trace event and enqueue a `Down` (which never takes
//! a monitor table lock).

use super::sync::{Mutex, MutexGuard};
use std::collections::HashMap;
use std::hash::Hash;

/// One monitor in its observer's table.
#[derive(Debug)]
pub(crate) enum Entry<V> {
    /// Registered: its waiter delivers the `Down` at the target's
    /// publication.
    Active(V),
    /// Its `Down` is in the observer's mailbox, not yet handled: dispatch
    /// takes it ([`take_queued`]); `demonitor` flushes it.
    Queued,
}

/// An observer's monitors: `None` once the observer's exit closed it.
pub(crate) type Entries<K, V> = Mutex<Option<HashMap<K, Entry<V>>>>;

fn lock<K, V>(table: &Entries<K, V>) -> MutexGuard<'_, Option<HashMap<K, Entry<V>>>> {
    table.lock().unwrap_or_else(|p| p.into_inner())
}

/// An open, empty table.
pub(crate) fn open<K, V>() -> Entries<K, V> {
    Mutex::new(Some(HashMap::new()))
}

/// The outcome of a `monitor` call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Registration {
    /// The observer is exiting or closed: no entry, no notice.
    Inert,
    /// The target was past D: the `NoProc` notice was enqueued (its entry is
    /// `Queued`).
    Late,
    /// The entry is installed; its waiter delivers the `Down` at publication.
    Active,
}

/// `monitor`: under the observer's lock, inert once `observer_exiting` or the
/// table is closed; past the target's D (`target_dead`, which reads the
/// target's exit table) enqueue the late notice (`late`) and mark it queued;
/// otherwise install the entry and run `active`.
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
        entries.insert(key, Entry::Queued);
        return Registration::Late;
    }
    entries.insert(key, Entry::Active(value));
    active();
    Registration::Active
}

/// The waiter, after the target's exit is published: mark the entry queued
/// and enqueue the `Down` (`deliver`) in one critical section, so once
/// [`demonitor`] returns no `Down` of this monitor is enqueued, and one
/// already enqueued is flushed. Returns whether it was delivered.
pub(crate) fn deliver<K: Hash + Eq, V>(
    table: &Entries<K, V>,
    key: &K,
    deliver: impl FnOnce(),
) -> bool {
    let mut guard = lock(table);
    let entry = guard.as_mut().and_then(|entries| entries.get_mut(key));
    let active = match entry {
        Some(entry @ Entry::Active(_)) => {
            *entry = Entry::Queued;
            true
        }
        _ => false,
    };
    if active {
        deliver();
    }
    drop(guard);
    active
}

/// Dispatch of a framework `Down`: whether it is still owed (its entry is
/// `Queued`; taken now). `false` once `demonitor` flushed it.
pub(crate) fn take_queued<K: Hash + Eq, V>(table: &Entries<K, V>, key: &K) -> bool {
    let mut guard = lock(table);
    let Some(entries) = guard.as_mut() else {
        return false;
    };
    if matches!(entries.get(key), Some(Entry::Queued)) {
        entries.remove(key);
        true
    } else {
        false
    }
}

/// A waiter that is dropped (unpolled, rejected or finished): remove an
/// `Active` entry under the lock, running `removed` there when it was
/// present. A `Queued` entry stays for its dispatch. The caller wakes the
/// waiter with the returned value after the lock is released.
pub(crate) fn remove<K: Hash + Eq, V>(
    table: &Entries<K, V>,
    key: &K,
    removed: impl FnOnce(),
) -> Option<V> {
    let mut guard = lock(table);
    let entries = guard.as_mut()?;
    if !matches!(entries.get(key), Some(Entry::Active(_))) {
        return None;
    }
    removed();
    match entries.remove(key) {
        Some(Entry::Active(value)) => Some(value),
        _ => None,
    }
}

/// `demonitor` with OTP's `flush`: under the lock, remove the entry
/// whatever its state, running `removed` there when it was present. An
/// `Active` entry's waiter (returned) is woken by the caller after the lock
/// is released; a `Queued` one's `Down`, already in the mailbox, will be
/// skipped at dispatch. Once this returns, no `Down` of the monitor is
/// enqueued or handled.
pub(crate) fn demonitor<K: Hash + Eq, V>(
    table: &Entries<K, V>,
    key: &K,
    removed: impl FnOnce(),
) -> Option<V> {
    let mut guard = lock(table);
    let entry = guard.as_mut().and_then(|entries| entries.remove(key))?;
    removed();
    match entry {
        Entry::Active(value) => Some(value),
        Entry::Queued => None,
    }
}

/// The observer's exit: close the table and take every active entry, whose
/// waiters the caller wakes after the lock is released.
pub(crate) fn close<K, V>(table: &Entries<K, V>) -> Option<Vec<(K, V)>> {
    lock(table).take().map(|entries| {
        entries
            .into_iter()
            .filter_map(|(key, entry)| match entry {
                Entry::Active(value) => Some((key, value)),
                Entry::Queued => None,
            })
            .collect()
    })
}
