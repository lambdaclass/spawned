use spawned_rt::threads::{
    self as rt, mpsc, oneshot, oneshot::RecvTimeoutError, CancellationToken,
};
use std::{
    collections::HashMap,
    fmt::Debug,
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{Arc, Mutex},
    time::Duration,
};

use crate::child_handle::{ActorId, ChildHandle};
use crate::error::{panic_message, ActorError, ExitReason};
use crate::message::Message;
use crate::monitor::{Down, MonitorCleanup, MonitorRef, MonitorTable};

pub use crate::response::DEFAULT_REQUEST_TIMEOUT;

// ---------------------------------------------------------------------------
// Actor trait
// ---------------------------------------------------------------------------

/// Trait for defining an actor's lifecycle hooks.
///
/// Implement this trait (typically via `#[actor]`) to define `started()` and
/// `stopped()` callbacks. Message handling is defined separately via [`Handler<M>`].
///
/// Actors must be `Send + Sized + 'static` so they can be moved to a spawned thread.
pub trait Actor: Send + Sized + 'static {
    fn started(&mut self, _ctx: &Context<Self>) {}
    fn stopped(&mut self, _ctx: &Context<Self>) {}
}

// ---------------------------------------------------------------------------
// Handler trait (per-message, sync version)
// ---------------------------------------------------------------------------

/// Per-message handler trait. Implement once for each message type the actor handles.
///
/// Unlike the `tasks` version, handlers are synchronous — no `async`/`.await`.
pub trait Handler<M: Message>: Actor {
    fn handle(&mut self, msg: M, ctx: &Context<Self>) -> M::Result;
}

// ---------------------------------------------------------------------------
// Envelope (type-erasure)
// ---------------------------------------------------------------------------

trait Envelope<A: Actor>: Send {
    fn handle(self: Box<Self>, actor: &mut A, ctx: &Context<A>);
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
    fn handle(self: Box<Self>, actor: &mut A, ctx: &Context<A>) {
        let result = actor.handle(self.msg, ctx);
        if let Some(tx) = self.tx {
            let _ = tx.send(result);
        }
    }
}

// ---------------------------------------------------------------------------
// Context
// ---------------------------------------------------------------------------

/// Handle passed to every handler and lifecycle hook, providing access to the
/// actor's mailbox and lifecycle controls.
///
/// Clone is cheap — it clones the inner channel sender and cancellation token.
pub struct Context<A: Actor> {
    id: ActorId,
    sender: mpsc::Sender<Box<dyn Envelope<A> + Send>>,
    cancellation_token: CancellationToken,
    completion: spawned_rt::tasks::watch::Receiver<Option<ExitReason>>,
    monitors: MonitorTable,
}

impl<A: Actor> Clone for Context<A> {
    fn clone(&self) -> Self {
        Self {
            id: self.id,
            sender: self.sender.clone(),
            cancellation_token: self.cancellation_token.clone(),
            completion: self.completion.clone(),
            monitors: self.monitors.clone(),
        }
    }
}

impl<A: Actor> Debug for Context<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Context").finish_non_exhaustive()
    }
}

impl<A: Actor> Context<A> {
    /// Create a `Context` from an `ActorRef`. Useful for setting up timers
    /// or stream listeners from outside the actor.
    pub fn from_ref(actor_ref: &ActorRef<A>) -> Self {
        Self {
            id: actor_ref.id,
            sender: actor_ref.sender.clone(),
            cancellation_token: actor_ref.cancellation_token.clone(),
            completion: actor_ref.completion.clone(),
            monitors: actor_ref.monitors.clone(),
        }
    }

    /// The actor's unique identity.
    pub fn id(&self) -> ActorId {
        self.id
    }

    /// Signal the actor to stop. The current handler will finish, then
    /// `stopped()` is called and the actor exits.
    pub fn stop(&self) {
        self.cancellation_token.cancel();
    }

    /// Send a fire-and-forget message to this actor.
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

    /// Send a request and get a raw oneshot receiver for the reply.
    pub fn request_raw<M>(&self, msg: M) -> Result<oneshot::Receiver<M::Result>, ActorError>
    where
        A: Handler<M>,
        M: Message,
    {
        let (tx, rx) = oneshot::channel();
        let envelope = MessageEnvelope { msg, tx: Some(tx) };
        self.sender
            .send(Box::new(envelope))
            .map_err(|_| ActorError::ActorStopped)?;
        Ok(rx)
    }

    /// Send a request and block until the reply arrives (default 5s timeout).
    pub fn request<M>(&self, msg: M) -> Result<M::Result, ActorError>
    where
        A: Handler<M>,
        M: Message,
    {
        self.request_with_timeout(msg, DEFAULT_REQUEST_TIMEOUT)
    }

