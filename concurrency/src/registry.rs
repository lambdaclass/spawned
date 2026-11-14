use std::any::Any;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{OnceLock, RwLock};

struct Entry {
    value: Box<dyn Any + Send + Sync>,
    owner: Option<(crate::ActorId, u64)>,
}
type Store = RwLock<HashMap<String, Entry>>;
static NEXT_REGISTRATION: AtomicU64 = AtomicU64::new(1);

fn global_store() -> &'static Store {
    static STORE: OnceLock<Store> = OnceLock::new();
    STORE.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Errors that can occur when registering a value in the registry.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RegistryError {
    /// A value with this name is already registered.
    #[error("name '{0}' is already registered")]
    AlreadyRegistered(String),
}

/// Errors when registering a name through a managed actor's lifecycle scope.
///
/// Kept separate from [`RegistryError`] so existing exhaustive matches on the
/// global registry's error remain source-compatible.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ManagedRegistrationError {
    #[error(transparent)]
    Registry(#[from] RegistryError),
    /// The actor is unmanaged or its owner is closing or already closed.
    #[error("registration requires a live managed actor scope")]
    OwnerClosed,
}

/// Register a value by name in the global registry.
///
/// Returns `Err(AlreadyRegistered)` if the name is already taken.
/// Use [`unregister`] first if you need to replace an existing entry.
pub fn register<T: Clone + Send + Sync + 'static>(
    name: &str,
    value: T,
) -> Result<(), RegistryError> {
    use std::collections::hash_map::Entry as MapEntry;
    let mut store = global_store().write().unwrap_or_else(|p| p.into_inner());
    match store.entry(name.to_string()) {
        MapEntry::Occupied(e) => Err(RegistryError::AlreadyRegistered(e.key().clone())),
        MapEntry::Vacant(e) => {
            e.insert(Entry {
                value: Box::new(value),
                owner: None,
            });
            Ok(())
        }
    }
}

/// Look up a value by name. Returns `None` if not found or if the stored
/// type doesn't match `T`.
pub fn whereis<T: Clone + Send + Sync + 'static>(name: &str) -> Option<T> {
    let store = global_store().read().unwrap_or_else(|p| p.into_inner());
    store.get(name)?.value.downcast_ref::<T>().cloned()
}

/// Remove a registration by name. No-op if the name is not registered.
pub fn unregister(name: &str) {
    let removed = global_store()
        .write()
        .unwrap_or_else(|p| p.into_inner())
        .remove(name);
    drop(removed);
}

/// Removes only this exact owned registration, including when a manual
/// unregister/re-register used the same actor identity in between.
pub struct RegistrationGuard {
    name: String,
    owner: (crate::ActorId, u64),
}
impl Drop for RegistrationGuard {
    fn drop(&mut self) {
        fail_point!("registry.before_unregister");
        let removed = {
            let mut store = global_store().write().unwrap_or_else(|p| p.into_inner());
            if store
                .get(&self.name)
                .is_some_and(|entry| entry.owner == Some(self.owner))
            {
                store.remove(&self.name)
            } else {
                None
            }
        };
        drop(removed);
    }
}

pub fn register_owned<T: Clone + Send + Sync + 'static>(
    name: &str,
    owner: crate::ActorId,
    value: T,
) -> Result<RegistrationGuard, RegistryError> {
    use std::collections::hash_map::Entry as MapEntry;
    let mut store = global_store().write().unwrap_or_else(|p| p.into_inner());
    match store.entry(name.to_string()) {
        MapEntry::Occupied(_) => Err(RegistryError::AlreadyRegistered(name.into())),
        MapEntry::Vacant(slot) => {
            let owner = (owner, NEXT_REGISTRATION.fetch_add(1, Ordering::Relaxed));
            slot.insert(Entry {
                value: Box::new(value),
                owner: Some(owner),
            });
            Ok(RegistrationGuard {
                name: name.into(),
                owner,
            })
        }
    }
}

/// List all registered names, sorted. (The store is a `HashMap`, whose
/// iteration order depends on per-process random state; sorting keeps
/// callers deterministic, which the simulation backend relies on.)
pub fn registered() -> Vec<String> {
    let store = global_store().read().unwrap_or_else(|p| p.into_inner());
    let mut names: Vec<String> = store.keys().cloned().collect();
    names.sort_unstable();
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registered_names_are_sorted() {
        let names: Vec<String> = (0..16)
            .map(|i| format!("sorted-probe-{:02}", 15 - i))
            .collect();
        for name in &names {
            register(name, 0u8).unwrap();
        }
        let listed: Vec<String> = registered()
            .into_iter()
            .filter(|n| n.starts_with("sorted-probe-"))
            .collect();
        let mut sorted = names.clone();
        sorted.sort();
        for name in &names {
            unregister(name);
        }
        assert_eq!(listed, sorted);
    }

    // Use unique names per test to avoid cross-test interference with global state.

    #[test]
    fn register_and_whereis() {
        register("test_rw_1", 42u64).unwrap();
        let val: Option<u64> = whereis("test_rw_1");
        assert_eq!(val, Some(42));
    }

    #[test]
    fn whereis_wrong_type_returns_none() {
        register("test_wt_1", 42u64).unwrap();
        let val: Option<String> = whereis("test_wt_1");
        assert_eq!(val, None);
    }

    #[test]
    fn whereis_missing_returns_none() {
        let val: Option<u64> = whereis("nonexistent_key");
        assert_eq!(val, None);
    }

    #[test]
    fn duplicate_register_fails() {
        register("test_dup_1", 1u32).unwrap();
        // Deliberately exhaustive: the v0.6 global-registry error must retain
        // its original shape when managed registration gains new failures.
        match register("test_dup_1", 2u32).unwrap_err() {
            RegistryError::AlreadyRegistered(name) => assert_eq!(name, "test_dup_1"),
        }
    }

    #[test]
    fn unregister_removes_entry() {
        register("test_unreg_1", "hello".to_string()).unwrap();
        unregister("test_unreg_1");
        let val: Option<String> = whereis("test_unreg_1");
        assert_eq!(val, None);
    }

    #[test]
    fn registered_lists_names() {
        register("test_list_a", 1u32).unwrap();
        register("test_list_b", 2u32).unwrap();
        let names = registered();
        assert!(names.contains(&"test_list_a".to_string()));
        assert!(names.contains(&"test_list_b".to_string()));
    }

    #[test]
    fn old_guard_preserves_new_registration_even_with_same_actor_identity() {
        let actor = crate::ActorId::next();
        let old = register_owned("owned_generation_guard_test", actor, 1u64).unwrap();
        unregister("owned_generation_guard_test");
        let new = register_owned("owned_generation_guard_test", actor, 2u64).unwrap();
        drop(old);
        assert_eq!(whereis::<u64>("owned_generation_guard_test"), Some(2));
        drop(new);
        assert_eq!(whereis::<u64>("owned_generation_guard_test"), None);
    }
}
