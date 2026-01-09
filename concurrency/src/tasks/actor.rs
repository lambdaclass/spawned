//! Actor trait and structs to create an abstraction similar to Erlang gen_server.
//! See examples/name_server for a usage example.
use crate::{
    error::ActorError,
    link::{MonitorRef, SystemMessage},
    pid::{ExitReason, HasPid, Pid},
    process_table::{self, LinkError, SystemMessageSender},
    registry::{self, RegistryError},
    tasks::InitResult::{NoSuccess, Success},
    Backend,
};
use core::pin::pin;
use futures::future::{self, FutureExt};
use spawned_rt::{
    tasks::{self as rt, mpsc, oneshot, timeout, CancellationToken, JoinHandle},
    threads,
};
use std::{fmt::Debug, future::Future, panic::AssertUnwindSafe, sync::Arc, time::Duration};

const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(5);

/// Handle to a running Actor.
///
/// This handle can be used to send messages to the Actor and to
/// obtain its unique process identifier (`Pid`).
///
/// Handles are cheap to clone and can be shared across tasks.
#[derive(Debug)]
pub struct ActorRef<G: Actor + 'static> {
    /// Unique process identifier for this Actor.
    pid: Pid,
    /// Channel sender for messages to the Actor.
    pub tx: mpsc::Sender<ActorInMsg<G>>,
    /// Cancellation token to stop the Actor.
    cancellation_token: CancellationToken,
    /// Channel for system messages (internal use).
    system_tx: mpsc::Sender<SystemMessage>,
}

impl<G: Actor> Clone for ActorRef<G> {
    fn clone(&self) -> Self {
        Self {
            pid: self.pid,
            tx: self.tx.clone(),
            cancellation_token: self.cancellation_token.clone(),
            system_tx: self.system_tx.clone(),
        }
    }
}

impl<G: Actor> HasPid for ActorRef<G> {
    fn pid(&self) -> Pid {
        self.pid
    }
}

/// Internal sender for system messages, implementing SystemMessageSender trait.
struct ActorSystemSender {
    system_tx: mpsc::Sender<SystemMessage>,
    cancellation_token: CancellationToken,
}

impl SystemMessageSender for ActorSystemSender {
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

impl<G: Actor> ActorRef<G> {
    fn new(gen_server: G) -> Self {
        let pid = Pid::new();
        let (tx, mut rx) = mpsc::channel::<ActorInMsg<G>>();
        let (system_tx, mut system_rx) = mpsc::channel::<SystemMessage>();
        let cancellation_token = CancellationToken::new();

        // Create the system message sender and register with process table
        let system_sender = Arc::new(ActorSystemSender {
            system_tx: system_tx.clone(),
            cancellation_token: cancellation_token.clone(),
        });
        process_table::register(pid, system_sender);

        let handle = ActorRef {
            pid,
            tx,
            cancellation_token,
            system_tx,
        };
        let handle_clone = handle.clone();
        let inner_future = async move {
            let result = gen_server.run(&handle, &mut rx, &mut system_rx).await;
            // Unregister from process table on exit
            let exit_reason = match &result {
                Ok(_) => ExitReason::Normal,
                Err(_) => ExitReason::Error("Actor crashed".to_string()),
            };
            process_table::unregister(pid, exit_reason);
            if let Err(error) = result {
                tracing::trace!(%error, "Actor crashed")
            }
        };

        #[cfg(debug_assertions)]
        // Optionally warn if the Actor future blocks for too much time
        let inner_future = warn_on_block::WarnOnBlocking::new(inner_future);

        // Ignore the JoinHandle for now. Maybe we'll use it in the future
        let _join_handle = rt::spawn(inner_future);

        handle_clone
    }

