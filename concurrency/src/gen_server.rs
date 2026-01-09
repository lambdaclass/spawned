//! GenServer trait and structs to create an abstraction similar to Erlang gen_server.
//! See examples/name_server for a usage example.
use crate::{
    error::GenServerError,
    link::{MonitorRef, SystemMessage},
    pid::{ExitReason, HasPid, Pid},
    process_table::{self, LinkError, SystemMessageSender},
    registry::{self, RegistryError},
    InitResult::{NoSuccess, Success},
};
use core::pin::pin;
use futures::future::{self, FutureExt};
use spawned_rt::{
    tasks::{self as rt, mpsc, oneshot, timeout, CancellationToken, JoinHandle},
    threads,
};
use std::{fmt::Debug, future::Future, panic::AssertUnwindSafe, sync::Arc, time::Duration};

const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(5);

/// Execution backend for GenServer.
///
/// Determines how the GenServer's async loop is executed. Choose based on
/// the nature of your workload:
///
/// # Backend Comparison
///
/// | Backend | Execution Model | Best For | Limitations |
/// |---------|-----------------|----------|-------------|
/// | `Async` | Tokio task | Non-blocking I/O, async operations | Blocks runtime if sync code runs too long |
/// | `Blocking` | Tokio blocking pool | Short blocking operations (file I/O, DNS) | Shared pool with limited threads |
/// | `Thread` | Dedicated OS thread | Long-running blocking work, CPU-heavy tasks | Higher memory overhead per GenServer |
///
/// # Examples
///
/// ```ignore
/// // For typical async workloads (HTTP handlers, database queries)
/// let handle = MyServer::new().start(Backend::Async);
///
/// // For occasional blocking operations (file reads, external commands)
/// let handle = MyServer::new().start(Backend::Blocking);
///
/// // For CPU-intensive or permanently blocking services
/// let handle = MyServer::new().start(Backend::Thread);
/// ```
///
/// # When to Use Each Backend
///
/// ## `Backend::Async` (Default)
/// - **Advantages**: Lightweight, efficient, good for high concurrency
/// - **Use when**: Your GenServer does mostly async I/O (network, database)
/// - **Avoid when**: Your code blocks (e.g., `std::thread::sleep`, heavy computation)
///
/// ## `Backend::Blocking`
/// - **Advantages**: Prevents blocking the async runtime, uses tokio's managed pool
/// - **Use when**: You have occasional blocking operations that complete quickly
/// - **Avoid when**: You need guaranteed thread availability or long-running blocks
///
/// ## `Backend::Thread`
/// - **Advantages**: Complete isolation, no interference with async runtime
/// - **Use when**: Long-running blocking work, singleton services, CPU-bound tasks
/// - **Avoid when**: You need many GenServers (each gets its own OS thread)
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Backend {
    /// Run on tokio async runtime (default).
    ///
    /// Best for non-blocking, async workloads. The GenServer runs as a
    /// lightweight tokio task, enabling high concurrency with minimal overhead.
    ///
    /// **Warning**: If your `handle_call` or `handle_cast` blocks synchronously
    /// (e.g., `std::thread::sleep`, CPU-heavy loops), it will block the entire
    /// tokio runtime thread, affecting other tasks.
    #[default]
    Async,

    /// Run on tokio's blocking thread pool.
    ///
    /// Use for GenServers that perform blocking operations like:
    /// - Synchronous file I/O
    /// - DNS lookups
    /// - External process calls
    /// - Short CPU-bound computations
    ///
    /// The pool is shared across all `spawn_blocking` calls and has a default
    /// limit of 512 threads. If the pool is exhausted, new blocking tasks wait.
    Blocking,

    /// Run on a dedicated OS thread.
    ///
    /// Use for GenServers that:
    /// - Block indefinitely or for long periods
    /// - Need guaranteed thread availability
    /// - Should not compete with other blocking tasks
    /// - Run CPU-intensive workloads
    ///
    /// Each GenServer gets its own thread, providing complete isolation from
    /// the async runtime. Higher memory overhead (~2MB stack per thread).
    Thread,
}