    /// Send a request and block until the reply arrives, with a custom timeout.
    pub fn request_with_timeout<M>(
        &self,
        msg: M,
        duration: Duration,
    ) -> Result<M::Result, ActorError>
    where
        A: Handler<M>,
        M: Message,
    {
        let rx = self.request_raw(msg)?;
        match rx.recv_timeout(duration) {
            Ok(result) => Ok(result),
            Err(RecvTimeoutError::Timeout) => Err(ActorError::RequestTimeout),
            Err(RecvTimeoutError::Disconnected) => Err(ActorError::ActorStopped),
        }
    }

    /// Get a type-erased `Recipient<M>` for sending a single message type
    /// to this actor.
    pub fn recipient<M>(&self) -> Recipient<M>
    where
        A: Handler<M>,
        M: Message,
    {
        Arc::new(self.clone())
    }

    /// Get an `ActorRef<A>` from this context.
    pub fn actor_ref(&self) -> ActorRef<A> {
        ActorRef {
            id: self.id,
            sender: self.sender.clone(),
            cancellation_token: self.cancellation_token.clone(),
            completion: self.completion.clone(),
            monitors: self.monitors.clone(),
        }
    }

    /// Set up a unidirectional monitor on another actor.
    ///
    /// Returns a [`MonitorRef`] that can be used to cancel the monitor via
    /// [`Context::demonitor`]. When the monitored actor stops, a [`Down`]
    /// message is delivered to this actor's mailbox via `Handler<Down>`.
    ///
    /// If the target is already dead, a `Down` message is delivered immediately.
    ///
    /// Multiple independent monitors are allowed on the same target — each
    /// call returns a distinct `MonitorRef`.
    ///
    /// Monitors are unidirectional: the monitored actor is unaware of the
    /// monitor and unaffected by it.
    /// Registration after owner shutdown starts (including in `stopped()`)
    /// returns an inert reference and creates no waiter or notification.
    ///
    /// **Resource cost (threads mode):** each active monitor occupies one OS
    /// thread until the target exits, the monitor is cancelled, or its owner
    /// stops, waiting on the target's completion signal. For supervisors with many long-lived
    /// children, consider using tasks mode instead.
    pub fn monitor(&self, target: &ChildHandle) -> MonitorRef
    where
        A: Handler<Down>,
    {
        let monitor_ref = MonitorRef::next();
        let active = spawned_rt::tasks::CancellationToken::new();

        {
            let mut monitors = self.monitors.lock().unwrap_or_else(|p| p.into_inner());
            // The token check makes registration inert once shutdown starts.
            // The closed table below serializes registration with exit cleanup,
            // even on paths that unwind before cancelling the owner token.
            if self.cancellation_token.is_cancelled() {
                return monitor_ref;
            }
            let Some(monitors) = monitors.as_mut() else {
                return monitor_ref;
            };
            monitors.insert(monitor_ref, active.clone());
        }

        let target = target.clone();
        let actor_ref = self.actor_ref();
        let monitors = self.monitors.clone();

        rt::spawn(move || {
            let reason = futures::executor::block_on(async {
                match futures::future::select(
                    Box::pin(active.cancelled()),
                    Box::pin(target.wait_exit_async()),
                )
                .await
                {
                    futures::future::Either::Left(_) => None,
                    futures::future::Either::Right((reason, _)) => Some(reason),
                }
            });
            let Some(reason) = reason else { return };
            // Remove the entry from the monitor table so it doesn't accumulate
            // stale entries over the actor's lifetime. Done before delivery
            // since `demonitor` is now a no-op for this monitor anyway.
            let deliver = monitors
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .as_mut()
                .and_then(|entries| entries.remove(&monitor_ref))
                .is_some();
            if deliver {
                let _ = actor_ref.send(Down {
                    monitor_ref,
                    reason,
                });
            }
        });

        monitor_ref
    }

