//! Actor trait and structs to create an abstraction similar to Erlang gen_server.
//! See examples/name_server for a usage example.
use crate::{
    error::ActorError,
    tasks::InitResult::{NoSuccess, Success},
};
use core::pin::pin;
use futures::future::{self, FutureExt as _};
use spawned_rt::{
    tasks::{self as rt, mpsc, oneshot, timeout, CancellationToken, JoinHandle},
    threads,
};
use std::{fmt::Debug, future::Future, panic::AssertUnwindSafe, time::Duration};

const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Execution backend for Actor.
///
/// Determines how the Actor's async loop is executed. Choose based on
/// the nature of your workload:
///
/// # Backend Comparison
///
/// | Backend | Execution Model | Best For | Limitations |
/// |---------|-----------------|----------|-------------|
/// | `Async` | Tokio task | Non-blocking I/O, async operations | Blocks runtime if sync code runs too long |
/// | `Blocking` | Tokio blocking pool | Short blocking operations (file I/O, DNS) | Shared pool with limited threads |
/// | `Thread` | Dedicated OS thread with own runtime | Long-running services, isolation from main runtime | Higher memory overhead per Actor |
///
/// **Note**: All backends use async internally. For fully synchronous code without any async
/// runtime, use [`threads::Actor`](crate::threads::Actor) instead.
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
/// - **Use when**: Your Actor does mostly async I/O (network, database)
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
/// - **Avoid when**: You need many Actors (each gets its own OS thread + runtime)
/// - **Note**: Still uses async internally (own runtime). For sync code, use `threads::Actor`
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Backend {
    /// Run on tokio async runtime (default).
    ///
    /// Best for non-blocking, async workloads. The Actor runs as a
    /// lightweight tokio task, enabling high concurrency with minimal overhead.
    ///
    /// **Warning**: If your `handle_request` or `handle_message` blocks synchronously
    /// (e.g., `std::thread::sleep`, CPU-heavy loops), it will block the entire
    /// tokio runtime thread, affecting other tasks.
    #[default]
    Async,

    /// Run on tokio's blocking thread pool.
    ///
    /// Use for Actors that perform blocking operations like:
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
    /// Use for Actors that:
    /// - Need isolation from the main tokio runtime
    /// - Are long-running singleton services
    /// - Should not compete with other tasks for runtime resources
    ///
    /// Each Actor gets its own thread with a separate tokio runtime,
    /// providing isolation from other async tasks. Higher memory overhead
    /// (~2MB stack per thread plus runtime overhead).
    ///
    /// **Note**: This still uses async internally. For fully synchronous code
    /// without any async runtime, use [`threads::Actor`](crate::threads::Actor).
    Thread,
}

#[derive(Debug)]
pub struct ActorRef<A: Actor + 'static> {
    pub tx: mpsc::Sender<ActorInMsg<A>>,
    /// Cancellation token to stop the Actor
    cancellation_token: CancellationToken,
}

impl<A: Actor> Clone for ActorRef<A> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            cancellation_token: self.cancellation_token.clone(),
        }
    }
}