/// Handle to a running GenServer.
///
/// This handle can be used to send messages to the GenServer and to
/// obtain its unique process identifier (`Pid`).
///
/// Handles are cheap to clone and can be shared across tasks.
#[derive(Debug)]
pub struct GenServerHandle<G: GenServer + 'static> {
    /// Unique process identifier for this GenServer.
    pid: Pid,
    /// Channel sender for messages to the GenServer.
    pub tx: mpsc::Sender<GenServerInMsg<G>>,
    /// Cancellation token to stop the GenServer.
    cancellation_token: CancellationToken,
    /// Channel for system messages (internal use).
    system_tx: mpsc::Sender<SystemMessage>,
}

impl<G: GenServer> Clone for GenServerHandle<G> {
    fn clone(&self) -> Self {
        Self {
            pid: self.pid,
            tx: self.tx.clone(),
            cancellation_token: self.cancellation_token.clone(),
            system_tx: self.system_tx.clone(),
        }
    }
}

impl<G: GenServer> HasPid for GenServerHandle<G> {
    fn pid(&self) -> Pid {
        self.pid
    }
}

/// Internal sender for system messages, implementing SystemMessageSender trait.
struct GenServerSystemSender {
    system_tx: mpsc::Sender<SystemMessage>,
    cancellation_token: CancellationToken,
}

impl SystemMessageSender for GenServerSystemSender {
    fn send_down(&self, pid: Pid, monitor_ref: MonitorRef, reason: ExitReason) {
        let _ = self.system_tx.send(SystemMessage::Down {
            pid,
            monitor_ref,
            reason,
        });
    }

    fn send_exit(&self, pid: Pid, reason: ExitReason) {
        let _ = self.system_tx.send(SystemMessage::Exit { pid, reason });
    }

    fn kill(&self, _reason: ExitReason) {
        // Kill the process by cancelling it
        self.cancellation_token.cancel();
    }

    fn is_alive(&self) -> bool {
        !self.cancellation_token.is_cancelled()
    }
}

/// Internal struct holding the initialized components for a GenServer.
struct GenServerInit<G: GenServer + 'static> {
    pid: Pid,
    handle: GenServerHandle<G>,
    rx: mpsc::Receiver<GenServerInMsg<G>>,
    system_rx: mpsc::Receiver<SystemMessage>,
}

impl<G: GenServer> GenServerHandle<G> {
    /// Common initialization for all backends.
    /// Returns the handle and channels needed to run the GenServer.
    fn init(gen_server: G) -> (GenServerInit<G>, G) {
        let pid = Pid::new();
        let (tx, rx) = mpsc::channel::<GenServerInMsg<G>>();
        let (system_tx, system_rx) = mpsc::channel::<SystemMessage>();
        let cancellation_token = CancellationToken::new();

        // Create the system message sender and register with process table
        let system_sender = Arc::new(GenServerSystemSender {
            system_tx: system_tx.clone(),
            cancellation_token: cancellation_token.clone(),
        });
        process_table::register(pid, system_sender);

        let handle = GenServerHandle {
            pid,
            tx,
            cancellation_token,
            system_tx,
        };

        (
            GenServerInit {
                pid,
                handle,
                rx,
                system_rx,
            },
            gen_server,
        )
    }

    /// Run the GenServer and handle cleanup on exit.
    async fn run_and_cleanup(
        gen_server: G,
        handle: &GenServerHandle<G>,
        rx: &mut mpsc::Receiver<GenServerInMsg<G>>,
        system_rx: &mut mpsc::Receiver<SystemMessage>,
        pid: Pid,
    ) {
        let result = gen_server.run(handle, rx, system_rx).await;
        let exit_reason = match &result {
            Ok(_) => ExitReason::Normal,
            Err(_) => ExitReason::Error("GenServer crashed".to_string()),
        };
        process_table::unregister(pid, exit_reason);
        if let Err(error) = result {
            tracing::trace!(%error, "GenServer crashed")
        }
    }

    fn new(gen_server: G) -> Self {
        let (init, gen_server) = Self::init(gen_server);
        let GenServerInit {
            pid,
            handle,
            mut rx,
            mut system_rx,
        } = init;
        let handle_clone = handle.clone();

        let inner_future = async move {
            Self::run_and_cleanup(gen_server, &handle, &mut rx, &mut system_rx, pid).await;
        };

        #[cfg(debug_assertions)]
        let inner_future = warn_on_block::WarnOnBlocking::new(inner_future);

        let _join_handle = rt::spawn(inner_future);
        handle_clone
    }