    /// Cancel a previously-set monitor.
    ///
    /// The monitor waiter is released promptly.
    /// If the target hasn't yet died, no `Down` message will be delivered.
    /// If a `Down` message has already been delivered (or queued), this is
    /// a best-effort cancellation — the message may still arrive.
    pub fn demonitor(&self, monitor_ref: MonitorRef) {
        let active = self
            .monitors
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_mut()
            .and_then(|entries| entries.remove(&monitor_ref));
        // Waking the waiter must happen after releasing the table lock.
        if let Some(active) = active {
            active.cancel();
        }
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

/// Object-safe trait for sending a single message type to an actor.
///
/// Implemented automatically by `ActorRef<A>` and `Context<A>` for any
/// message type that `A` handles.
pub trait Receiver<M: Message>: Send + Sync {
    fn send(&self, msg: M) -> Result<(), ActorError>;
    fn request_raw(&self, msg: M) -> Result<oneshot::Receiver<M::Result>, ActorError>;
}

/// Type-erased reference for sending a single message type.
pub type Recipient<M> = Arc<dyn Receiver<M>>;

/// Send a request through a type-erased `Receiver` with a custom timeout.
pub fn request<M: Message>(
    recipient: &dyn Receiver<M>,
    msg: M,
    timeout: Duration,
) -> Result<M::Result, ActorError> {
    let rx = recipient.request_raw(msg)?;
    match rx.recv_timeout(timeout) {
        Ok(result) => Ok(result),
        Err(RecvTimeoutError::Timeout) => Err(ActorError::RequestTimeout),
        Err(RecvTimeoutError::Disconnected) => Err(ActorError::ActorStopped),
    }
}

// ---------------------------------------------------------------------------
// ActorRef
// ---------------------------------------------------------------------------

struct CompletionGuard {
    completion: spawned_rt::tasks::watch::Sender<Option<ExitReason>>,
    reason: Option<ExitReason>,
}

impl Drop for CompletionGuard {
    fn drop(&mut self) {
        self.completion.send_replace(
            self.reason
                .take()
                .or(Some(ExitReason::Panic("unexpected framework panic".into()))),
        );
    }
}

/// External handle to a running actor. Cloneable, `Send + Sync`.
///
/// Use this to send messages, make requests, or wait for the actor to stop.
/// To stop the actor, send an explicit shutdown message through your protocol,
/// or call [`Context::stop`] from within a handler.
pub struct ActorRef<A: Actor> {
    id: ActorId,
    sender: mpsc::Sender<Box<dyn Envelope<A> + Send>>,
    cancellation_token: CancellationToken,
    completion: spawned_rt::tasks::watch::Receiver<Option<ExitReason>>,
    monitors: MonitorTable,
}

impl<A: Actor> Debug for ActorRef<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActorRef").finish_non_exhaustive()
    }
}

impl<A: Actor> Clone for ActorRef<A> {
    fn clone(&self) -> Self {
        Self {
            id: self.id,
            sender: self.sender.clone(),
            cancellation_token: self.cancellation_token.clone(),
            completion: self.completion.clone(),
            monitors: self.monitors.clone(),
        }
    }
}

impl<A: Actor> ActorRef<A> {
    /// Send a fire-and-forget message to the actor.
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

    /// Send a request and get a raw oneshot receiver for the reply.
    pub fn request_raw<M>(&self, msg: M) -> Result<oneshot::Receiver<M::Result>, ActorError>
    where
        A: Handler<M>,
        M: Message,
    {
        let (tx, rx) = oneshot::channel();
        let envelope = MessageEnvelope { msg, tx: Some(tx) };
        self.sender
            .send(Box::new(envelope))
            .map_err(|_| ActorError::ActorStopped)?;
        Ok(rx)
    }

    /// Send a request and block until the reply arrives (default 5s timeout).
    pub fn request<M>(&self, msg: M) -> Result<M::Result, ActorError>
    where
        A: Handler<M>,
        M: Message,
    {
        self.request_with_timeout(msg, DEFAULT_REQUEST_TIMEOUT)
    }

    /// Send a request and block until the reply arrives, with a custom timeout.
    pub fn request_with_timeout<M>(
        &self,
        msg: M,
        duration: Duration,
    ) -> Result<M::Result, ActorError>
    where
        A: Handler<M>,
        M: Message,
    {
        let rx = self.request_raw(msg)?;
        match rx.recv_timeout(duration) {
            Ok(result) => Ok(result),
            Err(RecvTimeoutError::Timeout) => Err(ActorError::RequestTimeout),
            Err(RecvTimeoutError::Disconnected) => Err(ActorError::ActorStopped),
        }
    }

    /// Get a type-erased `Recipient<M>` for this actor.
    pub fn recipient<M>(&self) -> Recipient<M>
    where
        A: Handler<M>,
        M: Message,
    {
        Arc::new(self.clone())
    }

    /// Get a `Context<A>` from this ref, for timer setup or stream listeners.
    pub fn context(&self) -> Context<A> {
        Context::from_ref(self)
    }

    /// Block until the actor has fully stopped (including `stopped()` callback).
    pub fn join(&self) {
        let _ = self.wait_exit();
    }

    /// Poll the exit reason. Returns `None` if the actor is still running.
    /// A completion channel closed without a reason reports [`ExitReason::Kill`],
    /// matching the wait methods (for example, after runtime shutdown).
    pub fn exit_reason(&self) -> Option<ExitReason> {
        crate::child_handle::poll_exit_reason(&self.completion)
    }

    /// Block until the actor stops and return the exit reason.
    pub fn wait_exit(&self) -> ExitReason {
        crate::child_handle::wait_for_thread_exit_blocking(self.completion.clone())
    }

    /// The actor's unique identity.
    pub fn id(&self) -> ActorId {
        self.id
    }