impl<A: Actor> ActorRef<A> {
    fn new(actor: A) -> Self {
        let (tx, mut rx) = mpsc::channel::<ActorInMsg<A>>();
        let cancellation_token = CancellationToken::new();
        let handle = ActorRef {
            tx,
            cancellation_token,
        };
        let handle_clone = handle.clone();
        let inner_future = async move {
            if let Err(error) = actor.run(&handle, &mut rx).await {
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

    fn new_blocking(actor: A) -> Self {
        let (tx, mut rx) = mpsc::channel::<ActorInMsg<A>>();
        let cancellation_token = CancellationToken::new();
        let handle = ActorRef {
            tx,
            cancellation_token,
        };
        let handle_clone = handle.clone();
        // Ignore the JoinHandle for now. Maybe we'll use it in the future
        let _join_handle = rt::spawn_blocking(|| {
            rt::block_on(async move {
                if let Err(error) = actor.run(&handle, &mut rx).await {
                    tracing::trace!(%error, "Actor crashed")
                };
            })
        });
        handle_clone
    }

    fn new_on_thread(actor: A) -> Self {
        let (tx, mut rx) = mpsc::channel::<ActorInMsg<A>>();
        let cancellation_token = CancellationToken::new();
        let handle = ActorRef {
            tx,
            cancellation_token,
        };
        let handle_clone = handle.clone();
        // Ignore the JoinHandle for now. Maybe we'll use it in the future
        let _join_handle = threads::spawn(|| {
            threads::block_on(async move {
                if let Err(error) = actor.run(&handle, &mut rx).await {
                    tracing::trace!(%error, "Actor crashed")
                };
            })
        });
        handle_clone
    }

    pub fn sender(&self) -> mpsc::Sender<ActorInMsg<A>> {
        self.tx.clone()
    }

    pub async fn request(&mut self, message: A::Request) -> Result<A::Reply, ActorError> {
        self.request_with_timeout(message, DEFAULT_REQUEST_TIMEOUT)
            .await
    }

    pub async fn request_with_timeout(
        &mut self,
        message: A::Request,
        duration: Duration,
    ) -> Result<A::Reply, ActorError> {
        let (oneshot_tx, oneshot_rx) = oneshot::channel::<Result<A::Reply, ActorError>>();
        self.tx.send(ActorInMsg::Request {
            sender: oneshot_tx,
            message,
        })?;

        match timeout(duration, oneshot_rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(ActorError::Server),
            Err(_) => Err(ActorError::RequestTimeout),
        }
    }

    pub async fn send(&mut self, message: A::Message) -> Result<(), ActorError> {
        self.tx
            .send(ActorInMsg::Message { message })
            .map_err(|_error| ActorError::Server)
    }

    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancellation_token.clone()
    }
}

pub enum ActorInMsg<A: Actor> {
    Request {
        sender: oneshot::Sender<Result<A::Reply, ActorError>>,
        message: A::Request,
    },
    Message {
        message: A::Message,
    },
}

pub enum RequestResponse<A: Actor> {
    Reply(A::Reply),
    Unused,
    Stop(A::Reply),
}

pub enum MessageResponse {
    NoReply,
    Unused,
    Stop,
}

pub enum InitResult<A: Actor> {
    Success(A),
    NoSuccess(A),
}

pub trait Actor: Send + Sized {
    type Request: Clone + Send + Sized + Sync;
    type Message: Clone + Send + Sized + Sync;
    type Reply: Send + Sized;
    type Error: Debug + Send;

    /// Start the Actor with the default backend (Async).
    fn start(self) -> ActorRef<Self> {
        self.start_with_backend(Backend::default())
    }

    /// Start the Actor with the specified backend.
    ///
    /// # Arguments
    /// * `backend` - The execution backend to use:
    ///   - `Backend::Async` - Run on tokio async runtime (default, best for non-blocking workloads)
    ///   - `Backend::Blocking` - Run on tokio's blocking thread pool (for blocking operations)
    ///   - `Backend::Thread` - Run on a dedicated OS thread (for long-running blocking services)
    fn start_with_backend(self, backend: Backend) -> ActorRef<Self> {
        match backend {
            Backend::Async => ActorRef::new(self),
            Backend::Blocking => ActorRef::new_blocking(self),
            Backend::Thread => ActorRef::new_on_thread(self),
        }
    }

    fn run(
        self,
        handle: &ActorRef<Self>,
        rx: &mut mpsc::Receiver<ActorInMsg<Self>>,
    ) -> impl Future<Output = Result<(), ActorError>> + Send {
        async {
            let res = match self.init(handle).await {
                Ok(Success(new_state)) => Ok(new_state.main_loop(handle, rx).await),
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
    ) -> impl Future<Output = Self> + Send {
        async {
            loop {
                if !self.receive(handle, rx).await {
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
    ) -> impl Future<Output = bool> + Send {
        async move {
            let message = rx.recv().await;

            let keep_running = match message {
                Some(ActorInMsg::Request { sender, message }) => {
                    let (keep_running, response) =
                        match AssertUnwindSafe(self.handle_request(message, handle))
                            .catch_unwind()
                            .await
                        {
                            Ok(response) => match response {
                                RequestResponse::Reply(response) => (true, Ok(response)),
                                RequestResponse::Stop(response) => (false, Ok(response)),
                                RequestResponse::Unused => {
                                    tracing::error!("Actor received unexpected Request");
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
                Some(ActorInMsg::Message { message }) => {
                    match AssertUnwindSafe(self.handle_message(message, handle))
                        .catch_unwind()
                        .await
                    {
                        Ok(response) => match response {
                            MessageResponse::NoReply => true,
                            MessageResponse::Stop => false,
                            MessageResponse::Unused => {
                                tracing::error!("Actor received unexpected Message");
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

    fn handle_request(
        &mut self,
        _message: Self::Request,
        _handle: &ActorRef<Self>,
    ) -> impl Future<Output = RequestResponse<Self>> + Send {
        async { RequestResponse::Unused }
    }

    fn handle_message(
        &mut self,
        _message: Self::Message,
        _handle: &ActorRef<Self>,
    ) -> impl Future<Output = MessageResponse> + Send {
        async { MessageResponse::Unused }
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

/// Spawns a task that awaits on a future and sends a message to an Actor
/// on completion.
/// This function returns a handle to the spawned task.
pub fn send_message_on<T, U>(handle: ActorRef<T>, future: U, message: T::Message) -> JoinHandle<()>
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
                if let Err(e) = handle_clone.send(message).await {
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

    impl Actor for BadlyBehavedTask {
        type Request = InMessage;
        type Message = Unused;
        type Reply = Unused;
        type Error = Unused;

        async fn handle_request(
            &mut self,
            _: Self::Request,
            _: &ActorRef<Self>,
        ) -> RequestResponse<Self> {
            RequestResponse::Stop(Unused)
        }

        async fn handle_message(
            &mut self,
            _: Self::Message,
            _: &ActorRef<Self>,
        ) -> MessageResponse {
            rt::sleep(Duration::from_millis(20)).await;
            thread::sleep(Duration::from_secs(2));
            MessageResponse::Stop
        }
    }

    struct WellBehavedTask {
        pub count: u64,
    }

    impl Actor for WellBehavedTask {
        type Request = InMessage;
        type Message = Unused;
        type Reply = OutMsg;
        type Error = Unused;

        async fn handle_request(
            &mut self,
            message: Self::Request,
            _: &ActorRef<Self>,
        ) -> RequestResponse<Self> {
            match message {
                InMessage::GetCount => RequestResponse::Reply(OutMsg::Count(self.count)),
                InMessage::Stop => RequestResponse::Stop(OutMsg::Count(self.count)),
            }
        }

        async fn handle_message(
            &mut self,
            _: Self::Message,
            handle: &ActorRef<Self>,
        ) -> MessageResponse {
            self.count += 1;
            println!("{:?}: good still alive", thread::current().id());
            send_after(Duration::from_millis(100), handle.to_owned(), Unused);
            MessageResponse::NoReply
        }
    }

    const BLOCKING: Backend = Backend::Blocking;

    #[test]
    pub fn badly_behaved_thread_non_blocking() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let mut badboy = BadlyBehavedTask.start();
            let _ = badboy.send(Unused).await;
            let mut goodboy = WellBehavedTask { count: 0 }.start();
            let _ = goodboy.send(Unused).await;
            rt::sleep(Duration::from_secs(1)).await;
            let count = goodboy.request(InMessage::GetCount).await.unwrap();

            match count {
                OutMsg::Count(num) => {
                    assert_ne!(num, 10);
                }
            }
            goodboy.request(InMessage::Stop).await.unwrap();
        });
    }

    #[test]
    pub fn badly_behaved_thread() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let mut badboy = BadlyBehavedTask.start_with_backend(BLOCKING);
            let _ = badboy.send(Unused).await;
            let mut goodboy = WellBehavedTask { count: 0 }.start();
            let _ = goodboy.send(Unused).await;
            rt::sleep(Duration::from_secs(1)).await;
            let count = goodboy.request(InMessage::GetCount).await.unwrap();

            match count {
                OutMsg::Count(num) => {
                    assert_eq!(num, 10);
                }
            }
            goodboy.request(InMessage::Stop).await.unwrap();
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
        ) -> RequestResponse<Self> {
            match message {
                SomeTaskRequest::SlowOperation => {
                    // Simulate a slow operation that will not resolve in time
                    rt::sleep(TIMEOUT_DURATION * 2).await;
                    RequestResponse::Reply(Unused)
                }
                SomeTaskRequest::FastOperation => {
                    // Simulate a fast operation that resolves in time
                    rt::sleep(TIMEOUT_DURATION / 2).await;
                    RequestResponse::Reply(Unused)
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
                .request_with_timeout(SomeTaskRequest::FastOperation, TIMEOUT_DURATION)
                .await;
            assert!(matches!(result, Ok(Unused)));

            let result = unresolving_task
                .request_with_timeout(SomeTaskRequest::SlowOperation, TIMEOUT_DURATION)
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

        async fn init(self, _handle: &ActorRef<Self>) -> Result<InitResult<Self>, Self::Error> {
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

    /// Simple counter Actor for testing all backends
    struct Counter {
        count: u64,
    }

    #[derive(Clone)]
    enum CounterRequest {
        Get,
        Increment,
        Stop,
    }

    #[derive(Clone)]
    enum CounterMessage {
        Increment,
    }

    impl Actor for Counter {
        type Request = CounterRequest;
        type Message = CounterMessage;
        type Reply = u64;
        type Error = ();

        async fn handle_request(
            &mut self,
            message: Self::Request,
            _: &ActorRef<Self>,
        ) -> RequestResponse<Self> {
            match message {
                CounterRequest::Get => RequestResponse::Reply(self.count),
                CounterRequest::Increment => {
                    self.count += 1;
                    RequestResponse::Reply(self.count)
                }
                CounterRequest::Stop => RequestResponse::Stop(self.count),
            }
        }

        async fn handle_message(
            &mut self,
            message: Self::Message,
            _: &ActorRef<Self>,
        ) -> MessageResponse {
            match message {
                CounterMessage::Increment => {
                    self.count += 1;
                    MessageResponse::NoReply
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
            let result = counter.request(CounterRequest::Get).await.unwrap();
            assert_eq!(result, 0);

            let result = counter.request(CounterRequest::Increment).await.unwrap();
            assert_eq!(result, 1);

            // Test cast
            counter.send(CounterMessage::Increment).await.unwrap();
            rt::sleep(Duration::from_millis(10)).await; // Give time for cast to process

            let result = counter.request(CounterRequest::Get).await.unwrap();
            assert_eq!(result, 2);

            // Stop
            let final_count = counter.request(CounterRequest::Stop).await.unwrap();
            assert_eq!(final_count, 2);
        });
    }

    #[test]
    pub fn backend_blocking_handles_call_and_cast() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let mut counter = Counter { count: 0 }.start_with_backend(Backend::Blocking);

            // Test call
            let result = counter.request(CounterRequest::Get).await.unwrap();
            assert_eq!(result, 0);

            let result = counter.request(CounterRequest::Increment).await.unwrap();
            assert_eq!(result, 1);

            // Test cast
            counter.send(CounterMessage::Increment).await.unwrap();
            rt::sleep(Duration::from_millis(50)).await; // Give time for cast to process

            let result = counter.request(CounterRequest::Get).await.unwrap();
            assert_eq!(result, 2);

            // Stop
            let final_count = counter.request(CounterRequest::Stop).await.unwrap();
            assert_eq!(final_count, 2);
        });
    }

    #[test]
    pub fn backend_thread_handles_call_and_cast() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let mut counter = Counter { count: 0 }.start_with_backend(Backend::Thread);

            // Test call
            let result = counter.request(CounterRequest::Get).await.unwrap();
            assert_eq!(result, 0);

            let result = counter.request(CounterRequest::Increment).await.unwrap();
            assert_eq!(result, 1);

            // Test cast
            counter.send(CounterMessage::Increment).await.unwrap();
            rt::sleep(Duration::from_millis(50)).await; // Give time for cast to process

            let result = counter.request(CounterRequest::Get).await.unwrap();
            assert_eq!(result, 2);

            // Stop
            let final_count = counter.request(CounterRequest::Stop).await.unwrap();
            assert_eq!(final_count, 2);
        });
    }

    #[test]
    pub fn backend_thread_isolates_blocking_work() {
        // Similar to badly_behaved_thread but using Backend::Thread
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let mut badboy = BadlyBehavedTask.start_with_backend(Backend::Thread);
            let _ = badboy.send(Unused).await;
            let mut goodboy = WellBehavedTask { count: 0 }.start();
            let _ = goodboy.send(Unused).await;
            rt::sleep(Duration::from_secs(1)).await;
            let count = goodboy.request(InMessage::GetCount).await.unwrap();

            // goodboy should have run normally because badboy is on a separate thread
            match count {
                OutMsg::Count(num) => {
                    assert_eq!(num, 10);
                }
            }
            goodboy.request(InMessage::Stop).await.unwrap();
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
            async_counter.request(CounterRequest::Increment).await.unwrap();
            blocking_counter
                .request(CounterRequest::Increment)
                .await
                .unwrap();
            thread_counter
                .request(CounterRequest::Increment)
                .await
                .unwrap();

            // Verify each has independent state
            let async_val = async_counter.request(CounterRequest::Get).await.unwrap();
            let blocking_val = blocking_counter.request(CounterRequest::Get).await.unwrap();
            let thread_val = thread_counter.request(CounterRequest::Get).await.unwrap();

            assert_eq!(async_val, 1);
            assert_eq!(blocking_val, 101);
            assert_eq!(thread_val, 201);

            // Clean up
            async_counter.request(CounterRequest::Stop).await.unwrap();
            blocking_counter.request(CounterRequest::Stop).await.unwrap();
            thread_counter.request(CounterRequest::Stop).await.unwrap();
        });
    }

    #[test]
    pub fn backend_default_works_in_start() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            // Using Backend::default() should work the same as Backend::Async
            let mut counter = Counter { count: 42 }.start_with_backend(Backend::Async);

            let result = counter.request(CounterRequest::Get).await.unwrap();
            assert_eq!(result, 42);

            counter.request(CounterRequest::Stop).await.unwrap();
        });
    }
}
