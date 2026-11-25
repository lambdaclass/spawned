//! The process-wide actor and system tables (`console` feature;
//! docs/design/introspection.md, "Actor and system tables").
//!
//! Every instance, bare or managed, is inserted once at spawn (when its start
//! commits) and removed once at publication, just before `wait_exit()`
//! becomes ready. Nothing here runs on send, dispatch or a turn, and D needs
//! no table write: a listed instance reads its own death point through
//! `ChildHandle::info`.
//!
//! 64 shards keyed by `ActorId & 63`, each an ordered map behind its own
//! `RwLock`. A reader takes one shard's read lock at a time and copies
//! handles out; it never holds a shard lock across `info()` (which takes the
//! actor's own metadata locks), so there is no lock order between the table
//! and an actor. A writer holds one shard's write lock for one map operation
//! and drops the removed handle after releasing it.

use crate::child_handle::{ActorId, ChildHandle};
use std::collections::BTreeMap;
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

const SHARDS: usize = 64;

struct Shard(RwLock<BTreeMap<u64, ChildHandle>>);

#[allow(clippy::declare_interior_mutable_const)] // only used to fill TABLE
const EMPTY: Shard = Shard(RwLock::new(BTreeMap::new()));
static TABLE: [Shard; SHARDS] = [EMPTY; SHARDS];

fn shard(id: u64) -> &'static Shard {
    &TABLE[(id % SHARDS as u64) as usize]
}

impl Shard {
    fn read(&self) -> RwLockReadGuard<'_, BTreeMap<u64, ChildHandle>> {
        self.0.read().unwrap_or_else(|p| p.into_inner())
    }
    fn write(&self) -> RwLockWriteGuard<'_, BTreeMap<u64, ChildHandle>> {
        self.0.write().unwrap_or_else(|p| p.into_inner())
    }
}

/// An instance's table entry: inserted by [`insert`], removed when dropped
/// (the actor's cleanup drops it at publication).
pub(crate) struct Listed(u64);

/// List `handle`'s instance until the returned entry is dropped.
pub(crate) fn insert(handle: &ChildHandle) -> Listed {
    let id = handle.id().raw();
    let old = shard(id).write().insert(id, handle.clone());
    debug_assert!(old.is_none(), "actor {id} listed twice");
    drop(old);
    Listed(id)
}

impl Drop for Listed {
    fn drop(&mut self) {
        // Removed under the lock, dropped after it.
        let removed = shard(self.0).write().remove(&self.0);
        drop(removed);
    }
}

/// The listed instance `id`, if any.
pub(crate) fn get(id: ActorId) -> Option<ChildHandle> {
    let id = id.raw();
    shard(id).read().get(&id).cloned()
}

/// Every listed id, sorted.
pub(crate) fn ids() -> Vec<ActorId> {
    let mut ids = Vec::new();
    for shard in &TABLE {
        ids.extend(shard.read().values().map(ChildHandle::id));
    }
    ids.sort_unstable();
    ids
}

/// The first `limit` listed instances with an id above `after` (keyset), in
/// id order, and whether more follow. Memory is bounded by `limit + 1`
/// handles: each shard contributes its own first `limit + 1` above `after`,
/// merged into a buffer cut back to `limit + 1` after every shard.
pub(crate) fn page(after: Option<ActorId>, limit: usize) -> (Vec<ChildHandle>, bool) {
    let start = after.map_or(0, |id| id.raw().saturating_add(1));
    let keep = limit.saturating_add(1);
    let mut found: Vec<(u64, ChildHandle)> = Vec::new();
    for shard in &TABLE {
        // A shard's entries above the current cut cannot make the page.
        let bound = (found.len() == keep).then(|| found[keep - 1].0);
        {
            let map = shard.read();
            found.extend(
                map.range(start..)
                    .take_while(|(id, _)| bound.is_none_or(|b| **id < b))
                    .take(keep)
                    .map(|(id, handle)| (*id, handle.clone())),
            );
        }
        found.sort_unstable_by_key(|(id, _)| *id);
        found.truncate(keep);
    }
    let more = found.len() > limit;
    found.truncate(limit);
    (found.into_iter().map(|(_, handle)| handle).collect(), more)
}

/// Every listed instance, in id order.
pub(crate) fn all() -> Vec<ChildHandle> {
    let mut found: Vec<ChildHandle> = Vec::new();
    for shard in &TABLE {
        found.extend(shard.read().values().cloned());
    }
    found.sort_unstable_by_key(ChildHandle::id);
    found
}

/// The live `ActorSystem`s, in `SystemId` order.
#[cfg(feature = "async")]
pub(crate) mod systems {
    use crate::introspection::SystemId;
    use crate::system::SystemInner;
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex, Weak};

    static SYSTEMS: Mutex<BTreeMap<SystemId, Weak<SystemInner>>> = Mutex::new(BTreeMap::new());

    fn lock() -> std::sync::MutexGuard<'static, BTreeMap<SystemId, Weak<SystemInner>>> {
        SYSTEMS.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// A system's entry, removed when the system's state is dropped.
    pub(crate) struct ListedSystem(SystemId);

    impl ListedSystem {
        pub(crate) fn new(id: SystemId) -> Self {
            Self(id)
        }
        /// Publish the system (once its state exists).
        pub(crate) fn publish(&self, system: &Arc<SystemInner>) {
            lock().insert(self.0, Arc::downgrade(system));
        }
    }

    impl Drop for ListedSystem {
        fn drop(&mut self) {
            let removed = lock().remove(&self.0);
            drop(removed);
        }
    }

    /// The systems still alive, in id order (upgraded outside the lock).
    pub(crate) fn live() -> Vec<Arc<SystemInner>> {
        let weak: Vec<Weak<SystemInner>> = lock().values().cloned().collect();
        weak.iter().filter_map(Weak::upgrade).collect()
    }
}
