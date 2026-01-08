//! Process registry for name-based process lookup.
//!
//! This module provides a global registry where processes can register themselves
//! with a unique name and be looked up by other processes.
//!
//! # Example
//!
//! ```ignore
//! use spawned_concurrency::registry;
//! use spawned_concurrency::{GenServer, Backend};
//!
//! // Register a GenServer handle by name (stores both Pid and typed handle)
//! let handle = MyServer::new().start(Backend::Async);
//! registry::register_handle("my_server", handle.clone())?;
//!
//! // Look up typed handle for messaging
//! if let Some(h) = registry::lookup::<MyServer>("my_server") {
//!     h.call(MyMessage::Ping).await;
//! }
//!
//! // Look up Pid only (for linking/monitoring)
//! if let Some(pid) = registry::whereis("my_server") {
//!     println!("Found server with pid: {}", pid);
//! }
//!
//! // Unregister
//! registry::unregister("my_server");
//! ```

use crate::pid::{HasPid, Pid};
use crate::{GenServer, GenServerHandle};
use std::any::Any;
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
    /// Name -> Typed handle storage (type-erased).
    handles: HashMap<String, Box<dyn Any + Send + Sync>>,
}

impl RegistryInner {
    fn new() -> Self {
        Self {
            by_name: HashMap::new(),
            by_pid: HashMap::new(),
            handles: HashMap::new(),
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
    /// Type mismatch when looking up a handle.
    TypeMismatch,
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RegistryError::AlreadyRegistered => write!(f, "name is already registered"),
            RegistryError::ProcessAlreadyNamed => {
                write!(f, "process is already registered with another name")
            }
            RegistryError::NotFound => write!(f, "name not found in registry"),
            RegistryError::TypeMismatch => write!(f, "registered handle has different type"),
        }
    }
}

impl std::error::Error for RegistryError {}

/// Register a process with a unique name (Pid only).
///
/// This is the basic registration that only stores the Pid.
/// For typed handle lookup, use [`register_handle`] instead.
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
/// let handle = MyServer::new().start(Backend::Async);
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

/// Register a GenServer handle with a unique name.
///
/// This stores both the Pid (for linking/monitoring) and the typed handle
/// (for name-based messaging via [`lookup`]).
///
/// # Arguments
///
/// * `name` - The name to register. Must be unique in the registry.
/// * `handle` - The GenServer handle to register.
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
/// let handle = MyServer::new().start(Backend::Async);
/// registry::register_handle("my_server", handle)?;
///
/// // Later, look up and use the handle
/// let h = registry::lookup::<MyServer>("my_server").unwrap();
/// h.call(MyMessage::Ping).await;
/// ```
pub fn register_handle<G: GenServer + 'static>(
    name: impl Into<String>,
    handle: GenServerHandle<G>,
) -> Result<(), RegistryError> {
    let name = name.into();
    let pid = handle.pid();
    let mut registry = REGISTRY.write().unwrap();

    // Check if name is already taken
    if registry.by_name.contains_key(&name) {
        return Err(RegistryError::AlreadyRegistered);
    }

    // Check if process already has a name
    if registry.by_pid.contains_key(&pid) {
        return Err(RegistryError::ProcessAlreadyNamed);
    }

    // Register Pid mappings
    registry.by_name.insert(name.clone(), pid);
    registry.by_pid.insert(pid, name.clone());

    // Store typed handle
    registry.handles.insert(name, Box::new(handle));

    Ok(())
}

