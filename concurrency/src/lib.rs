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
/// Determines how an Actor is spawned and executed. Choose the backend that
/// best matches your actor's workload characteristics.
///
/// # Comparison Table
///
/// | Backend | Execution Model | Best For | Limitations |
/// |---------|-----------------|----------|-------------|
/// | `Async` | Tokio tasks (cooperative) | I/O-bound, async work | Must not block |
/// | `Blocking` | Blocking thread pool | Blocking I/O, short CPU work | Limited pool size |
/// | `Thread` | Dedicated OS thread | Long-running singletons | Resource heavy |
///
/// # When to Use Each Backend
///
/// ## `Async` (Default)
/// - Non-blocking network I/O
/// - Database queries via async drivers
/// - HTTP clients/servers
/// - Any work that uses `async`/`await`
///
/// **Avoid when:** Your code blocks (e.g., `std::thread::sleep`, synchronous I/O)
///
/// ## `Blocking`
/// - Synchronous file I/O
/// - CPU-bound work that completes quickly
/// - Calling blocking libraries without async support
///
/// **Avoid when:** Work runs for extended periods (use `Thread` instead)
///
/// ## `Thread`
/// - Long-running background workers
/// - Singleton services that run for the application lifetime
/// - Actors that must never be preempted
///
/// **Avoid when:** You need many actors (OS threads are expensive)
///
/// # Example
///
/// ```ignore
/// use spawned_concurrency::{Backend, tasks::Actor};
///
/// // Default async execution
/// let handle = MyActor::new().start();
///
/// // Or with explicit backend selection
/// let handle = MyActor::new().start_with_backend(Backend::Async);
/// let handle = MyActor::new().start_with_backend(Backend::Blocking);
/// let handle = MyActor::new().start_with_backend(Backend::Thread);
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Backend {
    /// Run on the async runtime using Tokio tasks.
    ///
    /// Uses cooperative multitasking - tasks must yield (via `.await`) to
    /// allow other tasks to run. Best for non-blocking, I/O-bound workloads.
    ///
    /// This is the default and most efficient option for async code.
    #[default]
    Async,

    /// Run on Tokio's blocking thread pool via `spawn_blocking`.
    ///
    /// Best for blocking I/O operations or short CPU-bound tasks that would
    /// otherwise block the async runtime. The blocking thread pool is limited
    /// in size, so this should be used for work that completes relatively quickly.
    Blocking,

    /// Run on a dedicated OS thread.
    ///
    /// Best for long-running singleton actors that should not interfere with
    /// the async runtime's thread pool. Use sparingly as each actor gets its
    /// own OS thread, which is resource-intensive.
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