    fn new_blocking(gen_server: G) -> Self {
        let (init, gen_server) = Self::init(gen_server);
        let GenServerInit {
            pid,
            handle,
            mut rx,
            mut system_rx,
        } = init;
        let handle_clone = handle.clone();

        let _join_handle = rt::spawn_blocking(move || {
            rt::block_on(async move {
                Self::run_and_cleanup(gen_server, &handle, &mut rx, &mut system_rx, pid).await;
            })
        });
        handle_clone
    }

    fn new_on_thread(gen_server: G) -> Self {
        let (init, gen_server) = Self::init(gen_server);
        let GenServerInit {
            pid,
            handle,
            mut rx,
            mut system_rx,
        } = init;
        let handle_clone = handle.clone();

        let _join_handle = threads::spawn(move || {
            threads::block_on(async move {
                Self::run_and_cleanup(gen_server, &handle, &mut rx, &mut system_rx, pid).await;
            })
        });
        handle_clone
    }

    pub fn sender(&self) -> mpsc::Sender<GenServerInMsg<G>> {
        self.tx.clone()
    }

    pub async fn call(&mut self, message: G::CallMsg) -> Result<G::OutMsg, GenServerError> {
        self.call_with_timeout(message, DEFAULT_CALL_TIMEOUT).await
    }

    pub async fn call_with_timeout(
        &mut self,
        message: G::CallMsg,
        duration: Duration,
    ) -> Result<G::OutMsg, GenServerError> {
        let (oneshot_tx, oneshot_rx) = oneshot::channel::<Result<G::OutMsg, GenServerError>>();
        self.tx.send(GenServerInMsg::Call {
            sender: oneshot_tx,
            message,
        })?;

        match timeout(duration, oneshot_rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(GenServerError::Server),
            Err(_) => Err(GenServerError::CallTimeout),
        }
    }

    pub async fn cast(&mut self, message: G::CastMsg) -> Result<(), GenServerError> {
        self.tx
            .send(GenServerInMsg::Cast { message })
            .map_err(|_error| GenServerError::Server)
    }

    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancellation_token.clone()
    }

    /// Stop the GenServer by cancelling its token.
    ///
    /// This is a convenience method equivalent to `cancellation_token().cancel()`.
    /// The GenServer will exit and call its `teardown` method.
    pub fn stop(&self) {
        self.cancellation_token.cancel();
    }

    // ==================== Linking & Monitoring ====================

    /// Create a bidirectional link with another process.
    ///
    /// When either process exits abnormally, the other will be notified.
    /// If the other process is not trapping exits and this process crashes,
    /// the other process will also crash.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let handle1 = Server1::new().start(Backend::Async);
    /// let handle2 = Server2::new().start(Backend::Async);
    ///
    /// // Link the two processes
    /// handle1.link(&handle2)?;
    ///
    /// // Now if handle1 crashes, handle2 will also crash (unless trapping exits)
    /// ```
    pub fn link(&self, other: &impl HasPid) -> Result<(), LinkError> {
        process_table::link(self.pid, other.pid())
    }

    /// Remove a bidirectional link with another process.
    pub fn unlink(&self, other: &impl HasPid) {
        process_table::unlink(self.pid, other.pid())
    }

    /// Monitor another process.
    ///
    /// When the monitored process exits, this process will receive a DOWN message.
    /// Unlike links, monitors are unidirectional and don't cause the monitoring
    /// process to crash.
    ///
    /// Returns a `MonitorRef` that can be used to cancel the monitor.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let worker = Worker::new().start(Backend::Async);
    ///
    /// // Monitor the worker
    /// let monitor_ref = self_handle.monitor(&worker)?;
    ///
    /// // Later, if worker crashes, we'll receive a DOWN message
    /// // We can cancel the monitor if we no longer care:
    /// self_handle.demonitor(monitor_ref);
    /// ```
    pub fn monitor(&self, other: &impl HasPid) -> Result<MonitorRef, LinkError> {
        process_table::monitor(self.pid, other.pid())
    }

    /// Stop monitoring a process.
    pub fn demonitor(&self, monitor_ref: MonitorRef) {
        process_table::demonitor(monitor_ref)
    }

    /// Set whether this process traps exits.
    ///
    /// When trap_exit is true, EXIT messages from linked processes are delivered
    /// as messages instead of causing this process to crash.
    ///
    /// # Example
    ///
    /// ```ignore
    /// // Enable exit trapping
    /// handle.trap_exit(true);
    ///
    /// // Now when a linked process crashes, we'll receive an EXIT message
    /// // instead of crashing ourselves
    /// ```
    pub fn trap_exit(&self, trap: bool) {
        process_table::set_trap_exit(self.pid, trap)
    }

    /// Check if this process is trapping exits.
    pub fn is_trapping_exit(&self) -> bool {
        process_table::is_trapping_exit(self.pid)
    }

    /// Check if another process is alive.
    pub fn is_alive(&self, other: &impl HasPid) -> bool {
        process_table::is_alive(other.pid())
    }

    /// Get all processes linked to this process.
    pub fn get_links(&self) -> Vec<Pid> {
        process_table::get_links(self.pid)
    }

    // ==================== Registry ====================

    /// Register this process with a unique name.
    ///
    /// Once registered, other processes can find this process using
    /// `registry::whereis("name")`.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let handle = MyServer::new().start(Backend::Async);
    /// handle.register("my_server")?;
    ///
    /// // Now other processes can find it:
    /// // let pid = registry::whereis("my_server");
    /// ```
    pub fn register(&self, name: impl Into<String>) -> Result<(), RegistryError> {
        registry::register(name, self.pid)
    }

    /// Unregister this process from the registry.
    ///
    /// After this, the process can no longer be found by name.
    pub fn unregister(&self) {
        registry::unregister_pid(self.pid)
    }

    /// Get the registered name of this process, if any.
    pub fn registered_name(&self) -> Option<String> {
        registry::name_of(self.pid)
    }
}

