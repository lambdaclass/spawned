//! Application behavior for managing supervision trees.
//!
//! Similar to Erlang's `application` behavior, this provides a way to
//! package and manage a supervision tree as a unit.
//!
//! # Example
//!
//! ```ignore
//! use spawned_concurrency::application::{Application, ApplicationError};
//! use spawned_concurrency::supervisor::{RestartStrategy, Supervisor, SupervisorSpec, ChildSpec};
//! use spawned_concurrency::{GenServerHandle, Backend};
//!
//! struct MyApp;
//!
//! impl Application for MyApp {
//!     type Config = MyConfig;
//!
//!     fn start(config: Self::Config) -> Result<GenServerHandle<Supervisor>, ApplicationError> {
//!         let spec = SupervisorSpec::new(RestartStrategy::OneForOne)
//!             .child(ChildSpec::worker("worker", || MyWorker::new().start(Backend::Async)));
//!         Ok(Supervisor::start(spec))
//!     }
//! }
//!
//! // Start the application
//! let pid = application::start::<MyApp>(config)?;
//!
//! // Check if running
//! assert!(application::is_running::<MyApp>());
//!
//! // Stop the application
//! application::stop::<MyApp>().await?;
//! ```

use crate::supervisor::{Supervisor, SupervisorCall, SupervisorResponse};
use crate::{GenServerHandle, HasPid, Pid};
use std::any::TypeId;
use std::collections::HashMap;
use std::sync::RwLock;

/// Error type for application operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplicationError {
    /// Application is already running.
    AlreadyStarted(String),
    /// Application is not running.
    NotRunning(String),
    /// Failed to start the application.
    StartFailed(String),
    /// Failed to stop the application.
    StopFailed(String),
}

impl std::fmt::Display for ApplicationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApplicationError::AlreadyStarted(name) => {
                write!(f, "application '{}' is already started", name)
            }
            ApplicationError::NotRunning(name) => {
                write!(f, "application '{}' is not running", name)
            }
            ApplicationError::StartFailed(msg) => {
                write!(f, "failed to start application: {}", msg)
            }
            ApplicationError::StopFailed(msg) => {
                write!(f, "failed to stop application: {}", msg)
            }
        }
    }
}

impl std::error::Error for ApplicationError {}

/// Trait for defining an application.
///
/// An application is a component that can be started and stopped as a unit,
/// typically wrapping a supervision tree.
///
/// # Example
///
/// ```ignore
/// struct MyApp;
///
/// impl Application for MyApp {
///     type Config = u32; // Initial counter value
///
///     fn name() -> &'static str {
///         "my_app"
///     }
///
///     fn start(config: Self::Config) -> Result<GenServerHandle<Supervisor>, ApplicationError> {
///         let spec = SupervisorSpec::new(RestartStrategy::OneForOne)
///             .child(ChildSpec::worker("counter", move || {
///                 Counter::new(config).start(Backend::Async)
///             }));
///         Ok(Supervisor::start(spec))
///     }
///
///     fn prep_stop(handle: &GenServerHandle<Supervisor>) {
///         // Cleanup before stopping
///         println!("Preparing to stop MyApp");
///     }
///
///     fn stopped() {
///         // Final cleanup after stopping
///         println!("MyApp has stopped");
///     }
/// }
/// ```
pub trait Application: 'static {
    /// Configuration type for this application.
    type Config: Send + 'static;

    /// The name of this application.
    ///
    /// Used for registration and error messages.
    fn name() -> &'static str {
        std::any::type_name::<Self>()
    }

    /// Start the application with the given configuration.
    ///
    /// Returns a handle to the root supervisor.
    fn start(config: Self::Config) -> Result<GenServerHandle<Supervisor>, ApplicationError>;

    /// Called before the application stops.
    ///
    /// Override to perform cleanup before the supervisor tree is terminated.
    fn prep_stop(_handle: &GenServerHandle<Supervisor>) {}

    /// Called after the application has stopped.
    ///
    /// Override to perform final cleanup after the supervisor tree is gone.
    fn stopped() {}
}

/// Internal state for a running application.
struct RunningApp {
    name: &'static str,
    handle: GenServerHandle<Supervisor>,
    pid: Pid,
}

/// Global registry of running applications.
static APPLICATIONS: std::sync::LazyLock<RwLock<HashMap<TypeId, RunningApp>>> =
    std::sync::LazyLock::new(|| RwLock::new(HashMap::new()));

