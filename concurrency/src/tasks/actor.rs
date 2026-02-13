use crate::error::ActorError;
use crate::message::Message;
use core::pin::pin;
use futures::future::{self, FutureExt as _};
use spawned_rt::{
    tasks::{self as rt, mpsc, oneshot, timeout, watch, CancellationToken, JoinHandle},
    threads,
};
use std::{fmt::Debug, future::Future, panic::AssertUnwindSafe, pin::Pin, sync::Arc, time::Duration};

const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// Backend
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Backend {
    #[default]
    Async,
    Blocking,
    Thread,
}

// ---------------------------------------------------------------------------
// Actor trait
// ---------------------------------------------------------------------------

pub trait Actor: Send + Sized + 'static {
    fn started(&mut self, _ctx: &Context<Self>) -> impl Future<Output = ()> + Send {
        async {}
    }

    fn stopped(&mut self, _ctx: &Context<Self>) -> impl Future<Output = ()> + Send {
        async {}
    }
}

// ---------------------------------------------------------------------------
// Handler trait (per-message, uses RPITIT — NOT object-safe, that's fine)
// ---------------------------------------------------------------------------

pub trait Handler<M: Message>: Actor {
    fn handle(
        &mut self,
        msg: M,
        ctx: &Context<Self>,
    ) -> impl Future<Output = M::Result> + Send;
}

// ---------------------------------------------------------------------------
// Envelope (type-erasure on the actor side)
// ---------------------------------------------------------------------------

trait Envelope<A: Actor>: Send {
    fn handle<'a>(
        self: Box<Self>,
        actor: &'a mut A,
        ctx: &'a Context<A>,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>;
}

struct MessageEnvelope<M: Message> {
    msg: M,
    tx: Option<oneshot::Sender<M::Result>>,
}

impl<A, M> Envelope<A> for MessageEnvelope<M>
where
    A: Actor + Handler<M>,
    M: Message,
{
    fn handle<'a>(
        self: Box<Self>,
        actor: &'a mut A,
        ctx: &'a Context<A>,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            let result = actor.handle(self.msg, ctx).await;
            if let Some(tx) = self.tx {
                let _ = tx.send(result);
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Context
// ---------------------------------------------------------------------------

pub struct Context<A: Actor> {
    sender: mpsc::Sender<Box<dyn Envelope<A> + Send>>,
    cancellation_token: CancellationToken,
}

impl<A: Actor> Clone for Context<A> {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            cancellation_token: self.cancellation_token.clone(),
        }
    }
}

impl<A: Actor> Debug for Context<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Context").finish_non_exhaustive()
    }
}

impl<A: Actor> Context<A> {
    pub fn from_ref(actor_ref: &ActorRef<A>) -> Self {
        Self {
            sender: actor_ref.sender.clone(),
            cancellation_token: actor_ref.cancellation_token.clone(),
        }
    }

    pub fn stop(&self) {
        self.cancellation_token.cancel();
    }

    pub fn send<M>(&self, msg: M) -> Result<(), ActorError>
    where
        A: Handler<M>,
        M: Message,
    {
        let envelope = MessageEnvelope { msg, tx: None };
        self.sender
            .send(Box::new(envelope))
            .map_err(|_| ActorError::ActorStopped)
    }

    pub fn request_raw<M>(&self, msg: M) -> Result<oneshot::Receiver<M::Result>, ActorError>
    where
        A: Handler<M>,
        M: Message,
    {
        let (tx, rx) = oneshot::channel();
        let envelope = MessageEnvelope {
            msg,
            tx: Some(tx),
        };
        self.sender
            .send(Box::new(envelope))
            .map_err(|_| ActorError::ActorStopped)?;
        Ok(rx)
    }

    pub async fn request<M>(&self, msg: M) -> Result<M::Result, ActorError>
    where
        A: Handler<M>,
        M: Message,
    {
        let rx = self.request_raw(msg)?;
        match timeout(DEFAULT_REQUEST_TIMEOUT, rx).await {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(_)) => Err(ActorError::ActorStopped),
            Err(_) => Err(ActorError::RequestTimeout),
        }
    }

    pub fn recipient<M>(&self) -> Recipient<M>
    where
        A: Handler<M>,
        M: Message,
    {
        Arc::new(self.clone())
    }

    pub(crate) fn cancellation_token(&self) -> CancellationToken {
        self.cancellation_token.clone()
    }
}

