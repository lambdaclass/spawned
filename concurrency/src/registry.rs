//! Process registry for name-based process lookup.
//!
//! This module provides a global registry where processes can register themselves
//! with a unique name and be looked up by other processes.
//!
//! # Example
//!
//! ```ignore
//! use spawned_concurrency::registry;
//!
//! // Register a process
//! let handle = MyServer::new().start();
//! registry::register("my_server", handle.pid())?;
//!
//! // Look up by name
//! if let Some(pid) = registry::whereis("my_server") {
//!     println!("Found server with pid: {}", pid);
//! }
//!
//! // Unregister
//! registry::unregister("my_server");
//! ```

use crate::pid::Pid;
use std::collections::HashMap;
use std::sync::RwLock;

/// Global registry instance.
static REGISTRY: std::sync::LazyLock<RwLock<RegistryInner>> =
    std::sync::LazyLock::new(|| RwLock::new(RegistryInner::new()));

/// Internal registry state.
struct RegistryInner {
    /// Name -> Pid mapping.
    by_name: HashMap<String, Pid>,
    /// Pid -> Name mapping (for reverse lookup and cleanup).
    by_pid: HashMap<Pid, String>,
}

impl RegistryInner {
    fn new() -> Self {
        Self {
            by_name: HashMap::new(),
            by_pid: HashMap::new(),
        }
    }
}

/// Error type for registry operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryError {
    /// The name is already registered to another process.
    AlreadyRegistered,
    /// The process is already registered with another name.
    ProcessAlreadyNamed,
    /// The name was not found in the registry.
    NotFound,
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RegistryError::AlreadyRegistered => write!(f, "name is already registered"),
            RegistryError::ProcessAlreadyNamed => {
                write!(f, "process is already registered with another name")
            }
            RegistryError::NotFound => write!(f, "name not found in registry"),
        }
    }
}

impl std::error::Error for RegistryError {}

/// Register a process with a unique name.
///
/// # Arguments
///
/// * `name` - The name to register. Must be unique in the registry.
/// * `pid` - The process ID to associate with the name.
///
/// # Returns
///
/// * `Ok(())` if registration was successful.
/// * `Err(RegistryError::AlreadyRegistered)` if the name is already taken.
/// * `Err(RegistryError::ProcessAlreadyNamed)` if the process already has a name.
///
/// # Example
///
/// ```ignore
/// let handle = MyServer::new().start();
/// registry::register("my_server", handle.pid())?;
/// ```
pub fn register(name: impl Into<String>, pid: Pid) -> Result<(), RegistryError> {
    let name = name.into();
    let mut registry = REGISTRY.write().unwrap();

    // Check if name is already taken
    if registry.by_name.contains_key(&name) {
        return Err(RegistryError::AlreadyRegistered);
    }

    // Check if process already has a name
    if registry.by_pid.contains_key(&pid) {
        return Err(RegistryError::ProcessAlreadyNamed);
    }

    // Register
    registry.by_name.insert(name.clone(), pid);
    registry.by_pid.insert(pid, name);

    Ok(())
}

/// Unregister a name from the registry.
///
/// This removes the name and its associated process from the registry.
/// If the name doesn't exist, this is a no-op.
pub fn unregister(name: &str) {
    let mut registry = REGISTRY.write().unwrap();
    if let Some(pid) = registry.by_name.remove(name) {
        registry.by_pid.remove(&pid);
    }
}

/// Unregister a process by its Pid.
///
/// This removes the process and its associated name from the registry.
/// If the process isn't registered, this is a no-op.
pub fn unregister_pid(pid: Pid) {
    let mut registry = REGISTRY.write().unwrap();
    if let Some(name) = registry.by_pid.remove(&pid) {
        registry.by_name.remove(&name);
    }
}

/// Look up a process by name.
///
/// # Returns
///
/// * `Some(pid)` if the name is registered.
/// * `None` if the name is not found.
///
/// # Example
///
/// ```ignore
/// if let Some(pid) = registry::whereis("my_server") {
///     println!("Found: {}", pid);
/// }
/// ```
pub fn whereis(name: &str) -> Option<Pid> {
    let registry = REGISTRY.read().unwrap();
    registry.by_name.get(name).copied()
}

/// Get the registered name of a process.
///
/// # Returns
///
/// * `Some(name)` if the process is registered.
/// * `None` if the process is not registered.
pub fn name_of(pid: Pid) -> Option<String> {
    let registry = REGISTRY.read().unwrap();
    registry.by_pid.get(&pid).cloned()
}

/// Check if a name is registered.
pub fn is_registered(name: &str) -> bool {
    let registry = REGISTRY.read().unwrap();
    registry.by_name.contains_key(name)
}

/// Get a list of all registered names.
pub fn registered() -> Vec<String> {
    let registry = REGISTRY.read().unwrap();
    registry.by_name.keys().cloned().collect()
}