/// Start an application.
///
/// # Errors
///
/// Returns an error if the application is already started.
///
/// # Example
///
/// ```ignore
/// let pid = application::start::<MyApp>(config)?;
/// println!("Started application with root supervisor pid: {}", pid);
/// ```
pub fn start<A: Application>(config: A::Config) -> Result<Pid, ApplicationError> {
    let type_id = TypeId::of::<A>();
    let name = A::name();

    // Check if already running
    {
        let apps = APPLICATIONS.read().unwrap();
        if apps.contains_key(&type_id) {
            return Err(ApplicationError::AlreadyStarted(name.to_string()));
        }
    }

    // Start the application
    let handle = A::start(config)?;
    let pid = handle.pid();

    // Register the running application
    {
        let mut apps = APPLICATIONS.write().unwrap();
        apps.insert(type_id, RunningApp { name, handle, pid });
    }

    tracing::info!(%pid, name, "Application started");
    Ok(pid)
}

/// Stop a running application.
///
/// # Errors
///
/// Returns an error if the application is not running.
///
/// # Example
///
/// ```ignore
/// application::stop::<MyApp>().await?;
/// println!("Application stopped");
/// ```
pub async fn stop<A: Application>() -> Result<(), ApplicationError> {
    let type_id = TypeId::of::<A>();
    let name = A::name();

    // Get the running app
    let running = {
        let mut apps = APPLICATIONS.write().unwrap();
        apps.remove(&type_id)
    };

    match running {
        Some(app) => {
            tracing::info!(pid = %app.pid, name = app.name, "Stopping application");

            // Call prep_stop hook
            A::prep_stop(&app.handle);

            // Stop the supervisor
            app.handle.stop();

            // Wait a bit for clean shutdown
            spawned_rt::tasks::sleep(std::time::Duration::from_millis(50)).await;

            // Call stopped hook
            A::stopped();

            tracing::info!(name = app.name, "Application stopped");
            Ok(())
        }
        None => Err(ApplicationError::NotRunning(name.to_string())),
    }
}

/// Check if an application is running.
///
/// # Example
///
/// ```ignore
/// if application::is_running::<MyApp>() {
///     println!("MyApp is running");
/// }
/// ```
pub fn is_running<A: Application>() -> bool {
    let type_id = TypeId::of::<A>();
    let apps = APPLICATIONS.read().unwrap();
    apps.contains_key(&type_id)
}

/// Get the PID of a running application's root supervisor.
///
/// # Example
///
/// ```ignore
/// if let Some(pid) = application::whereis::<MyApp>() {
///     println!("MyApp root supervisor pid: {}", pid);
/// }
/// ```
pub fn whereis<A: Application>() -> Option<Pid> {
    let type_id = TypeId::of::<A>();
    let apps = APPLICATIONS.read().unwrap();
    apps.get(&type_id).map(|app| app.pid)
}

/// Get the names of all running applications.
pub fn which_applications() -> Vec<&'static str> {
    let apps = APPLICATIONS.read().unwrap();
    apps.values().map(|app| app.name).collect()
}

/// Send a call to a running application's supervisor.
///
/// # Example
///
/// ```ignore
/// let response = application::call::<MyApp>(SupervisorCall::CountChildren).await?;
/// println!("Response: {:?}", response);
/// ```
pub async fn call<A: Application>(
    msg: SupervisorCall,
) -> Result<SupervisorResponse, ApplicationError> {
    let type_id = TypeId::of::<A>();
    let name = A::name();

    let mut handle = {
        let apps = APPLICATIONS.read().unwrap();
        match apps.get(&type_id) {
            Some(app) => app.handle.clone(),
            None => return Err(ApplicationError::NotRunning(name.to_string())),
        }
    };

    handle
        .call(msg)
        .await
        .map_err(|e| ApplicationError::StopFailed(e.to_string()))
}

/// Get a clone of the application's supervisor handle.
///
/// Useful for more complex operations on the supervisor.
pub fn get_handle<A: Application>() -> Option<GenServerHandle<Supervisor>> {
    let type_id = TypeId::of::<A>();
    let apps = APPLICATIONS.read().unwrap();
    apps.get(&type_id).map(|app| app.handle.clone())
}