pub enum GenServerInMsg<G: GenServer> {
    Call {
        sender: oneshot::Sender<Result<G::OutMsg, GenServerError>>,
        message: G::CallMsg,
    },
    Cast {
        message: G::CastMsg,
    },
}

pub enum CallResponse<G: GenServer> {
    Reply(G::OutMsg),
    Stop(G::OutMsg),
}

pub enum CastResponse {
    NoReply,
    Stop,
}

/// Response from handle_info callback.
pub enum InfoResponse {
    /// Continue running, message was handled.
    NoReply,
    /// Stop the GenServer.
    Stop,
}

pub enum InitResult<G: GenServer> {
    Success(G),
    NoSuccess(G),
}

pub trait GenServer: Send + Sized {
    type CallMsg: Clone + Send + Sized + Sync;
    type CastMsg: Clone + Send + Sized + Sync;
    type OutMsg: Send + Sized;
    type Error: Debug + Send;

    /// Start the GenServer with the specified backend.
    ///
    /// # Arguments
    /// * `backend` - The execution backend to use:
    ///   - `Backend::Async` - Run on tokio async runtime (default, best for non-blocking workloads)
    ///   - `Backend::Blocking` - Run on tokio's blocking thread pool (for blocking operations)
    ///   - `Backend::Thread` - Run on a dedicated OS thread (for long-running blocking services)
    fn start(self, backend: Backend) -> GenServerHandle<Self> {
        match backend {
            Backend::Async => GenServerHandle::new(self),
            Backend::Blocking => GenServerHandle::new_blocking(self),
            Backend::Thread => GenServerHandle::new_on_thread(self),
        }
    }

    /// Start the GenServer and create a bidirectional link with another process.
    ///
    /// This is equivalent to calling `start()` followed by `link()`, but as an
    /// atomic operation. If the link fails, the GenServer is stopped.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let parent = ParentServer::new().start(Backend::Async);
    /// let child = ChildServer::new().start_linked(&parent, Backend::Async)?;
    /// // Now if either crashes, the other will be notified
    /// ```
    fn start_linked(
        self,
        other: &impl HasPid,
        backend: Backend,
    ) -> Result<GenServerHandle<Self>, LinkError> {
        let handle = self.start(backend);
        handle.link(other)?;
        Ok(handle)
    }