    /// Get a type-erased `ChildHandle` for this actor.
    pub fn child_handle(&self) -> ChildHandle {
        ChildHandle::from(self.clone())
    }
}

impl<A: Actor> From<ActorRef<A>> for ChildHandle {
    fn from(actor_ref: ActorRef<A>) -> Self {
        ChildHandle::from_threads(
            actor_ref.id,
            actor_ref.cancellation_token,
            actor_ref.completion,
        )
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
    fn spawn(actor: A) -> Self {
        let (tx, rx) = mpsc::channel::<Box<dyn Envelope<A> + Send>>();
        let cancellation_token = CancellationToken::new();
        let (completion_tx, completion) = spawned_rt::tasks::watch::channel(None);
        let id = ActorId::next();
        let monitors: MonitorTable = Arc::new(Mutex::new(Some(HashMap::new())));

        let actor_ref = ActorRef {
            id,
            sender: tx.clone(),
            cancellation_token: cancellation_token.clone(),
            completion: completion.clone(),
            monitors: monitors.clone(),
        };

        let ctx = Context {
            id,
            sender: tx,
            cancellation_token: cancellation_token.clone(),
            completion: actor_ref.completion.clone(),
            monitors,
        };

        let _thread_handle = rt::spawn(move || {
            let mut guard = CompletionGuard {
                completion: completion_tx,
                reason: None, // defaults to Kill if run_actor panics unexpectedly
            };
            guard.reason = Some(run_actor(actor, ctx, rx, cancellation_token));
        });

        actor_ref
    }
}

fn run_actor<A: Actor>(
    actor: A,
    ctx: Context<A>,
    rx: mpsc::Receiver<Box<dyn Envelope<A> + Send>>,
    cancellation_token: CancellationToken,
) -> ExitReason {
    run_actor_receiving(
        actor,
        ctx,
        move || rx.recv_timeout(Duration::from_millis(100)),
        cancellation_token,
    )
}

fn run_actor_receiving<A: Actor>(
    mut actor: A,
    ctx: Context<A>,
    mut receive: impl FnMut() -> Result<Box<dyn Envelope<A> + Send>, std::sync::mpsc::RecvTimeoutError>,
    cancellation_token: CancellationToken,
) -> ExitReason {
    let mut discarded = None;
    let _monitor_cleanup = MonitorCleanup(ctx.monitors.clone());
    let start_result = catch_unwind(AssertUnwindSafe(|| {
        actor.started(&ctx);
    }));
    if let Err(panic) = start_result {
        let msg = panic_message(&*panic);
        tracing::error!("Panic in started() callback: {msg}");
        cancellation_token.cancel();
        return ExitReason::Panic(format!("panic in started(): {msg}"));
    }

    if cancellation_token.is_cancelled() {
        if let Err(panic) = catch_unwind(AssertUnwindSafe(|| actor.stopped(&ctx))) {
            let msg = panic_message(&*panic);
            tracing::error!("Panic in stopped() callback: {msg}");
            return ExitReason::Panic(format!("panic in stopped(): {msg}"));
        }
        return ExitReason::Normal;
    }

    let mut exit_reason = ExitReason::Normal;

    loop {
        let msg = match receive() {
            Ok(msg) => Some(msg),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if cancellation_token.is_cancelled() {
                    break;
                }
                continue;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => None,
        };
        match msg {
            Some(envelope) => {
                // A stop observed after receive wins over dispatch, whether
                // the channel waited or was buffered. Defer the message drop.
                if cancellation_token.is_cancelled() {
                    discarded = Some(envelope);
                    break;
                }
                let result = catch_unwind(AssertUnwindSafe(|| {
                    envelope.handle(&mut actor, &ctx);
                }));
                if let Err(panic) = result {
                    let msg = panic_message(&*panic);
                    tracing::error!("Panic in message handler: {msg}");
                    exit_reason = ExitReason::Panic(format!("panic in handler: {msg}"));
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
    let stop_result = catch_unwind(AssertUnwindSafe(|| {
        actor.stopped(&ctx);
    }));
    if let Err(panic) = stop_result {
        let msg = panic_message(&*panic);
        tracing::error!("Panic in stopped() callback: {msg}");
        if !exit_reason.is_abnormal() {
            exit_reason = ExitReason::Panic(format!("panic in stopped(): {msg}"));
        }
    }

    // Message destructors and reply cancellation may re-enter monitor state.
    // Preserve stopped() -> monitor cleanup -> undelivered-message drop.
    drop(_monitor_cleanup);
    drop(discarded);
    exit_reason
}

// ---------------------------------------------------------------------------
// Actor::start
// ---------------------------------------------------------------------------

/// Extension trait for starting an actor. Automatically implemented for all [`Actor`] types.
pub trait ActorStart: Actor {
    /// Start the actor on a dedicated OS thread.
    fn start(self) -> ActorRef<Self> {
        ActorRef::spawn(self)
    }
}

impl<A: Actor> ActorStart for A {}

// ---------------------------------------------------------------------------
// send_message_on (utility)
// ---------------------------------------------------------------------------

/// Send a message to an actor when a blocking closure completes.
///
/// Spawns a thread that runs `f()`, then sends `msg` to the actor.
/// If the actor stops before `f()` returns, the message is not sent.
pub fn send_message_on<A, M, F>(ctx: Context<A>, f: F, msg: M) -> rt::JoinHandle<()>
where
    A: Actor + Handler<M>,
    M: Message,
    F: FnOnce() + Send + 'static,
{
    let cancellation_token = ctx.cancellation_token();
    rt::spawn(move || {
        f();
        if !cancellation_token.is_cancelled() {
            if let Err(e) = ctx.send(msg) {
                tracing::error!("Failed to send message: {e:?}")
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::Message;
    use std::thread;

    struct Counter {
        count: u64,
    }

    struct GetCount;
    impl Message for GetCount {
        type Result = u64;
    }

    struct Increment;
    impl Message for Increment {
        type Result = u64;
    }

    struct StopCounter;
    impl Message for StopCounter {
        type Result = u64;
    }

    impl Actor for Counter {}

    impl Handler<GetCount> for Counter {
        fn handle(&mut self, _msg: GetCount, _ctx: &Context<Self>) -> u64 {
            self.count
        }
    }

    impl Handler<Increment> for Counter {
        fn handle(&mut self, _msg: Increment, _ctx: &Context<Self>) -> u64 {
            self.count += 1;
            self.count
        }
    }

    impl Handler<StopCounter> for Counter {
        fn handle(&mut self, _msg: StopCounter, ctx: &Context<Self>) -> u64 {
            ctx.stop();
            self.count
        }
    }

    #[test]
    fn basic_send_and_request() {
        let actor = Counter { count: 0 }.start();
        assert_eq!(actor.request(GetCount).unwrap(), 0);
        assert_eq!(actor.request(Increment).unwrap(), 1);
        actor.send(Increment).unwrap();
        rt::sleep(Duration::from_millis(50));
        assert_eq!(actor.request(GetCount).unwrap(), 2);
        actor.request(StopCounter).unwrap();
    }

    #[test]
    fn join_waits_for_completion() {
        struct SlowStop;
        struct StopSlow;
        impl Message for StopSlow {
            type Result = ();
        }
        impl Actor for SlowStop {
            fn stopped(&mut self, _ctx: &Context<Self>) {
                rt::sleep(Duration::from_millis(300));
            }
        }
        impl Handler<StopSlow> for SlowStop {
            fn handle(&mut self, _msg: StopSlow, ctx: &Context<Self>) {
                ctx.stop();
            }
        }

        let actor = SlowStop.start();
        actor.send(StopSlow).unwrap();
        actor.join();
        // If join() returned, stopped() has completed
    }

    #[test]
    fn join_multiple_callers() {
        struct SlowStop2;
        struct StopSlow2;
        impl Message for StopSlow2 {
            type Result = ();
        }
        impl Actor for SlowStop2 {
            fn stopped(&mut self, _ctx: &Context<Self>) {
                rt::sleep(Duration::from_millis(200));
            }
        }
        impl Handler<StopSlow2> for SlowStop2 {
            fn handle(&mut self, _msg: StopSlow2, ctx: &Context<Self>) {
                ctx.stop();
            }
        }

        let actor = SlowStop2.start();
        let a1 = actor.clone();
        let a2 = actor.clone();
        let t1 = thread::spawn(move || {
            a1.join();
            1u32
        });
        let t2 = thread::spawn(move || {
            a2.join();
            2u32
        });
        actor.send(StopSlow2).unwrap();
        assert_eq!(t1.join().unwrap(), 1);
        assert_eq!(t2.join().unwrap(), 2);
    }

    #[test]
    fn panic_in_started_stops_actor() {
        struct PanicOnStart;
        struct PingThread;
        impl Message for PingThread {
            type Result = ();
        }
        impl Actor for PanicOnStart {
            fn started(&mut self, _ctx: &Context<Self>) {
                panic!("boom in started");
            }
        }
        impl Handler<PingThread> for PanicOnStart {
            fn handle(&mut self, _msg: PingThread, _ctx: &Context<Self>) {}
        }

        let actor = PanicOnStart.start();
        rt::sleep(Duration::from_millis(50));
        let result = actor.send(PingThread);
        assert!(result.is_err());
    }

    #[test]
    fn panic_in_handler_stops_actor() {
        struct PanicOnMsg;
        struct ExplodeThread;
        impl Message for ExplodeThread {
            type Result = ();
        }
        struct CheckThread;
        impl Message for CheckThread {
            type Result = u32;
        }
        impl Actor for PanicOnMsg {}
        impl Handler<ExplodeThread> for PanicOnMsg {
            fn handle(&mut self, _msg: ExplodeThread, _ctx: &Context<Self>) {
                panic!("boom in handler");
            }
        }
        impl Handler<CheckThread> for PanicOnMsg {
            fn handle(&mut self, _msg: CheckThread, _ctx: &Context<Self>) -> u32 {
                42
            }
        }

        let actor = PanicOnMsg.start();
        actor.send(ExplodeThread).unwrap();
        rt::sleep(Duration::from_millis(200));
        let result = actor.request(CheckThread);
        assert!(result.is_err());
    }

    #[test]
    fn panic_in_stopped_still_completes() {
        struct PanicOnStop;
        struct StopMeThread;
        impl Message for StopMeThread {
            type Result = ();
        }
        impl Actor for PanicOnStop {
            fn stopped(&mut self, _ctx: &Context<Self>) {
                panic!("boom in stopped");
            }
        }
        impl Handler<StopMeThread> for PanicOnStop {
            fn handle(&mut self, _msg: StopMeThread, ctx: &Context<Self>) {
                ctx.stop();
            }
        }

        let actor = PanicOnStop.start();
        actor.send(StopMeThread).unwrap();
        actor.join();
    }

    #[test]
    fn recipient_type_erasure() {
        let actor = Counter { count: 42 }.start();
        let recipient: Recipient<GetCount> = actor.recipient();
        let result = request(&*recipient, GetCount, Duration::from_secs(5)).unwrap();
        assert_eq!(result, 42);
    }

    #[test]
    fn send_message_on_delivers() {
        let actor = Counter { count: 0 }.start();
        let ctx = actor.context();
        send_message_on(ctx, || rt::sleep(Duration::from_millis(10)), Increment);
        rt::sleep(Duration::from_millis(200));
        let count = actor.request(GetCount).unwrap();
        assert_eq!(count, 1);
    }

    // --- ExitReason tests ---

    #[test]
    fn exit_reason_normal_on_clean_stop() {
        let actor = Counter { count: 0 }.start();
        actor.request(StopCounter).unwrap();
        let reason = actor.wait_exit();
        assert!(matches!(reason, ExitReason::Normal));
    }

    #[test]
    fn exit_reason_panic_in_started() {
        struct PanicStartThread;
        struct PingThread2;
        impl Message for PingThread2 {
            type Result = ();
        }
        impl Actor for PanicStartThread {
            fn started(&mut self, _ctx: &Context<Self>) {
                panic!("boom in started");
            }
        }
        impl Handler<PingThread2> for PanicStartThread {
            fn handle(&mut self, _msg: PingThread2, _ctx: &Context<Self>) {}
        }

        let actor = PanicStartThread.start();
        let reason = actor.wait_exit();
        assert!(matches!(reason, ExitReason::Panic(ref msg) if msg.contains("boom in started")));
    }

    #[test]
    fn exit_reason_panic_in_handler() {
        struct PanicHandlerThread;
        struct ExplodeThread2;
        impl Message for ExplodeThread2 {
            type Result = ();
        }
        impl Actor for PanicHandlerThread {}
        impl Handler<ExplodeThread2> for PanicHandlerThread {
            fn handle(&mut self, _msg: ExplodeThread2, _ctx: &Context<Self>) {
                panic!("boom in handler");
            }
        }

        let actor = PanicHandlerThread.start();
        let _ = actor.send(ExplodeThread2);
        let reason = actor.wait_exit();
        assert!(matches!(reason, ExitReason::Panic(ref msg) if msg.contains("boom in handler")));
    }

    #[test]
    fn exit_reason_poll_returns_none_while_running() {
        let actor = Counter { count: 0 }.start();
        assert!(actor.exit_reason().is_none());
        actor.request(StopCounter).unwrap();
        actor.join();
        assert!(actor.exit_reason().is_some());
    }

    // --- Monitor tests ---

    struct GetDowns;
    impl Message for GetDowns {
        type Result = Vec<crate::monitor::Down>;
    }

    struct Watcher {
        downs: Arc<Mutex<Vec<crate::monitor::Down>>>,
    }

    struct StartMonitor(crate::ChildHandle);
    impl Message for StartMonitor {
        type Result = crate::monitor::MonitorRef;
    }
    struct CallDemonitor(crate::monitor::MonitorRef);
    impl Message for CallDemonitor {
        type Result = ();
    }

    impl Actor for Watcher {}

    impl Handler<StartMonitor> for Watcher {
        fn handle(&mut self, msg: StartMonitor, ctx: &Context<Self>) -> crate::monitor::MonitorRef {
            ctx.monitor(&msg.0)
        }
    }

    impl Handler<CallDemonitor> for Watcher {
        fn handle(&mut self, msg: CallDemonitor, ctx: &Context<Self>) {
            ctx.demonitor(msg.0);
        }
    }

    impl Handler<crate::monitor::Down> for Watcher {
        fn handle(&mut self, msg: crate::monitor::Down, _ctx: &Context<Self>) {
            self.downs.lock().unwrap().push(msg);
        }
    }

    impl Handler<GetDowns> for Watcher {
        fn handle(&mut self, _msg: GetDowns, _ctx: &Context<Self>) -> Vec<crate::monitor::Down> {
            self.downs.lock().unwrap().clone()
        }
    }

    fn make_watcher() -> ActorRef<Watcher> {
        Watcher {
            downs: Arc::new(Mutex::new(Vec::new())),
        }
        .start()
    }

    #[test]
    fn monitor_running_actor_delivers_down_on_exit() {
        let target = Counter { count: 0 }.start();
        let target_handle = target.child_handle();
        let watcher = make_watcher();

        let monitor_ref = watcher.request(StartMonitor(target_handle)).unwrap();

        target.request(StopCounter).unwrap();
        target.join();
        rt::sleep(Duration::from_millis(150));

        let downs = watcher.request(GetDowns).unwrap();
        assert_eq!(downs.len(), 1);
        assert_eq!(downs[0].monitor_ref, monitor_ref);
        assert!(matches!(downs[0].reason, ExitReason::Normal));
    }

    #[test]
    fn monitor_already_dead_actor_delivers_down_immediately() {
        let target = Counter { count: 0 }.start();
        target.request(StopCounter).unwrap();
        target.join();
        let target_handle = target.child_handle();

        let watcher = make_watcher();
        let _ = watcher.request(StartMonitor(target_handle)).unwrap();
        rt::sleep(Duration::from_millis(150));

        let downs = watcher.request(GetDowns).unwrap();
        assert_eq!(downs.len(), 1);
    }

    #[test]
    fn demonitor_before_target_dies_suppresses_down() {
        let target = Counter { count: 0 }.start();
        let target_handle = target.child_handle();
        let watcher = make_watcher();

        let monitor_ref = watcher.request(StartMonitor(target_handle)).unwrap();
        watcher.request(CallDemonitor(monitor_ref)).unwrap();

        target.request(StopCounter).unwrap();
        target.join();
        rt::sleep(Duration::from_millis(150));

        let downs = watcher.request(GetDowns).unwrap();
        assert!(downs.is_empty());
    }

    #[test]
    fn multiple_monitors_each_get_own_ref_and_down() {
        let target = Counter { count: 0 }.start();
        let target_handle = target.child_handle();
        let watcher = make_watcher();

        let r1 = watcher
            .request(StartMonitor(target_handle.clone()))
            .unwrap();
        let r2 = watcher.request(StartMonitor(target_handle)).unwrap();
        assert_ne!(r1, r2);

        target.request(StopCounter).unwrap();
        target.join();
        rt::sleep(Duration::from_millis(150));

        let downs = watcher.request(GetDowns).unwrap();
        assert_eq!(downs.len(), 2);
        let refs: Vec<_> = downs.iter().map(|d| d.monitor_ref).collect();
        assert!(refs.contains(&r1));
        assert!(refs.contains(&r2));
    }

    #[test]
    fn monitor_observes_panic_reason() {
        struct PanicMsg;
        impl Message for PanicMsg {
            type Result = ();
        }
        struct PanicMe;
        impl Actor for PanicMe {}
        impl Handler<PanicMsg> for PanicMe {
            fn handle(&mut self, _msg: PanicMsg, _ctx: &Context<Self>) {
                panic!("intentional panic");
            }
        }

        let target = PanicMe.start();
        let target_handle = target.child_handle();
        let watcher = make_watcher();

        let _ = watcher.request(StartMonitor(target_handle)).unwrap();
        let _ = target.send(PanicMsg);

        rt::sleep(Duration::from_millis(200));

        let downs = watcher.request(GetDowns).unwrap();
        assert_eq!(downs.len(), 1);
        assert!(matches!(downs[0].reason, ExitReason::Panic(_)));
    }
}

#[cfg(test)]
mod monitor_cleanup_regressions {
    use super::*;
    struct Idle;
    impl Actor for Idle {}
    impl Handler<Down> for Idle {
        fn handle(&mut self, _: Down, _: &Context<Self>) {}
    }
    #[test]
    fn closed_monitor_table_rejects_late_registration() {
        let observer = Idle.start();
        let target = Idle.start();
        // Model cleanup during an unexpected unwind before token cancellation.
        drop(MonitorCleanup(observer.monitors.clone()));
        let child = target.child_handle();
        let waiter = child.lifetime_probe();
        observer.context().monitor(&child);
        drop(child);
        assert_eq!(waiter.strong_count(), 0);
        assert!(observer.monitors.lock().unwrap().is_none());
        observer.child_handle().stop();
        target.child_handle().stop();
        observer.join();
        target.join();
    }
    fn check_cleanup(stop_owner: bool) {
        let target = Idle.start();
        let observer = Idle.start();
        let child = target.child_handle();
        let waiter = child.lifetime_probe();
        let monitor = observer.context().monitor(&child);
        drop(child);
        if stop_owner {
            observer.child_handle().stop();
            observer.join();
            drop(observer);
        } else {
            observer.context().demonitor(monitor);
            // Only the waiter now owns this particular ChildHandle allocation.
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            while waiter.strong_count() != 0 && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
            let released = waiter.strong_count() == 0;
            observer.child_handle().stop();
            observer.join();
            drop(observer);
            target.child_handle().stop();
            target.join();
            assert!(released, "demonitor retained its waiter until target exit");
            return;
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while waiter.strong_count() != 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        let released = waiter.strong_count() == 0;
        target.child_handle().stop();
        target.join();
        assert!(released, "stopped observer retained its monitor waiter");
    }
    #[test]
    fn demonitor_releases_waiter() {
        check_cleanup(false);
    }
    #[test]
    fn stopped_observer_releases_waiter() {
        check_cleanup(true);
    }
}

#[cfg(test)]
mod early_stop_regression {
    use super::*;
    struct EarlyStop;
    impl Actor for EarlyStop {
        fn started(&mut self, ctx: &Context<Self>) {
            ctx.stop();
        }
        fn stopped(&mut self, _: &Context<Self>) {
            panic!("cleanup failed");
        }
    }
    #[test]
    fn panic_during_early_stop_is_reported() {
        let actor = EarlyStop.start();
        assert!(
            matches!(actor.wait_exit(), ExitReason::Panic(message) if message.contains("cleanup failed"))
        );
    }
}

#[cfg(test)]
mod idle_stop_regression {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Barrier,
    };
    struct Counter(Arc<AtomicUsize>, Arc<Mutex<Vec<&'static str>>>);
    impl Actor for Counter {
        fn stopped(&mut self, _: &Context<Self>) {
            self.1.lock().unwrap().push("stopped");
        }
    }
    impl Drop for Counter {
        fn drop(&mut self) {
            self.1.lock().unwrap().push("actor-dropped");
        }
    }
    struct Increment(Arc<Mutex<Vec<&'static str>>>, MonitorTable);
    impl Drop for Increment {
        fn drop(&mut self) {
            assert!(
                self.1.lock().unwrap().is_none(),
                "message dropped before monitor cleanup"
            );
            self.0.lock().unwrap().push("message-dropped");
        }
    }
    impl Message for Increment {
        type Result = ();
    }
    impl Handler<Increment> for Counter {
        fn handle(&mut self, _: Increment, _: &Context<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    #[test]
    fn stop_while_receiving_does_not_dispatch_a_later_request() {
        let (tx, rx) = mpsc::channel();
        let token = CancellationToken::new();
        let (_, completion_rx) = spawned_rt::tasks::watch::channel(None);
        let monitors = Arc::new(Mutex::new(Some(HashMap::new())));
        let events = Arc::new(Mutex::new(Vec::new()));
        let ctx = Context {
            id: ActorId::next(),
            sender: tx.clone(),
            cancellation_token: token.clone(),
            completion: completion_rx,
            monitors: monitors.clone(),
        };
        let count = Arc::new(AtomicUsize::new(0));
        let seen = count.clone();
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let (e, r) = (entered.clone(), release.clone());
        let cancel = token.clone();
        let trace = events.clone();
        let actor = std::thread::spawn(move || {
            let mut first = true;
            run_actor_receiving(
                Counter(seen, trace),
                ctx,
                move || {
                    // Pause the real receive boundary after the started/cancel
                    // checks; the standard channel still carries the envelope.
                    if first {
                        first = false;
                        e.wait();
                        r.wait();
                    }
                    rx.recv_timeout(Duration::from_secs(5))
                },
                cancel,
            )
        });
        entered.wait();
        let (reply, response) = oneshot::channel();
        tx.send(Box::new(MessageEnvelope {
            msg: Increment(events.clone(), monitors),
            tx: Some(reply),
        }) as Box<dyn Envelope<Counter> + Send>)
            .unwrap();
        token.cancel();
        release.wait();
        assert_eq!(actor.join().unwrap(), ExitReason::Normal);
        assert_eq!(
            count.load(Ordering::SeqCst),
            0,
            "thread actor dispatched work after idle stop"
        );
        assert!(response.recv().is_err());
        assert_eq!(
            *events.lock().unwrap(),
            ["stopped", "message-dropped", "actor-dropped"]
        );
    }
}