/// Get the number of registered processes.
pub fn count() -> usize {
    let registry = REGISTRY.read().unwrap();
    registry.by_name.len()
}

/// Clear all registrations.
///
/// This is mainly useful for testing.
pub fn clear() {
    let mut registry = REGISTRY.write().unwrap();
    registry.by_name.clear();
    registry.by_pid.clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    // Helper to ensure test isolation
    fn with_clean_registry<F, R>(f: F) -> R
    where
        F: FnOnce() -> R,
    {
        clear();
        let result = f();
        clear();
        result
    }

    #[test]
    fn test_register_and_whereis() {
        with_clean_registry(|| {
            let pid = Pid::new();
            let name = format!("test_server_{}", pid.id());
            assert!(register(&name, pid).is_ok());
            assert_eq!(whereis(&name), Some(pid));
        });
    }

    #[test]
    fn test_register_duplicate_name() {
        with_clean_registry(|| {
            let pid1 = Pid::new();
            let pid2 = Pid::new();
            let name = format!("test_server_{}", pid1.id());

            assert!(register(&name, pid1).is_ok());
            assert_eq!(
                register(&name, pid2),
                Err(RegistryError::AlreadyRegistered)
            );
        });
    }

    #[test]
    fn test_register_process_twice() {
        with_clean_registry(|| {
            let pid = Pid::new();
            let name1 = format!("server1_{}", pid.id());
            let name2 = format!("server2_{}", pid.id());

            assert!(register(&name1, pid).is_ok());
            assert_eq!(
                register(&name2, pid),
                Err(RegistryError::ProcessAlreadyNamed)
            );
        });
    }

    #[test]
    fn test_unregister() {
        with_clean_registry(|| {
            let pid = Pid::new();
            let name = format!("test_server_{}", pid.id());
            register(&name, pid).unwrap();

            unregister(&name);
            assert_eq!(whereis(&name), None);
            assert_eq!(name_of(pid), None);
        });
    }

    #[test]
    fn test_unregister_pid() {
        with_clean_registry(|| {
            let pid = Pid::new();
            let name = format!("test_server_{}", pid.id());
            register(&name, pid).unwrap();

            unregister_pid(pid);
            assert_eq!(whereis(&name), None);
            assert_eq!(name_of(pid), None);
        });
    }

    #[test]
    fn test_unregister_nonexistent() {
        with_clean_registry(|| {
            // Should not panic
            unregister("nonexistent");
            unregister_pid(Pid::new());
        });
    }

    #[test]
    fn test_name_of() {
        with_clean_registry(|| {
            let pid = Pid::new();
            let name = format!("my_server_{}", pid.id());
            register(&name, pid).unwrap();

            assert_eq!(name_of(pid), Some(name));
        });
    }

    #[test]
    fn test_is_registered() {
        with_clean_registry(|| {
            let pid = Pid::new();
            let name = format!("test_{}", pid.id());

            assert!(!is_registered(&name));
            register(&name, pid).unwrap();
            assert!(is_registered(&name));
        });
    }

    #[test]
    fn test_registered_list() {
        with_clean_registry(|| {
            let pid1 = Pid::new();
            let pid2 = Pid::new();

            // Use unique names to avoid conflicts with parallel tests
            let name1 = format!("server_list_{}", pid1.id());
            let name2 = format!("server_list_{}", pid2.id());

            register(&name1, pid1).unwrap();
            register(&name2, pid2).unwrap();

            let names = registered();
            // Check our names are in the list (there might be others from parallel tests)
            assert!(names.contains(&name1));
            assert!(names.contains(&name2));
        });
    }

    #[test]
    fn test_count() {
        // Note: Due to parallel test execution, we can't rely on absolute counts.
        // Instead, we verify that registration increases count and unregistration decreases it.
        let pid1 = Pid::new();
        let pid2 = Pid::new();

        // Use unique names to avoid conflicts with parallel tests
        let name1 = format!("count_test_{}", pid1.id());
        let name2 = format!("count_test_{}", pid2.id());

        let count_before = count();
        register(&name1, pid1).unwrap();
        let count_after_first = count();
        assert!(
            count_after_first > count_before,
            "Count should increase after registration"
        );

        register(&name2, pid2).unwrap();
        let count_after_second = count();
        assert!(
            count_after_second > count_after_first,
            "Count should increase after second registration"
        );

        // Clean up our registrations
        unregister(&name1);
        unregister(&name2);
    }

    #[test]
    fn test_reregister_after_unregister() {
        with_clean_registry(|| {
            let pid1 = Pid::new();
            let pid2 = Pid::new();
            let name = format!("server_{}", pid1.id());

            register(&name, pid1).unwrap();
            unregister(&name);

            // Should be able to register the same name with a different pid
            assert!(register(&name, pid2).is_ok());
            assert_eq!(whereis(&name), Some(pid2));
        });
    }
}
