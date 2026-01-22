//! GenServer trait and structs to create an abstraction similar to Erlang gen_server.
//! See examples/name_server for a usage example.
use crate::{
    error::GenServerError,
    tasks::InitResult::{NoSuccess, Success},
};
use core::pin::pin;
use futures::future::{self, FutureExt as _};
use spawned_rt::{
    tasks::{self as rt, mpsc, oneshot, timeout, CancellationToken, JoinHandle},
    threads,
};
use std::{fmt::Debug, future::Future, panic::AssertUnwindSafe, time::Duration};

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
/// | `Thread` | Dedicated OS thread with own runtime | Long-running services, isolation from main runtime | Higher memory overhead per GenServer |
///
/// **Note**: All backends use async internally. For fully synchronous code without any async
/// runtime, use [`threads::GenServer`](crate::threads::GenServer) instead.
///
/// # Examples
///
/// ```ignore
/// // For typical async workloads (HTTP handlers, database queries)
/// let handle = MyServer::new().start();
///
/// // For occasional blocking operations (file reads, external commands)
/// let handle = MyServer::new().start_with_backend(Backend::Blocking);
///
/// // For CPU-intensive or permanently blocking services
/// let handle = MyServer::new().start_with_backend(Backend::Thread);
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
/// - **Advantages**: Isolated from main runtime, dedicated thread won't affect other tasks
/// - **Use when**: Long-running singleton services that shouldn't share the main runtime
/// - **Avoid when**: You need many GenServers (each gets its own OS thread + runtime)
/// - **Note**: Still uses async internally (own runtime). For sync code, use `threads::GenServer`
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

    /// Run on a dedicated OS thread with its own async runtime.
    ///
    /// Use for GenServers that:
    /// - Need isolation from the main tokio runtime
    /// - Are long-running singleton services
    /// - Should not compete with other tasks for runtime resources
    ///
    /// Each GenServer gets its own thread with a separate tokio runtime,
    /// providing isolation from other async tasks. Higher memory overhead
    /// (~2MB stack per thread plus runtime overhead).
    ///
    /// **Note**: This still uses async internally. For fully synchronous code
    /// without any async runtime, use [`threads::GenServer`](crate::threads::GenServer).
    Thread,
}

#[derive(Debug)]
pub struct GenServerHandle<G: GenServer + 'static> {
    pub tx: mpsc::Sender<GenServerInMsg<G>>,
    /// Cancellation token to stop the GenServer
    cancellation_token: CancellationToken,
}

impl<G: GenServer> Clone for GenServerHandle<G> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            cancellation_token: self.cancellation_token.clone(),
        }
    }
}

impl<G: GenServer> GenServerHandle<G> {
    fn new(gen_server: G) -> Self {
        let (tx, mut rx) = mpsc::channel::<GenServerInMsg<G>>();
        let cancellation_token = CancellationToken::new();
        let handle = GenServerHandle {
            tx,
            cancellation_token,
        };
        let handle_clone = handle.clone();
        let inner_future = async move {
            if let Err(error) = gen_server.run(&handle, &mut rx).await {
                tracing::trace!(%error, "GenServer crashed")
            }
        };

        #[cfg(debug_assertions)]
        // Optionally warn if the GenServer future blocks for too much time
        let inner_future = warn_on_block::WarnOnBlocking::new(inner_future);

        // Ignore the JoinHandle for now. Maybe we'll use it in the future
        let _join_handle = rt::spawn(inner_future);

        handle_clone
    }

    fn new_blocking(gen_server: G) -> Self {
        let (tx, mut rx) = mpsc::channel::<GenServerInMsg<G>>();
        let cancellation_token = CancellationToken::new();
        let handle = GenServerHandle {
            tx,
            cancellation_token,
        };
        let handle_clone = handle.clone();
        // Ignore the JoinHandle for now. Maybe we'll use it in the future
        let _join_handle = rt::spawn_blocking(|| {
            rt::block_on(async move {
                if let Err(error) = gen_server.run(&handle, &mut rx).await {
                    tracing::trace!(%error, "GenServer crashed")
                };
            })
        });
        handle_clone
    }

