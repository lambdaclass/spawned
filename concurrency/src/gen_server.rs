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
    Unused,
    Stop(G::OutMsg),
}

pub enum CastResponse {
    NoReply,
    Unused,
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
                                        CallResponse::Unused => {
                                            tracing::error!("GenServer received unexpected CallMessage");
                                            (false, Err(GenServerError::CallMsgUnused))
                                        }
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
                                    CastResponse::Unused => {
                                        tracing::error!("GenServer received unexpected CastMessage");
                                        false
                                    }
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
        async { CallResponse::Unused }
    }

    fn handle_cast(
        &mut self,
        _message: Self::CastMsg,
        _handle: &GenServerHandle<Self>,
    ) -> impl Future<Output = CastResponse> + Send {
        async { CastResponse::Unused }
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

#[cfg(test)]
mod tests {

    use super::*;
    use crate::{messages::Unused, send_after};
    use std::{
        sync::{Arc, Mutex},
        thread,
        time::Duration,
    };

    struct BadlyBehavedTask;

    #[derive(Clone)]
    pub enum InMessage {
        GetCount,
        Stop,
    }
    #[derive(Clone)]
    pub enum OutMsg {
        Count(u64),
    }

    impl GenServer for BadlyBehavedTask {
        type CallMsg = InMessage;
        type CastMsg = Unused;
        type OutMsg = Unused;
        type Error = Unused;

        async fn handle_call(
            &mut self,
            _: Self::CallMsg,
            _: &GenServerHandle<Self>,
        ) -> CallResponse<Self> {
            CallResponse::Stop(Unused)
        }

        async fn handle_cast(
            &mut self,
            _: Self::CastMsg,
            _: &GenServerHandle<Self>,
        ) -> CastResponse {
            rt::sleep(Duration::from_millis(20)).await;
            thread::sleep(Duration::from_secs(2));
            CastResponse::Stop
        }
    }

    struct WellBehavedTask {
        pub count: u64,
    }

    impl GenServer for WellBehavedTask {
        type CallMsg = InMessage;
        type CastMsg = Unused;
        type OutMsg = OutMsg;
        type Error = Unused;

        async fn handle_call(
            &mut self,
            message: Self::CallMsg,
            _: &GenServerHandle<Self>,
        ) -> CallResponse<Self> {
            match message {
                InMessage::GetCount => CallResponse::Reply(OutMsg::Count(self.count)),
                InMessage::Stop => CallResponse::Stop(OutMsg::Count(self.count)),
            }
        }

        async fn handle_cast(
            &mut self,
            _: Self::CastMsg,
            handle: &GenServerHandle<Self>,
        ) -> CastResponse {
            self.count += 1;
            println!("{:?}: good still alive", thread::current().id());
            send_after(Duration::from_millis(100), handle.to_owned(), Unused);
            CastResponse::NoReply
        }
    }

    const ASYNC: Backend = Backend::Async;
    const BLOCKING: Backend = Backend::Blocking;

    #[test]
    pub fn badly_behaved_thread_non_blocking() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let mut badboy = BadlyBehavedTask.start(ASYNC);
            let _ = badboy.cast(Unused).await;
            let mut goodboy = WellBehavedTask { count: 0 }.start(ASYNC);
            let _ = goodboy.cast(Unused).await;
            rt::sleep(Duration::from_secs(1)).await;
            let count = goodboy.call(InMessage::GetCount).await.unwrap();

            match count {
                OutMsg::Count(num) => {
                    assert_ne!(num, 10);
                }
            }
            goodboy.call(InMessage::Stop).await.unwrap();
        });
    }

    #[test]
    pub fn badly_behaved_thread() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let mut badboy = BadlyBehavedTask.start(BLOCKING);
            let _ = badboy.cast(Unused).await;
            let mut goodboy = WellBehavedTask { count: 0 }.start(ASYNC);
            let _ = goodboy.cast(Unused).await;
            rt::sleep(Duration::from_secs(1)).await;
            let count = goodboy.call(InMessage::GetCount).await.unwrap();

            match count {
                OutMsg::Count(num) => {
                    assert_eq!(num, 10);
                }
            }
            goodboy.call(InMessage::Stop).await.unwrap();
        });
    }

    const TIMEOUT_DURATION: Duration = Duration::from_millis(100);

    #[derive(Debug, Default)]
    struct SomeTask;

    #[derive(Clone)]
    enum SomeTaskCallMsg {
        SlowOperation,
        FastOperation,
    }

    impl GenServer for SomeTask {
        type CallMsg = SomeTaskCallMsg;
        type CastMsg = Unused;
        type OutMsg = Unused;
        type Error = Unused;

        async fn handle_call(
            &mut self,
            message: Self::CallMsg,
            _handle: &GenServerHandle<Self>,
        ) -> CallResponse<Self> {
            match message {
                SomeTaskCallMsg::SlowOperation => {
                    // Simulate a slow operation that will not resolve in time
                    rt::sleep(TIMEOUT_DURATION * 2).await;
                    CallResponse::Reply(Unused)
                }
                SomeTaskCallMsg::FastOperation => {
                    // Simulate a fast operation that resolves in time
                    rt::sleep(TIMEOUT_DURATION / 2).await;
                    CallResponse::Reply(Unused)
                }
            }
        }
    }

    #[test]
    pub fn unresolving_task_times_out() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let mut unresolving_task = SomeTask.start(ASYNC);

            let result = unresolving_task
                .call_with_timeout(SomeTaskCallMsg::FastOperation, TIMEOUT_DURATION)
                .await;
            assert!(matches!(result, Ok(Unused)));

            let result = unresolving_task
                .call_with_timeout(SomeTaskCallMsg::SlowOperation, TIMEOUT_DURATION)
                .await;
            assert!(matches!(result, Err(GenServerError::CallTimeout)));
        });
    }

    struct SomeTaskThatFailsOnInit {
        sender_channel: Arc<Mutex<mpsc::Receiver<u8>>>,
    }

    impl SomeTaskThatFailsOnInit {
        pub fn new(sender_channel: Arc<Mutex<mpsc::Receiver<u8>>>) -> Self {
            Self { sender_channel }
        }
    }

    impl GenServer for SomeTaskThatFailsOnInit {
        type CallMsg = Unused;
        type CastMsg = Unused;
        type OutMsg = Unused;
        type Error = Unused;

        async fn init(
            self,
            _handle: &GenServerHandle<Self>,
        ) -> Result<InitResult<Self>, Self::Error> {
            // Simulate an initialization failure by returning NoSuccess
            Ok(NoSuccess(self))
        }

        async fn teardown(self, _handle: &GenServerHandle<Self>) -> Result<(), Self::Error> {
            self.sender_channel.lock().unwrap().close();
            Ok(())
        }
    }

    #[test]
    pub fn task_fails_with_intermediate_state() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let (rx, tx) = mpsc::channel::<u8>();
            let sender_channel = Arc::new(Mutex::new(tx));
            let _task = SomeTaskThatFailsOnInit::new(sender_channel).start(ASYNC);

            // Wait a while to ensure the task has time to run and fail
            rt::sleep(Duration::from_secs(1)).await;

            // We assure that the teardown function has ran by checking that the receiver channel is closed
            assert!(rx.is_closed())
        });
    }

    // ==================== Backend enum tests ====================

    #[test]
    pub fn backend_default_is_async() {
        assert_eq!(Backend::default(), Backend::Async);
    }

    #[test]
    pub fn backend_enum_is_copy_and_clone() {
        let backend = Backend::Async;
        let copied = backend; // Copy
        let cloned = backend.clone(); // Clone
        assert_eq!(backend, copied);
        assert_eq!(backend, cloned);
    }

    #[test]
    pub fn backend_enum_debug_format() {
        assert_eq!(format!("{:?}", Backend::Async), "Async");
        assert_eq!(format!("{:?}", Backend::Blocking), "Blocking");
        assert_eq!(format!("{:?}", Backend::Thread), "Thread");
    }

    #[test]
    pub fn backend_enum_equality() {
        assert_eq!(Backend::Async, Backend::Async);
        assert_eq!(Backend::Blocking, Backend::Blocking);
        assert_eq!(Backend::Thread, Backend::Thread);
        assert_ne!(Backend::Async, Backend::Blocking);
        assert_ne!(Backend::Async, Backend::Thread);
        assert_ne!(Backend::Blocking, Backend::Thread);
    }

    // ==================== Backend functionality tests ====================

    /// Simple counter GenServer for testing all backends
    struct Counter {
        count: u64,
    }

    #[derive(Clone)]
    enum CounterCall {
        Get,
        Increment,
        Stop,
    }

    #[derive(Clone)]
    enum CounterCast {
        Increment,
    }

    impl GenServer for Counter {
        type CallMsg = CounterCall;
        type CastMsg = CounterCast;
        type OutMsg = u64;
        type Error = ();

        async fn handle_call(
            &mut self,
            message: Self::CallMsg,
            _: &GenServerHandle<Self>,
        ) -> CallResponse<Self> {
            match message {
                CounterCall::Get => CallResponse::Reply(self.count),
                CounterCall::Increment => {
                    self.count += 1;
                    CallResponse::Reply(self.count)
                }
                CounterCall::Stop => CallResponse::Stop(self.count),
            }
        }

        async fn handle_cast(
            &mut self,
            message: Self::CastMsg,
            _: &GenServerHandle<Self>,
        ) -> CastResponse {
            match message {
                CounterCast::Increment => {
                    self.count += 1;
                    CastResponse::NoReply
                }
            }
        }
    }

    #[test]
    pub fn backend_async_handles_call_and_cast() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let mut counter = Counter { count: 0 }.start(Backend::Async);

            // Test call
            let result = counter.call(CounterCall::Get).await.unwrap();
            assert_eq!(result, 0);

            let result = counter.call(CounterCall::Increment).await.unwrap();
            assert_eq!(result, 1);

            // Test cast
            counter.cast(CounterCast::Increment).await.unwrap();
            rt::sleep(Duration::from_millis(10)).await; // Give time for cast to process

            let result = counter.call(CounterCall::Get).await.unwrap();
            assert_eq!(result, 2);

            // Stop
            let final_count = counter.call(CounterCall::Stop).await.unwrap();
            assert_eq!(final_count, 2);
        });
    }

    #[test]
    pub fn backend_blocking_handles_call_and_cast() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let mut counter = Counter { count: 0 }.start(Backend::Blocking);

            // Test call
            let result = counter.call(CounterCall::Get).await.unwrap();
            assert_eq!(result, 0);

            let result = counter.call(CounterCall::Increment).await.unwrap();
            assert_eq!(result, 1);

            // Test cast
            counter.cast(CounterCast::Increment).await.unwrap();
            rt::sleep(Duration::from_millis(50)).await; // Give time for cast to process

            let result = counter.call(CounterCall::Get).await.unwrap();
            assert_eq!(result, 2);

            // Stop
            let final_count = counter.call(CounterCall::Stop).await.unwrap();
            assert_eq!(final_count, 2);
        });
    }

    #[test]
    pub fn backend_thread_handles_call_and_cast() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let mut counter = Counter { count: 0 }.start(Backend::Thread);

            // Test call
            let result = counter.call(CounterCall::Get).await.unwrap();
            assert_eq!(result, 0);

            let result = counter.call(CounterCall::Increment).await.unwrap();
            assert_eq!(result, 1);

            // Test cast
            counter.cast(CounterCast::Increment).await.unwrap();
            rt::sleep(Duration::from_millis(50)).await; // Give time for cast to process

            let result = counter.call(CounterCall::Get).await.unwrap();
            assert_eq!(result, 2);

            // Stop
            let final_count = counter.call(CounterCall::Stop).await.unwrap();
            assert_eq!(final_count, 2);
        });
    }

    #[test]
    pub fn backend_thread_isolates_blocking_work() {
        // Similar to badly_behaved_thread but using Backend::Thread
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let mut badboy = BadlyBehavedTask.start(Backend::Thread);
            let _ = badboy.cast(Unused).await;
            let mut goodboy = WellBehavedTask { count: 0 }.start(ASYNC);
            let _ = goodboy.cast(Unused).await;
            rt::sleep(Duration::from_secs(1)).await;
            let count = goodboy.call(InMessage::GetCount).await.unwrap();

            // goodboy should have run normally because badboy is on a separate thread
            match count {
                OutMsg::Count(num) => {
                    assert_eq!(num, 10);
                }
            }
            goodboy.call(InMessage::Stop).await.unwrap();
        });
    }

    #[test]
    pub fn multiple_backends_concurrent() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            // Start counters on all three backends
            let mut async_counter = Counter { count: 0 }.start(Backend::Async);
            let mut blocking_counter = Counter { count: 100 }.start(Backend::Blocking);
            let mut thread_counter = Counter { count: 200 }.start(Backend::Thread);

            // Increment each
            async_counter.call(CounterCall::Increment).await.unwrap();
            blocking_counter.call(CounterCall::Increment).await.unwrap();
            thread_counter.call(CounterCall::Increment).await.unwrap();

            // Verify each has independent state
            let async_val = async_counter.call(CounterCall::Get).await.unwrap();
            let blocking_val = blocking_counter.call(CounterCall::Get).await.unwrap();
            let thread_val = thread_counter.call(CounterCall::Get).await.unwrap();

            assert_eq!(async_val, 1);
            assert_eq!(blocking_val, 101);
            assert_eq!(thread_val, 201);

            // Clean up
            async_counter.call(CounterCall::Stop).await.unwrap();
            blocking_counter.call(CounterCall::Stop).await.unwrap();
            thread_counter.call(CounterCall::Stop).await.unwrap();
        });
    }

    #[test]
    pub fn backend_default_works_in_start() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            // Using Backend::default() should work the same as Backend::Async
            let mut counter = Counter { count: 42 }.start(Backend::default());

            let result = counter.call(CounterCall::Get).await.unwrap();
            assert_eq!(result, 42);

            counter.call(CounterCall::Stop).await.unwrap();
        });
    }

    // ==================== Property-based tests ====================

    use proptest::prelude::*;

    /// Strategy to generate random Backend variants
    fn backend_strategy() -> impl Strategy<Value = Backend> {
        prop_oneof![
            Just(Backend::Async),
            Just(Backend::Blocking),
            Just(Backend::Thread),
        ]
    }

    proptest! {
        /// Property: Counter GenServer preserves initial state
        #[test]
        fn prop_counter_preserves_initial_state(initial_count in 0u64..10000) {
            let runtime = rt::Runtime::new().unwrap();
            runtime.block_on(async move {
                let mut counter = Counter { count: initial_count }.start(Backend::Async);
                let result = counter.call(CounterCall::Get).await.unwrap();
                prop_assert_eq!(result, initial_count);
                counter.call(CounterCall::Stop).await.unwrap();
                Ok(())
            })?;
        }

        /// Property: N increments result in initial + N
        #[test]
        fn prop_increments_are_additive(
            initial_count in 0u64..1000,
            num_increments in 0usize..50
        ) {
            let runtime = rt::Runtime::new().unwrap();
            runtime.block_on(async move {
                let mut counter = Counter { count: initial_count }.start(Backend::Async);

                for _ in 0..num_increments {
                    counter.call(CounterCall::Increment).await.unwrap();
                }

                let final_count = counter.call(CounterCall::Get).await.unwrap();
                prop_assert_eq!(final_count, initial_count + num_increments as u64);
                counter.call(CounterCall::Stop).await.unwrap();
                Ok(())
            })?;
        }

        /// Property: Get is idempotent (multiple calls return same value)
        #[test]
        fn prop_get_is_idempotent(
            initial_count in 0u64..10000,
            num_gets in 1usize..10
        ) {
            let runtime = rt::Runtime::new().unwrap();
            runtime.block_on(async move {
                let mut counter = Counter { count: initial_count }.start(Backend::Async);

                let mut results = Vec::new();
                for _ in 0..num_gets {
                    results.push(counter.call(CounterCall::Get).await.unwrap());
                }

                // All Get calls should return the same value
                for result in &results {
                    prop_assert_eq!(*result, initial_count);
                }
                counter.call(CounterCall::Stop).await.unwrap();
                Ok(())
            })?;
        }

        /// Property: All backends produce working GenServers
        #[test]
        fn prop_all_backends_work(
            backend in backend_strategy(),
            initial_count in 0u64..1000
        ) {
            let runtime = rt::Runtime::new().unwrap();
            runtime.block_on(async move {
                let mut counter = Counter { count: initial_count }.start(backend);

                // Should be able to get initial value
                let result = counter.call(CounterCall::Get).await.unwrap();
                prop_assert_eq!(result, initial_count);

                // Should be able to increment
                let result = counter.call(CounterCall::Increment).await.unwrap();
                prop_assert_eq!(result, initial_count + 1);

                // Should be able to stop
                let final_result = counter.call(CounterCall::Stop).await.unwrap();
                prop_assert_eq!(final_result, initial_count + 1);
                Ok(())
            })?;
        }

        /// Property: Multiple GenServers maintain independent state
        #[test]
        fn prop_genservers_have_independent_state(
            count1 in 0u64..1000,
            count2 in 0u64..1000,
            increments1 in 0usize..20,
            increments2 in 0usize..20
        ) {
            let runtime = rt::Runtime::new().unwrap();
            runtime.block_on(async move {
                let mut counter1 = Counter { count: count1 }.start(Backend::Async);
                let mut counter2 = Counter { count: count2 }.start(Backend::Async);

                // Increment each independently
                for _ in 0..increments1 {
                    counter1.call(CounterCall::Increment).await.unwrap();
                }
                for _ in 0..increments2 {
                    counter2.call(CounterCall::Increment).await.unwrap();
                }

                // Verify independence
                let final1 = counter1.call(CounterCall::Get).await.unwrap();
                let final2 = counter2.call(CounterCall::Get).await.unwrap();

                prop_assert_eq!(final1, count1 + increments1 as u64);
                prop_assert_eq!(final2, count2 + increments2 as u64);

                counter1.call(CounterCall::Stop).await.unwrap();
                counter2.call(CounterCall::Stop).await.unwrap();
                Ok(())
            })?;
        }

        /// Property: Cast followed by Get reflects the cast
        #[test]
        fn prop_cast_eventually_processed(
            initial_count in 0u64..1000,
            num_casts in 1usize..20
        ) {
            let runtime = rt::Runtime::new().unwrap();
            runtime.block_on(async move {
                let mut counter = Counter { count: initial_count }.start(Backend::Async);

                // Send casts
                for _ in 0..num_casts {
                    counter.cast(CounterCast::Increment).await.unwrap();
                }

                // Give time for casts to process
                rt::sleep(Duration::from_millis(100)).await;

                // Verify all casts were processed
                let final_count = counter.call(CounterCall::Get).await.unwrap();
                prop_assert_eq!(final_count, initial_count + num_casts as u64);

                counter.call(CounterCall::Stop).await.unwrap();
                Ok(())
            })?;
        }
    }

    // ==================== Integration tests: Backend equivalence ====================
    // These tests verify that all backends behave identically

    /// Runs the same test logic on all three backends and collects results
    async fn run_on_all_backends<F, Fut, T>(test_fn: F) -> (T, T, T)
    where
        F: Fn(Backend) -> Fut,
        Fut: std::future::Future<Output = T>,
    {
        let async_result = test_fn(Backend::Async).await;
        let blocking_result = test_fn(Backend::Blocking).await;
        let thread_result = test_fn(Backend::Thread).await;
        (async_result, blocking_result, thread_result)
    }

    #[test]
    fn integration_all_backends_get_same_initial_value() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let (async_val, blocking_val, thread_val) = run_on_all_backends(|backend| async move {
                let mut counter = Counter { count: 42 }.start(backend);
                let result = counter.call(CounterCall::Get).await.unwrap();
                counter.call(CounterCall::Stop).await.unwrap();
                result
            })
            .await;

            assert_eq!(async_val, 42);
            assert_eq!(blocking_val, 42);
            assert_eq!(thread_val, 42);
            assert_eq!(async_val, blocking_val);
            assert_eq!(blocking_val, thread_val);
        });
    }

    #[test]
    fn integration_all_backends_increment_sequence_identical() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let test_sequence = |backend| async move {
                let mut counter = Counter { count: 0 }.start(backend);
                let mut results = Vec::new();

                // Perform 10 increments and record each result
                for _ in 0..10 {
                    let result = counter.call(CounterCall::Increment).await.unwrap();
                    results.push(result);
                }

                counter.call(CounterCall::Stop).await.unwrap();
                results
            };

            let (async_results, blocking_results, thread_results) =
                run_on_all_backends(test_sequence).await;

            // Expected sequence: 1, 2, 3, 4, 5, 6, 7, 8, 9, 10
            let expected: Vec<u64> = (1..=10).collect();
            assert_eq!(async_results, expected);
            assert_eq!(blocking_results, expected);
            assert_eq!(thread_results, expected);
        });
    }

    #[test]
    fn integration_all_backends_interleaved_call_cast_identical() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let test_interleaved = |backend| async move {
                let mut counter = Counter { count: 0 }.start(backend);

                // Increment via call
                counter.call(CounterCall::Increment).await.unwrap();
                // Increment via cast
                counter.cast(CounterCast::Increment).await.unwrap();
                // Wait for cast to process
                rt::sleep(Duration::from_millis(50)).await;
                // Increment via call again
                counter.call(CounterCall::Increment).await.unwrap();
                // Get final value
                let final_val = counter.call(CounterCall::Get).await.unwrap();
                counter.call(CounterCall::Stop).await.unwrap();
                final_val
            };

            let (async_val, blocking_val, thread_val) =
                run_on_all_backends(test_interleaved).await;

            assert_eq!(async_val, 3);
            assert_eq!(blocking_val, 3);
            assert_eq!(thread_val, 3);
        });
    }

    #[test]
    fn integration_all_backends_multiple_casts_identical() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let test_casts = |backend| async move {
                let mut counter = Counter { count: 0 }.start(backend);

                // Send 20 casts
                for _ in 0..20 {
                    counter.cast(CounterCast::Increment).await.unwrap();
                }

                // Wait for all casts to process
                rt::sleep(Duration::from_millis(100)).await;

                let final_val = counter.call(CounterCall::Get).await.unwrap();
                counter.call(CounterCall::Stop).await.unwrap();
                final_val
            };

            let (async_val, blocking_val, thread_val) = run_on_all_backends(test_casts).await;

            assert_eq!(async_val, 20);
            assert_eq!(blocking_val, 20);
            assert_eq!(thread_val, 20);
        });
    }

    // ==================== Integration tests: Cross-backend communication ====================

    /// GenServer that can call another GenServer
    struct Forwarder {
        target: GenServerHandle<Counter>,
    }

    #[derive(Clone)]
    enum ForwarderCall {
        GetFromTarget,
        IncrementTarget,
        Stop,
    }

    impl GenServer for Forwarder {
        type CallMsg = ForwarderCall;
        type CastMsg = Unused;
        type OutMsg = u64;
        type Error = ();

        async fn handle_call(
            &mut self,
            message: Self::CallMsg,
            _: &GenServerHandle<Self>,
        ) -> CallResponse<Self> {
            match message {
                ForwarderCall::GetFromTarget => {
                    let result = self.target.call(CounterCall::Get).await.unwrap();
                    CallResponse::Reply(result)
                }
                ForwarderCall::IncrementTarget => {
                    let result = self.target.call(CounterCall::Increment).await.unwrap();
                    CallResponse::Reply(result)
                }
                ForwarderCall::Stop => {
                    let _ = self.target.call(CounterCall::Stop).await;
                    CallResponse::Stop(0)
                }
            }
        }
    }

    #[test]
    fn integration_async_to_blocking_communication() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            // Counter runs on Blocking backend
            let counter = Counter { count: 100 }.start(Backend::Blocking);
            // Forwarder runs on Async backend, calls Counter
            let mut forwarder = Forwarder { target: counter }.start(Backend::Async);

            let result = forwarder.call(ForwarderCall::GetFromTarget).await.unwrap();
            assert_eq!(result, 100);

            let result = forwarder
                .call(ForwarderCall::IncrementTarget)
                .await
                .unwrap();
            assert_eq!(result, 101);

            forwarder.call(ForwarderCall::Stop).await.unwrap();
        });
    }

    #[test]
    fn integration_async_to_thread_communication() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            // Counter runs on Thread backend
            let counter = Counter { count: 200 }.start(Backend::Thread);
            // Forwarder runs on Async backend
            let mut forwarder = Forwarder { target: counter }.start(Backend::Async);

            let result = forwarder.call(ForwarderCall::GetFromTarget).await.unwrap();
            assert_eq!(result, 200);

            let result = forwarder
                .call(ForwarderCall::IncrementTarget)
                .await
                .unwrap();
            assert_eq!(result, 201);

            forwarder.call(ForwarderCall::Stop).await.unwrap();
        });
    }

    #[test]
    fn integration_blocking_to_thread_communication() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            // Counter runs on Thread backend
            let counter = Counter { count: 300 }.start(Backend::Thread);
            // Forwarder runs on Blocking backend
            let mut forwarder = Forwarder { target: counter }.start(Backend::Blocking);

            let result = forwarder.call(ForwarderCall::GetFromTarget).await.unwrap();
            assert_eq!(result, 300);

            let result = forwarder
                .call(ForwarderCall::IncrementTarget)
                .await
                .unwrap();
            assert_eq!(result, 301);

            forwarder.call(ForwarderCall::Stop).await.unwrap();
        });
    }

    #[test]
    fn integration_thread_to_async_communication() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            // Counter runs on Async backend
            let counter = Counter { count: 400 }.start(Backend::Async);
            // Forwarder runs on Thread backend
            let mut forwarder = Forwarder { target: counter }.start(Backend::Thread);

            let result = forwarder.call(ForwarderCall::GetFromTarget).await.unwrap();
            assert_eq!(result, 400);

            let result = forwarder
                .call(ForwarderCall::IncrementTarget)
                .await
                .unwrap();
            assert_eq!(result, 401);

            forwarder.call(ForwarderCall::Stop).await.unwrap();
        });
    }

    #[test]
    fn integration_all_backend_combinations_communicate() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let backends = [Backend::Async, Backend::Blocking, Backend::Thread];

            for &counter_backend in &backends {
                for &forwarder_backend in &backends {
                    let counter = Counter { count: 50 }.start(counter_backend);
                    let mut forwarder =
                        Forwarder { target: counter }.start(forwarder_backend);

                    // Test get
                    let result = forwarder.call(ForwarderCall::GetFromTarget).await.unwrap();
                    assert_eq!(
                        result, 50,
                        "Failed for {:?} -> {:?}",
                        forwarder_backend, counter_backend
                    );

                    // Test increment
                    let result = forwarder
                        .call(ForwarderCall::IncrementTarget)
                        .await
                        .unwrap();
                    assert_eq!(
                        result, 51,
                        "Failed for {:?} -> {:?}",
                        forwarder_backend, counter_backend
                    );

                    forwarder.call(ForwarderCall::Stop).await.unwrap();
                }
            }
        });
    }

    // ==================== Integration tests: Concurrent stress tests ====================

    #[test]
    fn integration_concurrent_operations_same_backend() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            for backend in [Backend::Async, Backend::Blocking, Backend::Thread] {
                let counter = Counter { count: 0 }.start(backend);

                // Spawn 10 concurrent tasks that each increment 10 times
                let handles: Vec<_> = (0..10)
                    .map(|_| {
                        let mut handle = counter.clone();
                        rt::spawn(async move {
                            for _ in 0..10 {
                                let _ = handle.call(CounterCall::Increment).await;
                            }
                        })
                    })
                    .collect();

                // Wait for all tasks
                for h in handles {
                    h.await.unwrap();
                }

                // Final count should be 100 (10 tasks * 10 increments)
                let mut handle = counter.clone();
                let final_count = handle.call(CounterCall::Get).await.unwrap();
                assert_eq!(
                    final_count, 100,
                    "Failed for backend {:?}",
                    backend
                );

                handle.call(CounterCall::Stop).await.unwrap();
            }
        });
    }

    #[test]
    fn integration_concurrent_mixed_call_cast() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            for backend in [Backend::Async, Backend::Blocking, Backend::Thread] {
                let counter = Counter { count: 0 }.start(backend);

                // Spawn tasks doing calls
                let call_handles: Vec<_> = (0..5)
                    .map(|_| {
                        let mut handle = counter.clone();
                        rt::spawn(async move {
                            for _ in 0..10 {
                                let _ = handle.call(CounterCall::Increment).await;
                            }
                        })
                    })
                    .collect();

                // Spawn tasks doing casts
                let cast_handles: Vec<_> = (0..5)
                    .map(|_| {
                        let mut handle = counter.clone();
                        rt::spawn(async move {
                            for _ in 0..10 {
                                let _ = handle.cast(CounterCast::Increment).await;
                            }
                        })
                    })
                    .collect();

                // Wait for all
                for h in call_handles {
                    h.await.unwrap();
                }
                for h in cast_handles {
                    h.await.unwrap();
                }

                // Give casts time to process
                rt::sleep(Duration::from_millis(100)).await;

                let mut handle = counter.clone();
                let final_count = handle.call(CounterCall::Get).await.unwrap();
                // 5 call tasks * 10 + 5 cast tasks * 10 = 100
                assert_eq!(final_count, 100, "Failed for backend {:?}", backend);

                handle.call(CounterCall::Stop).await.unwrap();
            }
        });
    }

    #[test]
    fn integration_multiple_genservers_different_backends_concurrent() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            // Create one GenServer on each backend
            let mut async_counter = Counter { count: 0 }.start(Backend::Async);
            let mut blocking_counter = Counter { count: 0 }.start(Backend::Blocking);
            let mut thread_counter = Counter { count: 0 }.start(Backend::Thread);

            // Spawn concurrent tasks for each
            let async_handle = {
                let mut c = async_counter.clone();
                rt::spawn(async move {
                    for _ in 0..50 {
                        c.call(CounterCall::Increment).await.unwrap();
                    }
                })
            };

            let blocking_handle = {
                let mut c = blocking_counter.clone();
                rt::spawn(async move {
                    for _ in 0..50 {
                        c.call(CounterCall::Increment).await.unwrap();
                    }
                })
            };

            let thread_handle = {
                let mut c = thread_counter.clone();
                rt::spawn(async move {
                    for _ in 0..50 {
                        c.call(CounterCall::Increment).await.unwrap();
                    }
                })
            };

            // Wait for all
            async_handle.await.unwrap();
            blocking_handle.await.unwrap();
            thread_handle.await.unwrap();

            // Each should have exactly 50
            assert_eq!(async_counter.call(CounterCall::Get).await.unwrap(), 50);
            assert_eq!(blocking_counter.call(CounterCall::Get).await.unwrap(), 50);
            assert_eq!(thread_counter.call(CounterCall::Get).await.unwrap(), 50);

            async_counter.call(CounterCall::Stop).await.unwrap();
            blocking_counter.call(CounterCall::Stop).await.unwrap();
            thread_counter.call(CounterCall::Stop).await.unwrap();
        });
    }

    // ==================== Integration tests: Init/Teardown behavior ====================

    struct InitTeardownTracker {
        init_called: Arc<Mutex<bool>>,
        teardown_called: Arc<Mutex<bool>>,
    }

    #[derive(Clone)]
    enum TrackerCall {
        CheckInit,
        Stop,
    }

    impl GenServer for InitTeardownTracker {
        type CallMsg = TrackerCall;
        type CastMsg = Unused;
        type OutMsg = bool;
        type Error = ();

        async fn init(
            self,
            _handle: &GenServerHandle<Self>,
        ) -> Result<InitResult<Self>, Self::Error> {
            *self.init_called.lock().unwrap() = true;
            Ok(Success(self))
        }

        async fn handle_call(
            &mut self,
            message: Self::CallMsg,
            _: &GenServerHandle<Self>,
        ) -> CallResponse<Self> {
            match message {
                TrackerCall::CheckInit => {
                    CallResponse::Reply(*self.init_called.lock().unwrap())
                }
                TrackerCall::Stop => CallResponse::Stop(true),
            }
        }

        async fn teardown(self, _handle: &GenServerHandle<Self>) -> Result<(), Self::Error> {
            *self.teardown_called.lock().unwrap() = true;
            Ok(())
        }
    }

    #[test]
    fn integration_init_called_on_all_backends() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            for backend in [Backend::Async, Backend::Blocking, Backend::Thread] {
                let init_called = Arc::new(Mutex::new(false));
                let teardown_called = Arc::new(Mutex::new(false));

                let mut tracker = InitTeardownTracker {
                    init_called: init_called.clone(),
                    teardown_called: teardown_called.clone(),
                }
                .start(backend);

                // Give time for init to run
                rt::sleep(Duration::from_millis(50)).await;

                let result = tracker.call(TrackerCall::CheckInit).await.unwrap();
                assert!(result, "Init not called for {:?}", backend);

                tracker.call(TrackerCall::Stop).await.unwrap();
            }
        });
    }

    #[test]
    fn integration_teardown_called_on_all_backends() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            for backend in [Backend::Async, Backend::Blocking, Backend::Thread] {
                let init_called = Arc::new(Mutex::new(false));
                let teardown_called = Arc::new(Mutex::new(false));

                let mut tracker = InitTeardownTracker {
                    init_called: init_called.clone(),
                    teardown_called: teardown_called.clone(),
                }
                .start(backend);

                tracker.call(TrackerCall::Stop).await.unwrap();

                // Give time for teardown to run
                rt::sleep(Duration::from_millis(100)).await;

                assert!(
                    *teardown_called.lock().unwrap(),
                    "Teardown not called for {:?}",
                    backend
                );
            }
        });
    }

    // ==================== Integration tests: Error handling equivalence ====================

    #[test]
    fn integration_channel_closed_behavior_identical() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            for backend in [Backend::Async, Backend::Blocking, Backend::Thread] {
                let mut counter = Counter { count: 0 }.start(backend);

                // Stop the server
                counter.call(CounterCall::Stop).await.unwrap();

                // Give time for shutdown
                rt::sleep(Duration::from_millis(50)).await;

                // Further calls should fail
                let result = counter.call(CounterCall::Get).await;
                assert!(
                    result.is_err(),
                    "Call after stop should fail for {:?}",
                    backend
                );
            }
        });
    }

    // ==================== Integration tests: State consistency ====================

    #[test]
    fn integration_large_state_operations_identical() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let test_large_operations = |backend| async move {
                let mut counter = Counter { count: 0 }.start(backend);

                // Perform 1000 increments
                for _ in 0..1000 {
                    counter.call(CounterCall::Increment).await.unwrap();
                }

                let final_val = counter.call(CounterCall::Get).await.unwrap();
                counter.call(CounterCall::Stop).await.unwrap();
                final_val
            };

            let (async_val, blocking_val, thread_val) =
                run_on_all_backends(test_large_operations).await;

            assert_eq!(async_val, 1000);
            assert_eq!(blocking_val, 1000);
            assert_eq!(thread_val, 1000);
        });
    }

    #[test]
    fn integration_alternating_operations_identical() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let test_alternating = |backend| async move {
                let mut counter = Counter { count: 100 }.start(backend);
                let mut results = Vec::new();

                // Alternate between get and increment
                for i in 0..20 {
                    if i % 2 == 0 {
                        results.push(counter.call(CounterCall::Get).await.unwrap());
                    } else {
                        results.push(counter.call(CounterCall::Increment).await.unwrap());
                    }
                }

                counter.call(CounterCall::Stop).await.unwrap();
                results
            };

            let (async_results, blocking_results, thread_results) =
                run_on_all_backends(test_alternating).await;

            // All backends should produce identical sequence
            assert_eq!(async_results, blocking_results);
            assert_eq!(blocking_results, thread_results);

            // Verify expected pattern: get returns current, increment returns new
            // Pattern: 100, 101, 101, 102, 102, 103, ...
            let expected: Vec<u64> = (0..20)
                .map(|i| {
                    if i % 2 == 0 {
                        100 + (i / 2) as u64
                    } else {
                        100 + (i / 2) as u64 + 1
                    }
                })
                .collect();
            assert_eq!(async_results, expected);
        });
    }

    // ==================== Pid Tests ====================

    #[test]
    pub fn genserver_has_unique_pid() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let handle1 = WellBehavedTask { count: 0 }.start(ASYNC);
            let handle2 = WellBehavedTask { count: 0 }.start(ASYNC);
            let handle3 = WellBehavedTask { count: 0 }.start(ASYNC);

            // Each GenServer should have a unique Pid
            assert_ne!(handle1.pid(), handle2.pid());
            assert_ne!(handle2.pid(), handle3.pid());
            assert_ne!(handle1.pid(), handle3.pid());

            // Pids should be monotonically increasing
            assert!(handle1.pid().id() < handle2.pid().id());
            assert!(handle2.pid().id() < handle3.pid().id());
        });
    }

    #[test]
    pub fn cloned_handle_has_same_pid() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let handle1 = WellBehavedTask { count: 0 }.start(ASYNC);
            let handle2 = handle1.clone();

            // Cloned handles should have the same Pid
            assert_eq!(handle1.pid(), handle2.pid());
            assert_eq!(handle1.pid().id(), handle2.pid().id());
        });
    }

    #[test]
    pub fn pid_display_format() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let handle = WellBehavedTask { count: 0 }.start(ASYNC);
            let pid = handle.pid();

            // Check display format is Erlang-like: <0.N>
            let display = format!("{}", pid);
            assert!(display.starts_with("<0."));
            assert!(display.ends_with(">"));

            // Check debug format
            let debug = format!("{:?}", pid);
            assert!(debug.starts_with("Pid("));
            assert!(debug.ends_with(")"));
        });
    }

    #[test]
    pub fn pid_can_be_used_as_hashmap_key() {
        use std::collections::HashMap;

        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let handle1 = WellBehavedTask { count: 0 }.start(ASYNC);
            let handle2 = WellBehavedTask { count: 0 }.start(ASYNC);

            let mut map: HashMap<Pid, &str> = HashMap::new();
            map.insert(handle1.pid(), "server1");
            map.insert(handle2.pid(), "server2");

            assert_eq!(map.get(&handle1.pid()), Some(&"server1"));
            assert_eq!(map.get(&handle2.pid()), Some(&"server2"));
            assert_eq!(map.len(), 2);
        });
    }

    #[test]
    pub fn all_backends_produce_unique_pids() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let handle1 = WellBehavedTask { count: 0 }.start(ASYNC);
            let handle2 = WellBehavedTask { count: 0 }.start(ASYNC);
            let handle3 = WellBehavedTask { count: 0 }.start(ASYNC);

            // All handles should have unique, increasing Pids
            assert!(handle1.pid().id() < handle2.pid().id());
            assert!(handle2.pid().id() < handle3.pid().id());
        });
    }

    #[test]
    pub fn has_pid_trait_works() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let handle = WellBehavedTask { count: 0 }.start(ASYNC);

            // Test that HasPid trait is implemented
            fn accepts_has_pid(p: &impl HasPid) -> Pid {
                p.pid()
            }

            let pid = accepts_has_pid(&handle);
            assert_eq!(pid, handle.pid());
        });
    }

    // ==================== Registry Tests ====================

    #[test]
    pub fn genserver_can_register() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let handle = WellBehavedTask { count: 0 }.start(ASYNC);
            // Use unique name based on PID to avoid conflicts with parallel tests
            let name = format!("test_genserver_{}", handle.pid().id());

            // Register should succeed
            assert!(handle.register(&name).is_ok());

            // Should be findable via registry
            assert_eq!(crate::registry::whereis(&name), Some(handle.pid()));

            // registered_name should return the name
            assert_eq!(handle.registered_name(), Some(name.clone()));

            // Clean up
            handle.unregister();
            assert!(crate::registry::whereis(&name).is_none());
        });
    }

    #[test]
    pub fn genserver_duplicate_register_fails() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let handle1 = WellBehavedTask { count: 0 }.start(ASYNC);
            let handle2 = WellBehavedTask { count: 0 }.start(ASYNC);
            // Use unique name based on PID to avoid conflicts with parallel tests
            let name = format!("unique_name_{}", handle1.pid().id());
            let another_name = format!("another_name_{}", handle1.pid().id());

            // First registration should succeed
            assert!(handle1.register(&name).is_ok());

            // Second registration with same name should fail
            assert_eq!(
                handle2.register(&name),
                Err(RegistryError::AlreadyRegistered)
            );

            // Same process can't register twice
            assert_eq!(
                handle1.register(&another_name),
                Err(RegistryError::ProcessAlreadyNamed)
            );

            // Clean up
            handle1.unregister();
        });
    }

    #[test]
    pub fn genserver_unregister_allows_reregister() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let handle1 = WellBehavedTask { count: 0 }.start(ASYNC);
            let handle2 = WellBehavedTask { count: 0 }.start(ASYNC);
            // Use unique name based on PID to avoid conflicts with parallel tests
            let name = format!("shared_name_{}", handle1.pid().id());

            // Register first process
            assert!(handle1.register(&name).is_ok());

            // Unregister
            handle1.unregister();

            // Now second process can use the name
            assert!(handle2.register(&name).is_ok());
            assert_eq!(crate::registry::whereis(&name), Some(handle2.pid()));

            // Clean up
            handle2.unregister();
        });
    }
}