/// Look up a typed GenServer handle by name.
///
/// This allows you to retrieve a handle for messaging without needing
/// to pass handles around explicitly.
///
/// # Type Parameters
///
/// * `G` - The GenServer type. Must match the type used when registering.
///
/// # Returns
///
/// * `Some(handle)` if the name is registered and the type matches.
/// * `None` if the name is not found or the type doesn't match.
///
/// # Example
///
/// ```ignore
/// // Register a counter server
/// let counter = Counter::new().start(Backend::Async);
/// registry::register_handle("counter", counter)?;
///
/// // Look up and use it
/// if let Some(h) = registry::lookup::<Counter>("counter") {
///     let count = h.call(CounterMsg::Get).await?;
///     println!("Count: {}", count);
/// }
///
/// // Wrong type returns None, not panic
/// let wrong: Option<GenServerHandle<OtherServer>> = registry::lookup("counter");
/// assert!(wrong.is_none());
/// ```
pub fn lookup<G: GenServer + 'static>(name: &str) -> Option<GenServerHandle<G>> {
    let registry = REGISTRY.read().unwrap();
    registry
        .handles
        .get(name)?
        .downcast_ref::<GenServerHandle<G>>()
        .cloned()
}

/// Unregister a name from the registry.
///
/// This removes the name, its associated Pid, and any stored handle from the registry.
/// If the name doesn't exist, this is a no-op.
pub fn unregister(name: &str) {
    let mut registry = REGISTRY.write().unwrap();
    registry.handles.remove(name);
    if let Some(pid) = registry.by_name.remove(name) {
        registry.by_pid.remove(&pid);
    }
}