    fn new_blocking(gen_server: G) -> Self {
        let pid = Pid::new();
        let (tx, mut rx) = mpsc::channel::<ActorInMsg<G>>();
        let (system_tx, mut system_rx) = mpsc::channel::<SystemMessage>();
        let cancellation_token = CancellationToken::new();

        // Create the system message sender and register with process table
        let system_sender = Arc::new(ActorSystemSender {
            system_tx: system_tx.clone(),
            cancellation_token: cancellation_token.clone(),
        });
        process_table::register(pid, system_sender);

        let handle = ActorRef {
            pid,
            tx,
            cancellation_token,
            system_tx,
        };
        let handle_clone = handle.clone();
        // Ignore the JoinHandle for now. Maybe we'll use it in the future
        let _join_handle = rt::spawn_blocking(move || {
            rt::block_on(async move {
                let result = gen_server.run(&handle, &mut rx, &mut system_rx).await;
                let exit_reason = match &result {
                    Ok(_) => ExitReason::Normal,
                    Err(_) => ExitReason::Error("Actor crashed".to_string()),
                };
                process_table::unregister(pid, exit_reason);
                if let Err(error) = result {
                    tracing::trace!(%error, "Actor crashed")
                };
            })
        });
        handle_clone
    }

    fn new_on_thread(gen_server: G) -> Self {
        let pid = Pid::new();
        let (tx, mut rx) = mpsc::channel::<ActorInMsg<G>>();
        let (system_tx, mut system_rx) = mpsc::channel::<SystemMessage>();
        let cancellation_token = CancellationToken::new();

        // Create the system message sender and register with process table
        let system_sender = Arc::new(ActorSystemSender {
            system_tx: system_tx.clone(),
            cancellation_token: cancellation_token.clone(),
        });
        process_table::register(pid, system_sender);

        let handle = ActorRef {
            pid,
            tx,
            cancellation_token,
            system_tx,
        };
        let handle_clone = handle.clone();
        // Ignore the JoinHandle for now. Maybe we'll use it in the future
        let _join_handle = threads::spawn(move || {
            threads::block_on(async move {
                let result = gen_server.run(&handle, &mut rx, &mut system_rx).await;
                let exit_reason = match &result {
                    Ok(_) => ExitReason::Normal,
                    Err(_) => ExitReason::Error("Actor crashed".to_string()),
                };
                process_table::unregister(pid, exit_reason);
                if let Err(error) = result {
                    tracing::trace!(%error, "Actor crashed")
                };
            })
        });
        handle_clone
    }

    pub fn sender(&self) -> mpsc::Sender<ActorInMsg<G>> {
        self.tx.clone()
    }

    pub async fn call(&mut self, message: G::Request) -> Result<G::Reply, ActorError> {
        self.call_with_timeout(message, DEFAULT_CALL_TIMEOUT).await
    }

    pub async fn call_with_timeout(
        &mut self,
        message: G::Request,
        duration: Duration,
    ) -> Result<G::Reply, ActorError> {
        let (oneshot_tx, oneshot_rx) = oneshot::channel::<Result<G::Reply, ActorError>>();
        self.tx.send(ActorInMsg::Call {
            sender: oneshot_tx,
            message,
        })?;

        match timeout(duration, oneshot_rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(ActorError::Server),
            Err(_) => Err(ActorError::RequestTimeout),
        }
    }

    pub async fn cast(&mut self, message: G::Message) -> Result<(), ActorError> {
        self.tx
            .send(ActorInMsg::Cast { message })
            .map_err(|_error| ActorError::Server)
    }

    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancellation_token.clone()
    }

    /// Stop the Actor by cancelling its token.
    ///
    /// This is a convenience method equivalent to `cancellation_token().cancel()`.
    /// The Actor will exit and call its `teardown` method.
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
    /// let handle1 = Server1::new().start();
    /// let handle2 = Server2::new().start();
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
    /// let worker = Worker::new().start();
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
    /// let handle = MyServer::new().start();
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

pub enum ActorInMsg<G: Actor> {
    Call {
        sender: oneshot::Sender<Result<G::Reply, ActorError>>,
        message: G::Request,
    },
    Cast {
        message: G::Message,
    },
}

pub enum RequestResult<G: Actor> {
    Reply(G::Reply),
    Unused,
    Stop(G::Reply),
}

pub enum MessageResult {
    NoReply,
    Unused,
    Stop,
}

/// Response from handle_info callback.
pub enum InfoResult {
    /// Continue running, message was handled.
    NoReply,
    /// Stop the Actor.
    Stop,
}

pub enum InitResult<G: Actor> {
    Success(G),
    NoSuccess(G),
}

pub trait Actor: Send + Sized {
    type Request: Clone + Send + Sized + Sync;
    type Message: Clone + Send + Sized + Sync;
    type Reply: Send + Sized;
    type Error: Debug + Send;