    /// Start the GenServer and set up monitoring from another process.
    ///
    /// This is equivalent to calling `start()` followed by `monitor()`, but as an
    /// atomic operation. The monitoring process will receive a DOWN message when
    /// this GenServer exits.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let supervisor = SupervisorServer::new().start(Backend::Async);
    /// let (worker, monitor_ref) = WorkerServer::new().start_monitored(&supervisor, Backend::Async)?;
    /// // supervisor will receive DOWN message when worker exits
    /// ```
    fn start_monitored(
        self,
        monitor_from: &impl HasPid,
        backend: Backend,
    ) -> Result<(GenServerHandle<Self>, MonitorRef), LinkError> {
        let handle = self.start(backend);
        let monitor_ref = monitor_from.pid();
        let actual_ref = process_table::monitor(monitor_ref, handle.pid())?;
        Ok((handle, actual_ref))
    }

    fn run(
        self,
        handle: &GenServerHandle<Self>,
        rx: &mut mpsc::Receiver<GenServerInMsg<Self>>,
        system_rx: &mut mpsc::Receiver<SystemMessage>,
    ) -> impl Future<Output = Result<(), GenServerError>> + Send {
        async {
            let res = match self.init(handle).await {
                Ok(Success(new_state)) => Ok(new_state.main_loop(handle, rx, system_rx).await),
                Ok(NoSuccess(intermediate_state)) => {
                    // new_state is NoSuccess, this means the initialization failed, but the error was handled
                    // in callback. No need to report the error.
                    // Just skip main_loop and return the state to teardown the GenServer
                    Ok(intermediate_state)
                }
                Err(err) => {
                    tracing::error!("Initialization failed with unhandled error: {err:?}");
                    Err(GenServerError::Initialization)
                }
            };

            handle.cancellation_token().cancel();
            if let Ok(final_state) = res {
                if let Err(err) = final_state.teardown(handle).await {
                    tracing::error!("Error during teardown: {err:?}");
                }
            }
            Ok(())
        }
    }

    /// Initialization function. It's called before main loop. It
    /// can be overrided on implementations in case initial steps are
    /// required.
    fn init(
        self,
        _handle: &GenServerHandle<Self>,
    ) -> impl Future<Output = Result<InitResult<Self>, Self::Error>> + Send {
        async { Ok(Success(self)) }
    }

    fn main_loop(
        mut self,
        handle: &GenServerHandle<Self>,
        rx: &mut mpsc::Receiver<GenServerInMsg<Self>>,
        system_rx: &mut mpsc::Receiver<SystemMessage>,
    ) -> impl Future<Output = Self> + Send {
        async {
            loop {
                if !self.receive(handle, rx, system_rx).await {
                    break;
                }
            }
            tracing::trace!("Stopping GenServer");
            self
        }
    }

    fn receive(
        &mut self,
        handle: &GenServerHandle<Self>,
        rx: &mut mpsc::Receiver<GenServerInMsg<Self>>,
        system_rx: &mut mpsc::Receiver<SystemMessage>,
    ) -> impl Future<Output = bool> + Send {
        async move {
            // Use futures::select_biased! to prioritize system messages
            // We pin both futures inline
            let system_fut = pin!(system_rx.recv());
            let message_fut = pin!(rx.recv());

            // Select with bias towards system messages
            futures::select_biased! {
                system_msg = system_fut.fuse() => {
                    match system_msg {
                        Some(msg) => {
                            match AssertUnwindSafe(self.handle_info(msg, handle))
                                .catch_unwind()
                                .await
                            {
                                Ok(response) => match response {
                                    InfoResponse::NoReply => true,
                                    InfoResponse::Stop => false,
                                },
                                Err(error) => {
                                    tracing::error!("Error in handle_info: '{error:?}'");
                                    false
                                }
                            }
                        }
                        None => {
                            // System channel closed, continue with regular messages
                            true
                        }
                    }
                }

                message = message_fut.fuse() => {
                    match message {
                        Some(GenServerInMsg::Call { sender, message }) => {
                            let (keep_running, response) =
                                match AssertUnwindSafe(self.handle_call(message, handle))
                                    .catch_unwind()
                                    .await
                                {
                                    Ok(response) => match response {
                                        CallResponse::Reply(response) => (true, Ok(response)),
                                        CallResponse::Stop(response) => (false, Ok(response)),
                                    },
                                    Err(error) => {
                                        tracing::error!("Error in callback: '{error:?}'");
                                        (false, Err(GenServerError::Callback))
                                    }
                                };
                            // Send response back
                            if sender.send(response).is_err() {
                                tracing::error!(
                                    "GenServer failed to send response back, client must have died"
                                )
                            };
                            keep_running
                        }
                        Some(GenServerInMsg::Cast { message }) => {
                            match AssertUnwindSafe(self.handle_cast(message, handle))
                                .catch_unwind()
                                .await
                            {
                                Ok(response) => match response {
                                    CastResponse::NoReply => true,
                                    CastResponse::Stop => false,
                                },
                                Err(error) => {
                                    tracing::trace!("Error in callback: '{error:?}'");
                                    false
                                }
                            }
                        }
                        None => {
                            // Channel has been closed; won't receive further messages. Stop the server.
                            false
                        }
                    }
                }
            }
        }
    }