/// Unregister a process by its Pid.
///
/// This removes the process, its associated name, and any stored handle from the registry.
/// If the process isn't registered, this is a no-op.
pub fn unregister_pid(pid: Pid) {
    let mut registry = REGISTRY.write().unwrap();
    if let Some(name) = registry.by_pid.remove(&pid) {
        registry.by_name.remove(&name);
        registry.handles.remove(&name);
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

/// Check if a name has a typed handle stored.
///
/// Returns true if the name was registered via [`register_handle`],
/// false if registered via [`register`] or not registered at all.
pub fn has_handle(name: &str) -> bool {
    let registry = REGISTRY.read().unwrap();
    registry.handles.contains_key(name)
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
    registry.handles.clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messages::Unused;
    use crate::{Backend, CallResponse, CastResponse};
    use std::sync::Mutex;

    // Mutex to serialize tests that need an isolated registry
    static TEST_MUTEX: Mutex<()> = Mutex::new(());

    // Helper to ensure test isolation - clears registry and holds lock
    fn with_clean_registry<F, R>(f: F) -> R
    where
        F: FnOnce() -> R,
    {
        let _guard = TEST_MUTEX.lock().unwrap();
        clear();
        let result = f();
        clear();
        result
    }

    // Test GenServer for typed registry tests
    struct TestServer {
        value: u32,
    }

    #[derive(Clone)]
    enum TestCall {
        Get,
        Set(u32),
    }

    impl GenServer for TestServer {
        type CallMsg = TestCall;
        type CastMsg = Unused;
        type OutMsg = u32;
        type Error = ();

        async fn handle_call(
            &mut self,
            message: Self::CallMsg,
            _handle: &GenServerHandle<Self>,
        ) -> CallResponse<Self> {
            match message {
                TestCall::Get => CallResponse::Reply(self.value),
                TestCall::Set(v) => {
                    self.value = v;
                    CallResponse::Reply(v)
                }
            }
        }
    }

    // Another test GenServer to verify type safety
    struct OtherServer;

    impl GenServer for OtherServer {
        type CallMsg = Unused;
        type CastMsg = Unused;
        type OutMsg = Unused;
        type Error = ();

        async fn handle_call(
            &mut self,
            _message: Self::CallMsg,
            _handle: &GenServerHandle<Self>,
        ) -> CallResponse<Self> {
            CallResponse::Unused
        }

        async fn handle_cast(
            &mut self,
            _message: Self::CastMsg,
            _handle: &GenServerHandle<Self>,
        ) -> CastResponse {
            CastResponse::Unused
        }
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
        // Use with_clean_registry for test isolation
        with_clean_registry(|| {
            let pid1 = Pid::new();
            let pid2 = Pid::new();

            let name1 = format!("count_test_{}", pid1.id());
            let name2 = format!("count_test_{}", pid2.id());

            assert_eq!(count(), 0, "Registry should be empty");

            register(&name1, pid1).unwrap();
            assert_eq!(count(), 1, "Count should be 1 after first registration");

            register(&name2, pid2).unwrap();
            assert_eq!(count(), 2, "Count should be 2 after second registration");

            unregister(&name1);
            assert_eq!(count(), 1, "Count should be 1 after unregistration");

            unregister(&name2);
            assert_eq!(count(), 0, "Count should be 0 after all unregistrations");
        });
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

    // ==================== Typed Registry Tests ====================

    #[test]
    fn test_register_handle_and_lookup() {
        with_clean_registry(|| {
            let rt = spawned_rt::tasks::Runtime::new().unwrap();
            rt.block_on(async {
                let handle = TestServer { value: 42 }.start(Backend::Async);
                let pid = handle.pid();
                let name = format!("typed_server_{}", pid.id());

                // Register the handle
                assert!(register_handle(&name, handle.clone()).is_ok());

                // Should be findable via whereis
                assert_eq!(whereis(&name), Some(pid));

                // Should be findable via lookup
                let looked_up = lookup::<TestServer>(&name);
                assert!(looked_up.is_some());
                let looked_up = looked_up.unwrap();
                assert_eq!(looked_up.pid(), pid);

                // Should be able to use the looked up handle
                let mut h = looked_up;
                let result = h.call(TestCall::Get).await.unwrap();
                assert_eq!(result, 42);
            });
        });
    }

    #[test]
    fn test_lookup_wrong_type_returns_none() {
        with_clean_registry(|| {
            let rt = spawned_rt::tasks::Runtime::new().unwrap();
            rt.block_on(async {
                let handle = TestServer { value: 42 }.start(Backend::Async);
                let name = format!("typed_server_{}", handle.pid().id());

                register_handle(&name, handle).unwrap();

                // Wrong type should return None, not panic
                let wrong: Option<GenServerHandle<OtherServer>> = lookup::<OtherServer>(&name);
                assert!(wrong.is_none());
            });
        });
    }

    #[test]
    fn test_lookup_nonexistent_returns_none() {
        with_clean_registry(|| {
            let result: Option<GenServerHandle<TestServer>> = lookup::<TestServer>("nonexistent");
            assert!(result.is_none());
        });
    }

    #[test]
    fn test_register_handle_duplicate_name() {
        with_clean_registry(|| {
            let rt = spawned_rt::tasks::Runtime::new().unwrap();
            rt.block_on(async {
                let handle1 = TestServer { value: 1 }.start(Backend::Async);
                let handle2 = TestServer { value: 2 }.start(Backend::Async);
                let name = format!("dup_name_{}", handle1.pid().id());

                assert!(register_handle(&name, handle1).is_ok());
                assert_eq!(
                    register_handle(&name, handle2),
                    Err(RegistryError::AlreadyRegistered)
                );
            });
        });
    }

    #[test]
    fn test_register_handle_process_twice() {
        with_clean_registry(|| {
            let rt = spawned_rt::tasks::Runtime::new().unwrap();
            rt.block_on(async {
                let handle = TestServer { value: 1 }.start(Backend::Async);
                let name1 = format!("name1_{}", handle.pid().id());
                let name2 = format!("name2_{}", handle.pid().id());

                assert!(register_handle(&name1, handle.clone()).is_ok());
                assert_eq!(
                    register_handle(&name2, handle),
                    Err(RegistryError::ProcessAlreadyNamed)
                );
            });
        });
    }

    #[test]
    fn test_unregister_removes_handle() {
        with_clean_registry(|| {
            let rt = spawned_rt::tasks::Runtime::new().unwrap();
            rt.block_on(async {
                let handle = TestServer { value: 42 }.start(Backend::Async);
                let name = format!("remove_handle_{}", handle.pid().id());

                register_handle(&name, handle).unwrap();
                assert!(has_handle(&name));

                unregister(&name);
                assert!(!has_handle(&name));
                assert!(lookup::<TestServer>(&name).is_none());
            });
        });
    }

    #[test]
    fn test_unregister_pid_removes_handle() {
        with_clean_registry(|| {
            let rt = spawned_rt::tasks::Runtime::new().unwrap();
            rt.block_on(async {
                let handle = TestServer { value: 42 }.start(Backend::Async);
                let pid = handle.pid();
                let name = format!("remove_by_pid_{}", pid.id());

                register_handle(&name, handle).unwrap();
                assert!(has_handle(&name));

                unregister_pid(pid);
                assert!(!has_handle(&name));
                assert!(lookup::<TestServer>(&name).is_none());
            });
        });
    }

    #[test]
    fn test_has_handle() {
        with_clean_registry(|| {
            let rt = spawned_rt::tasks::Runtime::new().unwrap();
            rt.block_on(async {
                let handle = TestServer { value: 42 }.start(Backend::Async);
                let pid = Pid::new();

                let name_with_handle = format!("with_handle_{}", handle.pid().id());
                let name_pid_only = format!("pid_only_{}", pid.id());

                // Register with handle
                register_handle(&name_with_handle, handle).unwrap();
                assert!(has_handle(&name_with_handle));

                // Register with Pid only
                register(&name_pid_only, pid).unwrap();
                assert!(!has_handle(&name_pid_only));

                // Nonexistent
                assert!(!has_handle("nonexistent"));
            });
        });
    }

    #[test]
    fn test_lookup_after_reregister() {
        with_clean_registry(|| {
            let rt = spawned_rt::tasks::Runtime::new().unwrap();
            rt.block_on(async {
                let handle1 = TestServer { value: 1 }.start(Backend::Async);
                let handle2 = TestServer { value: 2 }.start(Backend::Async);
                let name = format!("reregister_{}", handle1.pid().id());

                // Register first handle
                register_handle(&name, handle1).unwrap();

                // Unregister
                unregister(&name);

                // Register second handle with same name
                register_handle(&name, handle2.clone()).unwrap();

                // Lookup should return second handle
                let looked_up = lookup::<TestServer>(&name).unwrap();
                assert_eq!(looked_up.pid(), handle2.pid());
            });
        });
    }

    #[test]
    fn test_clear_removes_handles() {
        with_clean_registry(|| {
            let rt = spawned_rt::tasks::Runtime::new().unwrap();
            rt.block_on(async {
                let handle = TestServer { value: 42 }.start(Backend::Async);
                let name = format!("clear_test_{}", handle.pid().id());

                register_handle(&name, handle).unwrap();
                assert!(has_handle(&name));

                clear();

                assert!(!has_handle(&name));
                assert!(lookup::<TestServer>(&name).is_none());
                assert!(whereis(&name).is_none());
            });
        });
    }

    #[test]
    fn test_typed_registry_with_messaging() {
        with_clean_registry(|| {
            let rt = spawned_rt::tasks::Runtime::new().unwrap();
            rt.block_on(async {
                // Start server and register
                let handle = TestServer { value: 0 }.start(Backend::Async);
                let name = format!("counter_{}", handle.pid().id());
                register_handle(&name, handle).unwrap();

                // Look up and send messages
                let mut h = lookup::<TestServer>(&name).unwrap();
                assert_eq!(h.call(TestCall::Get).await.unwrap(), 0);
                assert_eq!(h.call(TestCall::Set(100)).await.unwrap(), 100);
                assert_eq!(h.call(TestCall::Get).await.unwrap(), 100);

                // Look up again to verify state persists
                let mut h2 = lookup::<TestServer>(&name).unwrap();
                assert_eq!(h2.call(TestCall::Get).await.unwrap(), 100);
            });
        });
    }
}