    fn new_on_thread(gen_server: G) -> Self {
        let (tx, mut rx) = mpsc::channel::<GenServerInMsg<G>>();
        let cancellation_token = CancellationToken::new();
        let handle = GenServerHandle {
            tx,
            cancellation_token,
        };
        let handle_clone = handle.clone();
        // Ignore the JoinHandle for now. Maybe we'll use it in the future
        let _join_handle = threads::spawn(|| {
            threads::block_on(async move {
                if let Err(error) = gen_server.run(&handle, &mut rx).await {
                    tracing::trace!(%error, "GenServer crashed")
                };
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

pub enum InitResult<G: GenServer> {
    Success(G),
    NoSuccess(G),
}

pub trait GenServer: Send + Sized {
    type CallMsg: Clone + Send + Sized + Sync;
    type CastMsg: Clone + Send + Sized + Sync;
    type OutMsg: Send + Sized;
    type Error: Debug + Send;

    /// Start the GenServer with the default backend (Async).
    fn start(self) -> GenServerHandle<Self> {
        self.start_with_backend(Backend::default())
    }

    /// Start the GenServer with the specified backend.
    ///
    /// # Arguments
    /// * `backend` - The execution backend to use:
    ///   - `Backend::Async` - Run on tokio async runtime (default, best for non-blocking workloads)
    ///   - `Backend::Blocking` - Run on tokio's blocking thread pool (for blocking operations)
    ///   - `Backend::Thread` - Run on a dedicated OS thread (for long-running blocking services)
    fn start_with_backend(self, backend: Backend) -> GenServerHandle<Self> {
        match backend {
            Backend::Async => GenServerHandle::new(self),
            Backend::Blocking => GenServerHandle::new_blocking(self),
            Backend::Thread => GenServerHandle::new_on_thread(self),
        }
    }

    fn run(
        self,
        handle: &GenServerHandle<Self>,
        rx: &mut mpsc::Receiver<GenServerInMsg<Self>>,
    ) -> impl Future<Output = Result<(), GenServerError>> + Send {
        async {
            let res = match self.init(handle).await {
                Ok(Success(new_state)) => Ok(new_state.main_loop(handle, rx).await),
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
    ) -> impl Future<Output = Self> + Send {
        async {
            loop {
                if !self.receive(handle, rx).await {
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
    ) -> impl Future<Output = bool> + Send {
        async move {
            let message = rx.recv().await;

            let keep_running = match message {
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
            };
            keep_running
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

    const BLOCKING: Backend = Backend::Blocking;

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
            let mut badboy = BadlyBehavedTask.start_with_backend(BLOCKING);
            let _ = badboy.cast(Unused).await;
            let mut goodboy = WellBehavedTask { count: 0 }.start();
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
            let mut unresolving_task = SomeTask.start();

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
            let _task = SomeTaskThatFailsOnInit::new(sender_channel).start();

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
    #[allow(clippy::clone_on_copy)]
    pub fn backend_enum_is_copy_and_clone() {
        let backend = Backend::Async;
        let copied = backend; // Copy
        let cloned = backend.clone(); // Clone - intentionally testing Clone trait
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
            let mut counter = Counter { count: 0 }.start();

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
            let mut counter = Counter { count: 0 }.start_with_backend(Backend::Blocking);

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
            let mut counter = Counter { count: 0 }.start_with_backend(Backend::Thread);

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
            let mut badboy = BadlyBehavedTask.start_with_backend(Backend::Thread);
            let _ = badboy.cast(Unused).await;
            let mut goodboy = WellBehavedTask { count: 0 }.start();
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
            let mut async_counter = Counter { count: 0 }.start();
            let mut blocking_counter = Counter { count: 100 }.start_with_backend(Backend::Blocking);
            let mut thread_counter = Counter { count: 200 }.start_with_backend(Backend::Thread);

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
            let mut counter = Counter { count: 42 }.start_with_backend(Backend::Async);

            let result = counter.call(CounterCall::Get).await.unwrap();
            assert_eq!(result, 42);

            counter.call(CounterCall::Stop).await.unwrap();
        });
    }
}