    fn handle_call(
        &mut self,
        _message: Self::CallMsg,
        _handle: &GenServerHandle<Self>,
    ) -> impl Future<Output = CallResponse<Self>> + Send {
        async { panic!("handle_call not implemented") }
    }

    fn handle_cast(
        &mut self,
        _message: Self::CastMsg,
        _handle: &GenServerHandle<Self>,
    ) -> impl Future<Output = CastResponse> + Send {
        async { panic!("handle_cast not implemented") }
    }

    /// Handle system messages (DOWN, EXIT, Timeout).
    ///
    /// This is called when:
    /// - A monitored process exits (receives `SystemMessage::Down`)
    /// - A linked process exits and trap_exit is enabled (receives `SystemMessage::Exit`)
    /// - A timer fires (receives `SystemMessage::Timeout`)
    ///
    /// Default implementation ignores all system messages.
    fn handle_info(
        &mut self,
        _message: SystemMessage,
        _handle: &GenServerHandle<Self>,
    ) -> impl Future<Output = InfoResponse> + Send {
        async { InfoResponse::NoReply }
    }

    /// Teardown function. It's called after the stop message is received.
    /// It can be overrided on implementations in case final steps are required,
    /// like closing streams, stopping timers, etc.
    fn teardown(
        self,
        _handle: &GenServerHandle<Self>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
        async { Ok(()) }
    }
}

/// Spawns a task that awaits on a future and sends a message to a GenServer
/// on completion.
/// This function returns a handle to the spawned task.
pub fn send_message_on<T, U>(
    handle: GenServerHandle<T>,
    future: U,
    message: T::CastMsg,
) -> JoinHandle<()>
where
    T: GenServer,
    U: Future + Send + 'static,
    <U as Future>::Output: Send,
{
    let cancelation_token = handle.cancellation_token();
    let mut handle_clone = handle.clone();
    let join_handle = rt::spawn(async move {
        let is_cancelled = pin!(cancelation_token.cancelled());
        let signal = pin!(future);
        match future::select(is_cancelled, signal).await {
            future::Either::Left(_) => tracing::debug!("GenServer stopped"),
            future::Either::Right(_) => {
                if let Err(e) = handle_clone.cast(message).await {
                    tracing::error!("Failed to send message: {e:?}")
                }
            }
        }
    });
    join_handle
}

#[cfg(debug_assertions)]
mod warn_on_block {
    use super::*;

    use std::time::Instant;
    use tracing::warn;

    pin_project_lite::pin_project! {
        pub struct WarnOnBlocking<F: Future>{
            #[pin]
            inner: F
        }
    }

    impl<F: Future> WarnOnBlocking<F> {
        pub fn new(inner: F) -> Self {
            Self { inner }
        }
    }

    impl<F: Future> Future for WarnOnBlocking<F> {
        type Output = F::Output;

        fn poll(
            self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Self::Output> {
            let type_id = std::any::type_name::<F>();
            let task_id = rt::task_id();
            let this = self.project();
            let now = Instant::now();
            let res = this.inner.poll(cx);
            let elapsed = now.elapsed();
            if elapsed > Duration::from_millis(10) {
                warn!(task = ?task_id, future = ?type_id, elapsed = ?elapsed, "Blocking operation detected");
            }
            res
        }
    }
}