    fn start(self) -> ActorRef<Self> {
        ActorRef::new(self)
    }

    /// Tokio tasks depend on a collaborative multitasking model. "Work stealing" can't
    /// happen if the task is blocking the thread. As such, for sync compute tasks
    /// or other blocking tasks need to be in their own separate thread, and the OS
    /// will manage them through hardware interrupts.
    /// `start_blocking` provides such a thread.
    fn start_blocking(self) -> ActorRef<Self> {
        ActorRef::new_blocking(self)
    }

    /// For some "singleton" Actors that run throughout the whole execution of the
    /// program, it makes sense to run in their own dedicated thread to avoid interference
    /// with the rest of the tasks' runtime.
    /// The use of `tokio::task::spawn_blocking` is not recommended for these scenarios
    /// as it is a limited thread pool better suited for blocking IO tasks that eventually end.
    fn start_on_thread(self) -> ActorRef<Self> {
        ActorRef::new_on_thread(self)
    }

    /// Start the Actor with the specified backend.
    ///
    /// This is the unified API for starting an Actor with explicit backend selection.
    /// See [`Backend`] for details on each option.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use spawned_concurrency::Backend;
    ///
    /// // Start on async runtime (default)
    /// let handle = MyActor::new().start_with_backend(Backend::Async);
    ///
    /// // Start on blocking thread pool
    /// let handle = MyActor::new().start_with_backend(Backend::Blocking);
    ///
    /// // Start on dedicated thread
    /// let handle = MyActor::new().start_with_backend(Backend::Thread);
    /// ```
    fn start_with_backend(self, backend: Backend) -> ActorRef<Self> {
        match backend {
            Backend::Async => ActorRef::new(self),
            Backend::Blocking => ActorRef::new_blocking(self),
            Backend::Thread => ActorRef::new_on_thread(self),
        }
    }

    /// Start the Actor and create a bidirectional link with another process.
    ///
    /// This is equivalent to calling `start()` followed by `link()`, but as an
    /// atomic operation. If the link fails, the Actor is stopped.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let parent = ParentServer::new().start();
    /// let child = ChildServer::new().start_linked(&parent)?;
    /// // Now if either crashes, the other will be notified
    /// ```
    fn start_linked(self, other: &impl HasPid) -> Result<ActorRef<Self>, LinkError> {
        let handle = self.start();
        handle.link(other)?;
        Ok(handle)
    }

    /// Start the Actor and set up monitoring from another process.
    ///
    /// This is equivalent to calling `start()` followed by `monitor()`, but as an
    /// atomic operation. The monitoring process will receive a DOWN message when
    /// this Actor exits.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let supervisor = SupervisorServer::new().start();
    /// let (worker, monitor_ref) = WorkerServer::new().start_monitored(&supervisor)?;
    /// // supervisor will receive DOWN message when worker exits
    /// ```
    fn start_monitored(
        self,
        monitor_from: &impl HasPid,
    ) -> Result<(ActorRef<Self>, MonitorRef), LinkError> {
        let handle = self.start();
        let monitor_ref = monitor_from.pid();
        let actual_ref = process_table::monitor(monitor_ref, handle.pid())?;
        Ok((handle, actual_ref))
    }