// Bridge: Context<A> implements Receiver<M> for any M that A handles
impl<A, M> Receiver<M> for Context<A>
where
    A: Actor + Handler<M>,
    M: Message,
{
    fn send(&self, msg: M) -> Result<(), ActorError> {
        Context::send(self, msg)
    }

    fn request_raw(&self, msg: M) -> Result<oneshot::Receiver<M::Result>, ActorError> {
        Context::request_raw(self, msg)
    }
}

// ---------------------------------------------------------------------------
// Receiver trait (object-safe) + Recipient alias
// ---------------------------------------------------------------------------

pub trait Receiver<M: Message>: Send + Sync {
    fn send(&self, msg: M) -> Result<(), ActorError>;
    fn request_raw(&self, msg: M) -> Result<oneshot::Receiver<M::Result>, ActorError>;
}

pub type Recipient<M> = Arc<dyn Receiver<M>>;

pub async fn request<M: Message>(
    recipient: &dyn Receiver<M>,
    msg: M,
    timeout_duration: Duration,
) -> Result<M::Result, ActorError> {
    let rx = recipient.request_raw(msg)?;
    match timeout(timeout_duration, rx).await {
        Ok(Ok(result)) => Ok(result),
        Ok(Err(_)) => Err(ActorError::ActorStopped),
        Err(_) => Err(ActorError::RequestTimeout),
    }
}

// ---------------------------------------------------------------------------
// ActorRef
// ---------------------------------------------------------------------------

pub struct ActorRef<A: Actor> {
    sender: mpsc::Sender<Box<dyn Envelope<A> + Send>>,
    cancellation_token: CancellationToken,
    completion_rx: watch::Receiver<bool>,
}

impl<A: Actor> Debug for ActorRef<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActorRef").finish_non_exhaustive()
    }
}

impl<A: Actor> Clone for ActorRef<A> {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            cancellation_token: self.cancellation_token.clone(),
            completion_rx: self.completion_rx.clone(),
        }
    }
}

impl<A: Actor> ActorRef<A> {
    pub fn send<M>(&self, msg: M) -> Result<(), ActorError>
    where
        A: Handler<M>,
        M: Message,
    {
        let envelope = MessageEnvelope { msg, tx: None };
        self.sender
            .send(Box::new(envelope))
            .map_err(|_| ActorError::ActorStopped)
    }

    pub fn request_raw<M>(&self, msg: M) -> Result<oneshot::Receiver<M::Result>, ActorError>
    where
        A: Handler<M>,
        M: Message,
    {
        let (tx, rx) = oneshot::channel();
        let envelope = MessageEnvelope {
            msg,
            tx: Some(tx),
        };
        self.sender
            .send(Box::new(envelope))
            .map_err(|_| ActorError::ActorStopped)?;
        Ok(rx)
    }

    pub async fn request<M>(&self, msg: M) -> Result<M::Result, ActorError>
    where
        A: Handler<M>,
        M: Message,
    {
        self.request_with_timeout(msg, DEFAULT_REQUEST_TIMEOUT).await
    }

    pub async fn request_with_timeout<M>(
        &self,
        msg: M,
        duration: Duration,
    ) -> Result<M::Result, ActorError>
    where
        A: Handler<M>,
        M: Message,
    {
        let rx = self.request_raw(msg)?;
        match timeout(duration, rx).await {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(_)) => Err(ActorError::ActorStopped),
            Err(_) => Err(ActorError::RequestTimeout),
        }
    }

    pub fn recipient<M>(&self) -> Recipient<M>
    where
        A: Handler<M>,
        M: Message,
    {
        Arc::new(self.clone())
    }

    pub fn context(&self) -> Context<A> {
        Context::from_ref(self)
    }

    pub async fn join(&self) {
        let mut rx = self.completion_rx.clone();
        while !*rx.borrow_and_update() {
            if rx.changed().await.is_err() {
                break;
            }
        }
    }
}

// Bridge: ActorRef<A> implements Receiver<M> for any M that A handles
impl<A, M> Receiver<M> for ActorRef<A>
where
    A: Actor + Handler<M>,
    M: Message,
{
    fn send(&self, msg: M) -> Result<(), ActorError> {
        ActorRef::send(self, msg)
    }

    fn request_raw(&self, msg: M) -> Result<oneshot::Receiver<M::Result>, ActorError> {
        ActorRef::request_raw(self, msg)
    }
}

// ---------------------------------------------------------------------------
// Actor startup + main loop
// ---------------------------------------------------------------------------

