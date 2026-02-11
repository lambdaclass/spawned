use std::any::Any;
use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

type Store = RwLock<HashMap<String, Box<dyn Any + Send + Sync>>>;

fn global_store() -> &'static Store {
    static STORE: OnceLock<Store> = OnceLock::new();
    STORE.get_or_init(|| RwLock::new(HashMap::new()))
}

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("name '{0}' is already registered")]
    AlreadyRegistered(String),
}

pub fn register<T: Send + Sync + 'static>(name: &str, value: T) -> Result<(), RegistryError> {
    let mut store = global_store().write().unwrap_or_else(|p| p.into_inner());
    if store.contains_key(name) {
        return Err(RegistryError::AlreadyRegistered(name.to_string()));
    }
    store.insert(name.to_string(), Box::new(value));
    Ok(())
}

pub fn whereis<T: Clone + Send + Sync + 'static>(name: &str) -> Option<T> {
    let store = global_store().read().unwrap_or_else(|p| p.into_inner());
    store.get(name)?.downcast_ref::<T>().cloned()
}

pub fn unregister(name: &str) {
    let mut store = global_store().write().unwrap_or_else(|p| p.into_inner());
    store.remove(name);
}

pub fn registered() -> Vec<String> {
    let store = global_store().read().unwrap_or_else(|p| p.into_inner());
    store.keys().cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let result = register("test_dup_1", 2u32);
        assert!(result.is_err());
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
}
