//! spawned concurrency
//! Some basic traits and structs to implement concurrent code à-la-Erlang.
pub mod error;
pub mod link;
pub mod messages;
pub mod pid;
pub mod process_table;
pub mod registry;
pub mod supervisor;
pub mod tasks;
pub mod threads;

/// Backend selection for Actor execution.
///
/// Determines how an Actor is spawned and executed:
/// - `Async`: Runs on the async runtime (tokio tasks) - cooperative multitasking
/// - `Blocking`: Runs on a blocking thread pool (spawn_blocking) - for blocking I/O
/// - `Thread`: Runs on a dedicated OS thread - for long-running singletons
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Backend {
    /// Run on the async runtime (default). Best for non-blocking, I/O-bound tasks.
    #[default]
    Async,
    /// Run on a blocking thread pool. Best for blocking I/O or CPU-bound tasks.
    Blocking,
    /// Run on a dedicated OS thread. Best for long-running singleton actors.
    Thread,
}

// Re-export commonly used types at the crate root
pub use link::{MonitorRef, SystemMessage};
pub use pid::{ExitReason, HasPid, Pid};
pub use process_table::LinkError;
pub use registry::RegistryError;
pub use supervisor::{
    BoxedChildHandle, ChildHandle, ChildInfo, ChildSpec, ChildType, DynamicSupervisor,
    DynamicSupervisorCall, DynamicSupervisorCast, DynamicSupervisorError, DynamicSupervisorResponse,
    DynamicSupervisorSpec, RestartIntensityTracker, RestartStrategy, RestartType, Shutdown,
    Supervisor, SupervisorCall, SupervisorCast, SupervisorCounts, SupervisorError,
    SupervisorResponse, SupervisorSpec,
};

#[cfg(test)]
mod backend_tests {
    use super::Backend;

    #[test]
    fn test_backend_default() {
        assert_eq!(Backend::default(), Backend::Async);
    }

    #[test]
    fn test_backend_copy() {
        let backend = Backend::Async;
        let copied = backend; // Copy
        assert_eq!(backend, copied);
    }

    #[test]
    fn test_backend_clone() {
        let backend = Backend::Blocking;
        #[allow(clippy::clone_on_copy)]
        let cloned = backend.clone(); // Explicit clone to test Clone trait
        assert_eq!(backend, cloned);
    }

    #[test]
    fn test_backend_debug() {
        assert_eq!(format!("{:?}", Backend::Async), "Async");
        assert_eq!(format!("{:?}", Backend::Blocking), "Blocking");
        assert_eq!(format!("{:?}", Backend::Thread), "Thread");
    }

    #[test]
    fn test_backend_equality() {
        assert_eq!(Backend::Async, Backend::Async);
        assert_eq!(Backend::Blocking, Backend::Blocking);
        assert_eq!(Backend::Thread, Backend::Thread);
        assert_ne!(Backend::Async, Backend::Blocking);
        assert_ne!(Backend::Async, Backend::Thread);
        assert_ne!(Backend::Blocking, Backend::Thread);
    }

    #[test]
    fn test_backend_variants_exhaustive() {
        // Ensure all variants can be matched
        let backends = [Backend::Async, Backend::Blocking, Backend::Thread];
        for backend in backends {
            match backend {
                Backend::Async => {}
                Backend::Blocking => {}
                Backend::Thread => {}
            }
        }
    }
}