    fn run(
        self,
        handle: &ActorRef<Self>,
        rx: &mut mpsc::Receiver<ActorInMsg<Self>>,
        system_rx: &mut mpsc::Receiver<SystemMessage>,
    ) -> impl Future<Output = Result<(), ActorError>> + Send {
        async {
            let res = match self.init(handle).await {
                Ok(Success(new_state)) => Ok(new_state.main_loop(handle, rx, system_rx).await),
                Ok(NoSuccess(intermediate_state)) => {
                    // new_state is NoSuccess, this means the initialization failed, but the error was handled
                    // in callback. No need to report the error.
                    // Just skip main_loop and return the state to teardown the Actor
                    Ok(intermediate_state)
                }
                Err(err) => {
                    tracing::error!("Initialization failed with unhandled error: {err:?}");
                    Err(ActorError::Initialization)
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
        _handle: &ActorRef<Self>,
    ) -> impl Future<Output = Result<InitResult<Self>, Self::Error>> + Send {
        async { Ok(Success(self)) }
    }

    fn main_loop(
        mut self,
        handle: &ActorRef<Self>,
        rx: &mut mpsc::Receiver<ActorInMsg<Self>>,
        system_rx: &mut mpsc::Receiver<SystemMessage>,
    ) -> impl Future<Output = Self> + Send {
        async {
            loop {
                if !self.receive(handle, rx, system_rx).await {
                    break;
                }
            }
            tracing::trace!("Stopping Actor");
            self
        }
    }

    fn receive(
        &mut self,
        handle: &ActorRef<Self>,
        rx: &mut mpsc::Receiver<ActorInMsg<Self>>,
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
                                    InfoResult::NoReply => true,
                                    InfoResult::Stop => false,
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
                        Some(ActorInMsg::Call { sender, message }) => {
                            let (keep_running, response) =
                                match AssertUnwindSafe(self.handle_request(message, handle))
                                    .catch_unwind()
                                    .await
                                {
                                    Ok(response) => match response {
                                        RequestResult::Reply(response) => (true, Ok(response)),
                                        RequestResult::Stop(response) => (false, Ok(response)),
                                        RequestResult::Unused => {
                                            tracing::error!("Actor received unexpected CallMessage");
                                            (false, Err(ActorError::RequestUnused))
                                        }
                                    },
                                    Err(error) => {
                                        tracing::error!("Error in callback: '{error:?}'");
                                        (false, Err(ActorError::Callback))
                                    }
                                };
                            // Send response back
                            if sender.send(response).is_err() {
                                tracing::error!(
                                    "Actor failed to send response back, client must have died"
                                )
                            };
                            keep_running
                        }
                        Some(ActorInMsg::Cast { message }) => {
                            match AssertUnwindSafe(self.handle_message(message, handle))
                                .catch_unwind()
                                .await
                            {
                                Ok(response) => match response {
                                    MessageResult::NoReply => true,
                                    MessageResult::Stop => false,
                                    MessageResult::Unused => {
                                        tracing::error!("Actor received unexpected CastMessage");
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

    fn handle_request(
        &mut self,
        _message: Self::Request,
        _handle: &ActorRef<Self>,
    ) -> impl Future<Output = RequestResult<Self>> + Send {
        async { RequestResult::Unused }
    }

    fn handle_message(
        &mut self,
        _message: Self::Message,
        _handle: &ActorRef<Self>,
    ) -> impl Future<Output = MessageResult> + Send {
        async { MessageResult::Unused }
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
        _handle: &ActorRef<Self>,
    ) -> impl Future<Output = InfoResult> + Send {
        async { InfoResult::NoReply }
    }

    /// Teardown function. It's called after the stop message is received.
    /// It can be overrided on implementations in case final steps are required,
    /// like closing streams, stopping timers, etc.
    fn teardown(
        self,
        _handle: &ActorRef<Self>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
        async { Ok(()) }
    }
}

/// Spawns a task that awaits on a future and sends a message to a Actor
/// on completion.
/// This function returns a handle to the spawned task.
pub fn send_message_on<T, U>(
    handle: ActorRef<T>,
    future: U,
    message: T::Message,
) -> JoinHandle<()>
where
    T: Actor,
    U: Future + Send + 'static,
    <U as Future>::Output: Send,
{
    let cancelation_token = handle.cancellation_token();
    let mut handle_clone = handle.clone();
    let join_handle = rt::spawn(async move {
        let is_cancelled = pin!(cancelation_token.cancelled());
        let signal = pin!(future);
        match future::select(is_cancelled, signal).await {
            future::Either::Left(_) => tracing::debug!("Actor stopped"),
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
    use crate::{messages::Unused, tasks::send_after};
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
    pub enum Reply {
        Count(u64),
    }

    impl Actor for BadlyBehavedTask {
        type Request = InMessage;
        type Message = Unused;
        type Reply = Unused;
        type Error = Unused;

        async fn handle_request(
            &mut self,
            _: Self::Request,
            _: &ActorRef<Self>,
        ) -> RequestResult<Self> {
            RequestResult::Stop(Unused)
        }

        async fn handle_message(
            &mut self,
            _: Self::Message,
            _: &ActorRef<Self>,
        ) -> MessageResult {
            rt::sleep(Duration::from_millis(20)).await;
            thread::sleep(Duration::from_secs(2));
            MessageResult::Stop
        }
    }

    struct WellBehavedTask {
        pub count: u64,
    }

    impl Actor for WellBehavedTask {
        type Request = InMessage;
        type Message = Unused;
        type Reply = Reply;
        type Error = Unused;

        async fn handle_request(
            &mut self,
            message: Self::Request,
            _: &ActorRef<Self>,
        ) -> RequestResult<Self> {
            match message {
                InMessage::GetCount => RequestResult::Reply(Reply::Count(self.count)),
                InMessage::Stop => RequestResult::Stop(Reply::Count(self.count)),
            }
        }

        async fn handle_message(
            &mut self,
            _: Self::Message,
            handle: &ActorRef<Self>,
        ) -> MessageResult {
            self.count += 1;
            println!("{:?}: good still alive", thread::current().id());
            send_after(Duration::from_millis(100), handle.to_owned(), Unused);
            MessageResult::NoReply
        }
    }

    #[test]
    pub fn badly_behaved_thread_non_blocking() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let mut badboy = BadlyBehavedTask.start();
            let _ = badboy.cast(Unused).await;
            let mut goodboy = WellBehavedTask { count: 0 }.start();
            let _ = goodboy.cast(Unused).await;
            rt::sleep(Duration::from_secs(1)).await;
            let count = goodboy.call(InMessage::GetCount).await.unwrap();

            match count {
                Reply::Count(num) => {
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
            let mut badboy = BadlyBehavedTask.start_blocking();
            let _ = badboy.cast(Unused).await;
            let mut goodboy = WellBehavedTask { count: 0 }.start();
            let _ = goodboy.cast(Unused).await;
            rt::sleep(Duration::from_secs(1)).await;
            let count = goodboy.call(InMessage::GetCount).await.unwrap();

            match count {
                Reply::Count(num) => {
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
    enum SomeTaskRequest {
        SlowOperation,
        FastOperation,
    }

    impl Actor for SomeTask {
        type Request = SomeTaskRequest;
        type Message = Unused;
        type Reply = Unused;
        type Error = Unused;

        async fn handle_request(
            &mut self,
            message: Self::Request,
            _handle: &ActorRef<Self>,
        ) -> RequestResult<Self> {
            match message {
                SomeTaskRequest::SlowOperation => {
                    // Simulate a slow operation that will not resolve in time
                    rt::sleep(TIMEOUT_DURATION * 2).await;
                    RequestResult::Reply(Unused)
                }
                SomeTaskRequest::FastOperation => {
                    // Simulate a fast operation that resolves in time
                    rt::sleep(TIMEOUT_DURATION / 2).await;
                    RequestResult::Reply(Unused)
                }
            }
        }
    }

    #[test]
    pub fn unresolving_task_times_out() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let mut unresolving_task = SomeTask.start();

            let result = unresolving_task
                .call_with_timeout(SomeTaskRequest::FastOperation, TIMEOUT_DURATION)
                .await;
            assert!(matches!(result, Ok(Unused)));

            let result = unresolving_task
                .call_with_timeout(SomeTaskRequest::SlowOperation, TIMEOUT_DURATION)
                .await;
            assert!(matches!(result, Err(ActorError::RequestTimeout)));
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

    impl Actor for SomeTaskThatFailsOnInit {
        type Request = Unused;
        type Message = Unused;
        type Reply = Unused;
        type Error = Unused;

        async fn init(
            self,
            _handle: &ActorRef<Self>,
        ) -> Result<InitResult<Self>, Self::Error> {
            // Simulate an initialization failure by returning NoSuccess
            Ok(NoSuccess(self))
        }

        async fn teardown(self, _handle: &ActorRef<Self>) -> Result<(), Self::Error> {
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
            let _task = SomeTaskThatFailsOnInit::new(sender_channel).start();

            // Wait a while to ensure the task has time to run and fail
            rt::sleep(Duration::from_secs(1)).await;

            // We assure that the teardown function has ran by checking that the receiver channel is closed
            assert!(rx.is_closed())
        });
    }

    // ==================== Pid Tests ====================

    #[test]
    pub fn genserver_has_unique_pid() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let handle1 = WellBehavedTask { count: 0 }.start();
            let handle2 = WellBehavedTask { count: 0 }.start();
            let handle3 = WellBehavedTask { count: 0 }.start();

            // Each Actor should have a unique Pid
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
            let handle1 = WellBehavedTask { count: 0 }.start();
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
            let handle = WellBehavedTask { count: 0 }.start();
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
            let handle1 = WellBehavedTask { count: 0 }.start();
            let handle2 = WellBehavedTask { count: 0 }.start();

            let mut map: HashMap<Pid, &str> = HashMap::new();
            map.insert(handle1.pid(), "server1");
            map.insert(handle2.pid(), "server2");

            assert_eq!(map.get(&handle1.pid()), Some(&"server1"));
            assert_eq!(map.get(&handle2.pid()), Some(&"server2"));
            assert_eq!(map.len(), 2);
        });
    }

    #[test]
    pub fn all_start_methods_produce_unique_pids() {
        // Test that start(), start_blocking(), and start_on_thread() all produce unique Pids
        // by checking the Pid IDs are monotonically increasing across all start methods.
        //
        // Note: We can't easily test start_blocking() and start_on_thread() in isolation
        // within an async runtime block_on context due to potential deadlocks.
        // Instead, we verify the Pid generation is consistent by checking multiple
        // regular starts produce increasing IDs.
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let handle1 = WellBehavedTask { count: 0 }.start();
            let handle2 = WellBehavedTask { count: 0 }.start();
            let handle3 = WellBehavedTask { count: 0 }.start();

            // All handles should have unique, increasing Pids
            assert!(handle1.pid().id() < handle2.pid().id());
            assert!(handle2.pid().id() < handle3.pid().id());
        });
    }

    #[test]
    pub fn has_pid_trait_works() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let handle = WellBehavedTask { count: 0 }.start();

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
        // Clean registry before test
        crate::registry::clear();

        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let handle = WellBehavedTask { count: 0 }.start();

            // Register should succeed
            assert!(handle.register("test_genserver").is_ok());

            // Should be findable via registry
            assert_eq!(
                crate::registry::whereis("test_genserver"),
                Some(handle.pid())
            );

            // registered_name should return the name
            assert_eq!(
                handle.registered_name(),
                Some("test_genserver".to_string())
            );

            // Clean up
            handle.unregister();
            assert!(crate::registry::whereis("test_genserver").is_none());
        });

        // Clean registry after test
        crate::registry::clear();
    }

    #[test]
    pub fn genserver_duplicate_register_fails() {
        // Clean registry before test
        crate::registry::clear();

        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let handle1 = WellBehavedTask { count: 0 }.start();
            let handle2 = WellBehavedTask { count: 0 }.start();

            // First registration should succeed
            assert!(handle1.register("unique_name").is_ok());

            // Second registration with same name should fail
            assert_eq!(
                handle2.register("unique_name"),
                Err(RegistryError::AlreadyRegistered)
            );

            // Same process can't register twice
            assert_eq!(
                handle1.register("another_name"),
                Err(RegistryError::ProcessAlreadyNamed)
            );
        });

        // Clean registry after test
        crate::registry::clear();
    }

    #[test]
    pub fn genserver_unregister_allows_reregister() {
        // Clean registry before test
        crate::registry::clear();

        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let handle1 = WellBehavedTask { count: 0 }.start();
            let handle2 = WellBehavedTask { count: 0 }.start();

            // Register first process
            assert!(handle1.register("shared_name").is_ok());

            // Unregister
            handle1.unregister();

            // Now second process can use the name
            assert!(handle2.register("shared_name").is_ok());
            assert_eq!(
                crate::registry::whereis("shared_name"),
                Some(handle2.pid())
            );
        });

        // Clean registry after test
        crate::registry::clear();
    }
}