impl<A: Actor> ActorRef<A> {
    fn spawn(actor: A, backend: Backend) -> Self {
        let (tx, rx) = mpsc::channel::<Box<dyn Envelope<A> + Send>>();
        let cancellation_token = CancellationToken::new();
        let (completion_tx, completion_rx) = watch::channel(false);

        let actor_ref = ActorRef {
            sender: tx.clone(),
            cancellation_token: cancellation_token.clone(),
            completion_rx,
        };

        let ctx = Context {
            sender: tx,
            cancellation_token: cancellation_token.clone(),
        };

        let inner_future = async move {
            run_actor(actor, ctx, rx, cancellation_token).await;
            let _ = completion_tx.send(true);
        };

        match backend {
            Backend::Async => {
                #[cfg(debug_assertions)]
                let inner_future = warn_on_block::WarnOnBlocking::new(inner_future);
                let _handle = rt::spawn(inner_future);
            }
            Backend::Blocking => {
                let _handle = rt::spawn_blocking(move || {
                    rt::block_on(inner_future)
                });
            }
            Backend::Thread => {
                let _handle = threads::spawn(move || {
                    threads::block_on(inner_future)
                });
            }
        }

        actor_ref
    }
}

async fn run_actor<A: Actor>(
    mut actor: A,
    ctx: Context<A>,
    mut rx: mpsc::Receiver<Box<dyn Envelope<A> + Send>>,
    cancellation_token: CancellationToken,
) {
    actor.started(&ctx).await;

    if cancellation_token.is_cancelled() {
        actor.stopped(&ctx).await;
        return;
    }

    loop {
        let msg = rx.recv().await;
        match msg {
            Some(envelope) => {
                let result = AssertUnwindSafe(envelope.handle(&mut actor, &ctx))
                    .catch_unwind()
                    .await;
                if let Err(panic) = result {
                    tracing::error!("Panic in message handler: {panic:?}");
                    break;
                }
                if cancellation_token.is_cancelled() {
                    break;
                }
            }
            None => break,
        }
    }

    cancellation_token.cancel();
    actor.stopped(&ctx).await;
}

// ---------------------------------------------------------------------------
// Actor::start
// ---------------------------------------------------------------------------

pub trait ActorStart: Actor {
    fn start(self) -> ActorRef<Self> {
        self.start_with_backend(Backend::default())
    }

    fn start_with_backend(self, backend: Backend) -> ActorRef<Self> {
        ActorRef::spawn(self, backend)
    }
}

impl<A: Actor> ActorStart for A {}

// ---------------------------------------------------------------------------
// send_message_on (utility)
// ---------------------------------------------------------------------------

pub fn send_message_on<A, M, U>(ctx: Context<A>, future: U, msg: M) -> JoinHandle<()>
where
    A: Actor + Handler<M>,
    M: Message,
    U: Future + Send + 'static,
    <U as Future>::Output: Send,
{
    let cancellation_token = ctx.cancellation_token();
    let join_handle = rt::spawn(async move {
        let is_cancelled = pin!(cancellation_token.cancelled());
        let signal = pin!(future);
        match future::select(is_cancelled, signal).await {
            future::Either::Left(_) => tracing::debug!("Actor stopped"),
            future::Either::Right(_) => {
                if let Err(e) = ctx.send(msg) {
                    tracing::error!("Failed to send message: {e:?}")
                }
            }
        }
    });
    join_handle
}