/// Clear all running applications (for testing).
#[cfg(test)]
pub fn clear() {
    let mut apps = APPLICATIONS.write().unwrap();
    apps.clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supervisor::{ChildSpec, RestartStrategy, SupervisorSpec};
    use crate::{Backend, CallResponse, CastResponse, GenServer};

    // Mutex to serialize application tests (shared global state)
    static APPLICATION_TEST_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    // Simple counter for testing
    struct TestCounter {
        value: i32,
    }

    #[derive(Clone, Debug)]
    enum CounterCall {
        Get,
    }

    #[derive(Clone, Debug)]
    enum CounterCast {}

    impl GenServer for TestCounter {
        type CallMsg = CounterCall;
        type CastMsg = CounterCast;
        type OutMsg = i32;
        type Error = ();

        async fn handle_call(
            &mut self,
            message: Self::CallMsg,
            _handle: &GenServerHandle<Self>,
        ) -> CallResponse<Self> {
            match message {
                CounterCall::Get => CallResponse::Reply(self.value),
            }
        }

        async fn handle_cast(
            &mut self,
            _message: Self::CastMsg,
            _handle: &GenServerHandle<Self>,
        ) -> CastResponse {
            CastResponse::NoReply
        }
    }

    struct TestApp;

    impl Application for TestApp {
        type Config = i32;

        fn name() -> &'static str {
            "test_app"
        }

        fn start(config: Self::Config) -> Result<GenServerHandle<Supervisor>, ApplicationError> {
            let spec = SupervisorSpec::new(RestartStrategy::OneForOne).child(ChildSpec::worker(
                "counter",
                move || TestCounter { value: config }.start(Backend::Async),
            ));

            Ok(Supervisor::start(spec))
        }
    }

    struct AnotherApp;

    impl Application for AnotherApp {
        type Config = ();

        fn name() -> &'static str {
            "another_app"
        }

        fn start(_config: Self::Config) -> Result<GenServerHandle<Supervisor>, ApplicationError> {
            let spec = SupervisorSpec::new(RestartStrategy::OneForOne).child(ChildSpec::worker(
                "counter",
                || TestCounter { value: 0 }.start(Backend::Async),
            ));

            Ok(Supervisor::start(spec))
        }
    }

    #[tokio::test]
    async fn test_application_start_stop() {
        let _guard = APPLICATION_TEST_MUTEX.lock().unwrap();
        clear();
        // Start the application
        let pid = start::<TestApp>(42).unwrap();
        assert!(is_running::<TestApp>());
        assert_eq!(whereis::<TestApp>(), Some(pid));

        // Can't start twice
        assert!(matches!(
            start::<TestApp>(0),
            Err(ApplicationError::AlreadyStarted(_))
        ));

        // Stop the application
        stop::<TestApp>().await.unwrap();
        assert!(!is_running::<TestApp>());
        assert_eq!(whereis::<TestApp>(), None);

        // Can't stop twice
        assert!(matches!(
            stop::<TestApp>().await,
            Err(ApplicationError::NotRunning(_))
        ));
    }

    #[tokio::test]
    async fn test_multiple_applications() {
        let _guard = APPLICATION_TEST_MUTEX.lock().unwrap();
        clear();
        // Start both applications
        let pid1 = start::<TestApp>(10).unwrap();
        let pid2 = start::<AnotherApp>(()).unwrap();

        assert!(is_running::<TestApp>());
        assert!(is_running::<AnotherApp>());
        assert_ne!(pid1, pid2);

        // Both should be in which_applications
        let running = which_applications();
        assert_eq!(running.len(), 2);
        assert!(running.contains(&"test_app"));
        assert!(running.contains(&"another_app"));

        // Stop both
        stop::<TestApp>().await.unwrap();
        stop::<AnotherApp>().await.unwrap();

        assert!(!is_running::<TestApp>());
        assert!(!is_running::<AnotherApp>());
        assert!(which_applications().is_empty());
    }

    #[tokio::test]
    async fn test_application_call() {
        let _guard = APPLICATION_TEST_MUTEX.lock().unwrap();
        clear();
        start::<TestApp>(42).unwrap();

        // Call the supervisor
        let response = call::<TestApp>(SupervisorCall::CountChildren).await.unwrap();
        assert!(matches!(response, SupervisorResponse::Counts(_)));

        stop::<TestApp>().await.unwrap();
    }

    #[tokio::test]
    async fn test_application_get_handle() {
        let _guard = APPLICATION_TEST_MUTEX.lock().unwrap();
        clear();
        start::<TestApp>(42).unwrap();

        // Get the handle
        let handle = get_handle::<TestApp>();
        assert!(handle.is_some());

        // Not running app has no handle
        let no_handle = get_handle::<AnotherApp>();
        assert!(no_handle.is_none());

        stop::<TestApp>().await.unwrap();
    }

    #[test]
    fn test_application_error_display() {
        let err = ApplicationError::AlreadyStarted("test".to_string());
        assert_eq!(
            format!("{}", err),
            "application 'test' is already started"
        );

        let err = ApplicationError::NotRunning("test".to_string());
        assert_eq!(format!("{}", err), "application 'test' is not running");

        let err = ApplicationError::StartFailed("oops".to_string());
        assert_eq!(format!("{}", err), "failed to start application: oops");

        let err = ApplicationError::StopFailed("oops".to_string());
        assert_eq!(format!("{}", err), "failed to stop application: oops");
    }
}