// ---------------------------------------------------------------------------
// WarnOnBlocking (debug only)
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messages;
    use std::{
        sync::{atomic, Arc},
        thread,
        time::Duration,
    };

    // --- Counter actor for basic tests ---

    struct Counter {
        count: u64,
    }

    messages! {
        GetCount -> u64;
        Increment -> u64;
        StopCounter -> u64
    }

    impl Actor for Counter {}

    impl Handler<GetCount> for Counter {
        async fn handle(&mut self, _msg: GetCount, _ctx: &Context<Self>) -> u64 {
            self.count
        }
    }

    impl Handler<Increment> for Counter {
        async fn handle(&mut self, _msg: Increment, _ctx: &Context<Self>) -> u64 {
            self.count += 1;
            self.count
        }
    }

    impl Handler<StopCounter> for Counter {
        async fn handle(&mut self, _msg: StopCounter, ctx: &Context<Self>) -> u64 {
            ctx.stop();
            self.count
        }
    }

    #[test]
    pub fn backend_default_is_async() {
        assert_eq!(Backend::default(), Backend::Async);
    }

    #[test]
    #[allow(clippy::clone_on_copy)]
    pub fn backend_enum_is_copy_and_clone() {
        let backend = Backend::Async;
        let copied = backend;
        let cloned = backend.clone();
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

    #[test]
    pub fn backend_async_handles_send_and_request() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let counter = Counter { count: 0 }.start();

            let result = counter.request(GetCount).await.unwrap();
            assert_eq!(result, 0);

            let result = counter.request(Increment).await.unwrap();
            assert_eq!(result, 1);

            // fire-and-forget send
            counter.send(Increment).unwrap();
            rt::sleep(Duration::from_millis(10)).await;

            let result = counter.request(GetCount).await.unwrap();
            assert_eq!(result, 2);

            let final_count = counter.request(StopCounter).await.unwrap();
            assert_eq!(final_count, 2);
        });
    }

    #[test]
    pub fn backend_blocking_handles_send_and_request() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let counter = Counter { count: 0 }.start_with_backend(Backend::Blocking);

            let result = counter.request(GetCount).await.unwrap();
            assert_eq!(result, 0);

            let result = counter.request(Increment).await.unwrap();
            assert_eq!(result, 1);

            counter.send(Increment).unwrap();
            rt::sleep(Duration::from_millis(50)).await;

            let result = counter.request(GetCount).await.unwrap();
            assert_eq!(result, 2);

            let final_count = counter.request(StopCounter).await.unwrap();
            assert_eq!(final_count, 2);
        });
    }

    #[test]
    pub fn backend_thread_handles_send_and_request() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let counter = Counter { count: 0 }.start_with_backend(Backend::Thread);

            let result = counter.request(GetCount).await.unwrap();
            assert_eq!(result, 0);

            let result = counter.request(Increment).await.unwrap();
            assert_eq!(result, 1);

            counter.send(Increment).unwrap();
            rt::sleep(Duration::from_millis(50)).await;

            let result = counter.request(GetCount).await.unwrap();
            assert_eq!(result, 2);

            let final_count = counter.request(StopCounter).await.unwrap();
            assert_eq!(final_count, 2);
        });
    }

    #[test]
    pub fn multiple_backends_concurrent() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let async_counter = Counter { count: 0 }.start();
            let blocking_counter = Counter { count: 100 }.start_with_backend(Backend::Blocking);
            let thread_counter = Counter { count: 200 }.start_with_backend(Backend::Thread);

            async_counter.request(Increment).await.unwrap();
            blocking_counter.request(Increment).await.unwrap();
            thread_counter.request(Increment).await.unwrap();

            let async_val = async_counter.request(GetCount).await.unwrap();
            let blocking_val = blocking_counter.request(GetCount).await.unwrap();
            let thread_val = thread_counter.request(GetCount).await.unwrap();

            assert_eq!(async_val, 1);
            assert_eq!(blocking_val, 101);
            assert_eq!(thread_val, 201);

            async_counter.request(StopCounter).await.unwrap();
            blocking_counter.request(StopCounter).await.unwrap();
            thread_counter.request(StopCounter).await.unwrap();
        });
    }

    #[test]
    pub fn request_timeout() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            struct SlowActor;
            messages! { SlowOp -> () }
            impl Actor for SlowActor {}
            impl Handler<SlowOp> for SlowActor {
                async fn handle(&mut self, _msg: SlowOp, _ctx: &Context<Self>) {
                    rt::sleep(Duration::from_millis(200)).await;
                }
            }

            let actor = SlowActor.start();
            let result = actor
                .request_with_timeout(SlowOp, Duration::from_millis(50))
                .await;
            assert!(matches!(result, Err(ActorError::RequestTimeout)));
        });
    }

    #[test]
    pub fn recipient_type_erasure() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let counter = Counter { count: 42 }.start();
            let recipient: Recipient<GetCount> = counter.recipient();

            let rx = recipient.request_raw(GetCount).unwrap();
            let result = rx.await.unwrap();
            assert_eq!(result, 42);

            // Also test request helper
            let result = request(&*recipient, GetCount, Duration::from_secs(5)).await.unwrap();
            assert_eq!(result, 42);
        });
    }

    // --- SlowShutdownActor for join tests ---

    struct SlowShutdownActor;

    messages! { StopSlow -> () }

    impl Actor for SlowShutdownActor {
        async fn stopped(&mut self, _ctx: &Context<Self>) {
            thread::sleep(Duration::from_millis(500));
        }
    }

    impl Handler<StopSlow> for SlowShutdownActor {
        async fn handle(&mut self, _msg: StopSlow, ctx: &Context<Self>) {
            ctx.stop();
        }
    }

    #[test]
    pub fn thread_backend_join_does_not_block_runtime() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async move {
            let slow_actor = SlowShutdownActor.start_with_backend(Backend::Thread);

            let tick_count = Arc::new(atomic::AtomicU64::new(0));
            let tick_count_clone = tick_count.clone();
            let _ticker = rt::spawn(async move {
                for _ in 0..20 {
                    rt::sleep(Duration::from_millis(50)).await;
                    tick_count_clone.fetch_add(1, atomic::Ordering::SeqCst);
                }
            });

            slow_actor.send(StopSlow).unwrap();
            rt::sleep(Duration::from_millis(10)).await;

            slow_actor.join().await;

            let count_after_join = tick_count.load(atomic::Ordering::SeqCst);
            assert!(
                count_after_join >= 8,
                "Ticker should have completed ~10 ticks during the 500ms join(), but only got {}. \
                 This suggests join() blocked the runtime.",
                count_after_join
            );
        });
    }

    #[test]
    pub fn multiple_join_callers_all_notified() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let actor = SlowShutdownActor.start();
            let actor_clone1 = actor.clone();
            let actor_clone2 = actor.clone();

            let join1 = rt::spawn(async move {
                actor_clone1.join().await;
                1u32
            });
            let join2 = rt::spawn(async move {
                actor_clone2.join().await;
                2u32
            });

            rt::sleep(Duration::from_millis(10)).await;

            actor.send(StopSlow).unwrap();

            let (r1, r2) = tokio::join!(join1, join2);
            assert_eq!(r1.unwrap(), 1);
            assert_eq!(r2.unwrap(), 2);

            actor.join().await;
        });
    }

    // --- Badly behaved actors for blocking tests ---

    struct BadlyBehavedTask;

    messages! { DoBlock -> () }

    impl Actor for BadlyBehavedTask {}

    impl Handler<DoBlock> for BadlyBehavedTask {
        async fn handle(&mut self, _msg: DoBlock, ctx: &Context<Self>) {
            rt::sleep(Duration::from_millis(20)).await;
            thread::sleep(Duration::from_secs(2));
            ctx.stop();
        }
    }

    messages! { IncrementWell -> () }

    struct WellBehavedTask {
        pub count: u64,
    }

    impl Actor for WellBehavedTask {}

    impl Handler<GetCount> for WellBehavedTask {
        async fn handle(&mut self, _msg: GetCount, _ctx: &Context<Self>) -> u64 {
            self.count
        }
    }

    impl Handler<StopCounter> for WellBehavedTask {
        async fn handle(&mut self, _msg: StopCounter, ctx: &Context<Self>) -> u64 {
            ctx.stop();
            self.count
        }
    }

    impl Handler<IncrementWell> for WellBehavedTask {
        async fn handle(&mut self, _msg: IncrementWell, ctx: &Context<Self>) {
            self.count += 1;
            use crate::tasks::send_after;
            send_after(Duration::from_millis(100), ctx.clone(), IncrementWell);
        }
    }

    #[test]
    pub fn badly_behaved_thread_non_blocking() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let badboy = BadlyBehavedTask.start();
            badboy.send(DoBlock).unwrap();
            let goodboy = WellBehavedTask { count: 0 }.start();
            goodboy.send(IncrementWell).unwrap();
            rt::sleep(Duration::from_secs(1)).await;
            let count = goodboy.request(GetCount).await.unwrap();
            assert_ne!(count, 10);
            goodboy.request(StopCounter).await.unwrap();
        });
    }

    #[test]
    pub fn badly_behaved_thread() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let badboy = BadlyBehavedTask.start_with_backend(Backend::Blocking);
            badboy.send(DoBlock).unwrap();
            let goodboy = WellBehavedTask { count: 0 }.start();
            goodboy.send(IncrementWell).unwrap();
            rt::sleep(Duration::from_secs(1)).await;
            let count = goodboy.request(GetCount).await.unwrap();
            assert_eq!(count, 10);
            goodboy.request(StopCounter).await.unwrap();
        });
    }

    #[test]
    pub fn backend_thread_isolates_blocking_work() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let badboy = BadlyBehavedTask.start_with_backend(Backend::Thread);
            badboy.send(DoBlock).unwrap();
            let goodboy = WellBehavedTask { count: 0 }.start();
            goodboy.send(IncrementWell).unwrap();
            rt::sleep(Duration::from_secs(1)).await;
            let count = goodboy.request(GetCount).await.unwrap();
            assert_eq!(count, 10);
            goodboy.request(StopCounter).await.unwrap();
        });
    }
}
