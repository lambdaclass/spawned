use crate::child_handle::{ActorId, ChildHandle};
use crate::error::{panic_message, ActorError, ExitReason};
use crate::link::{
    self, new_link_table, new_linked_exit_reason, new_trap_exit_flag, Exit, LinkTable,
    LinkedExitReason, SendExitFn, TrapExitFlag,
};
use crate::message::Message;
use crate::monitor::{Down, MonitorRef};
use core::pin::pin;
use futures::future::{self, FutureExt as _};
use spawned_rt::{
    tasks::{self as rt, mpsc, oneshot, timeout, watch, CancellationToken, JoinHandle},
    threads,
};
use std::{
    collections::HashMap,
    fmt::Debug,
    future::Future,
    panic::AssertUnwindSafe,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

/// Per-actor table of active monitors. Each entry maps a `MonitorRef` to a
/// flag the watcher checks before delivering `Down`. Shared across `Context`
/// clones via `Arc`.
type MonitorTable = Arc<Mutex<HashMap<MonitorRef, Arc<AtomicBool>>>>;

pub use crate::response::DEFAULT_REQUEST_TIMEOUT;

// ---------------------------------------------------------------------------
// Backend
// ---------------------------------------------------------------------------

/// Runtime backend for the actor's message loop (tasks mode only).
///
/// - `Async` — runs on the tokio async runtime (default, lowest overhead)
/// - `Blocking` — runs on tokio's blocking thread pool (for blocking I/O)
/// - `Thread` — runs on a dedicated OS thread (for CPU-bound work or isolation)
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

/// Trait for defining an actor's lifecycle hooks.
///
/// Implement this trait (typically via `#[actor]`) to define `started()` and
/// `stopped()` callbacks. Message handling is defined separately via [`Handler<M>`].
///
/// Actors must be `Send + Sized + 'static` so they can be moved to a spawned task.
pub trait Actor: Send + Sized + 'static {
    fn started(&mut self, _ctx: &Context<Self>) -> impl Future<Output = ()> + Send {
        async {}
    }

    fn stopped(&mut self, _ctx: &Context<Self>) -> impl Future<Output = ()> + Send {
        async {}
    }

    /// Called when a linked actor stops, if this actor has called
    /// `ctx.trap_exit(true)`. Default impl ignores the signal.
    fn exit_received(
        &mut self,
        _exit: Exit,
        _ctx: &Context<Self>,
    ) -> impl Future<Output = ()> + Send {
        async {}
    }
}

// ---------------------------------------------------------------------------
// Handler trait (per-message, uses RPITIT — NOT object-safe, that's fine)
// ---------------------------------------------------------------------------

/// Per-message handler trait. Implement once for each message type the actor handles.
///
/// Uses RPITIT (return-position `impl Trait` in traits), which means this trait
/// is **not** object-safe. For type-erased references, use [`Receiver<M>`] / [`Recipient<M>`].
pub trait Handler<M: Message>: Actor {
    fn handle(&mut self, msg: M, ctx: &Context<Self>) -> impl Future<Output = M::Result> + Send;
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

/// System envelope that dispatches an `Exit` notification to the actor's
/// `exit_received` callback. Unlike `MessageEnvelope<M>`, this does not require
/// `Handler<Exit>` on the actor — every actor can receive `Exit` via the
/// trait's default-no-op `exit_received` callback.
struct ExitEnvelope {
    exit: Exit,
}

impl<A: Actor> Envelope<A> for ExitEnvelope {
    fn handle<'a>(
        self: Box<Self>,
        actor: &'a mut A,
        ctx: &'a Context<A>,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            actor.exit_received(self.exit, ctx).await;
        })
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
    completion_rx: watch::Receiver<Option<ExitReason>>,
    monitors: MonitorTable,
    trap_exit: TrapExitFlag,
    links: LinkTable,
    linked_reason: LinkedExitReason,
}

impl<A: Actor> Clone for Context<A> {
    fn clone(&self) -> Self {
        Self {
            id: self.id,
            sender: self.sender.clone(),
            cancellation_token: self.cancellation_token.clone(),
            completion_rx: self.completion_rx.clone(),
            monitors: self.monitors.clone(),
            trap_exit: self.trap_exit.clone(),
            links: self.links.clone(),
            linked_reason: self.linked_reason.clone(),
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
            completion_rx: actor_ref.completion_rx.clone(),
            monitors: actor_ref.monitors.clone(),
            trap_exit: actor_ref.trap_exit.clone(),
            links: actor_ref.links.clone(),
            linked_reason: actor_ref.linked_reason.clone(),
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

    /// Send a request and wait for the reply (default 5s timeout).
    pub async fn request<M>(&self, msg: M) -> Result<M::Result, ActorError>
    where
        A: Handler<M>,
        M: Message,
    {
        self.request_with_timeout(msg, DEFAULT_REQUEST_TIMEOUT)
            .await
    }

    /// Send a request and wait for the reply with a custom timeout.
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
            completion_rx: self.completion_rx.clone(),
            monitors: self.monitors.clone(),
            trap_exit: self.trap_exit.clone(),
            links: self.links.clone(),
            linked_reason: self.linked_reason.clone(),
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
    pub fn monitor(&self, target: &ChildHandle) -> MonitorRef
    where
        A: Handler<Down>,
    {
        let monitor_ref = MonitorRef::next();
        let active = Arc::new(AtomicBool::new(true));

        self.monitors
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(monitor_ref, active.clone());

        let target = target.clone();
        let actor_ref = self.actor_ref();
        let monitors = self.monitors.clone();

        rt::spawn(async move {
            let reason = target.wait_exit_async().await;
            // Remove the entry from the monitor table so it doesn't accumulate
            // stale entries over the actor's lifetime. Done before delivery
            // since `demonitor` is now a no-op for this monitor anyway.
            monitors
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&monitor_ref);
            if active.load(Ordering::Acquire) {
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
    /// If the target hasn't yet died, no `Down` message will be delivered.
    /// If a `Down` message has already been delivered (or queued), this is
    /// a best-effort cancellation — the message may still arrive.
    pub fn demonitor(&self, monitor_ref: MonitorRef) {
        if let Some(active) = self
            .monitors
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&monitor_ref)
        {
            active.store(false, Ordering::Release);
        }
    }

    /// Set up a bidirectional link with another actor.
    ///
    /// When either side dies abnormally, the other receives an exit signal.
    /// By default the receiver is terminated by the signal. Call
    /// [`trap_exit(true)`] to convert signals into `Exit` messages delivered
    /// via [`Actor::exit_received`] instead.
    ///
    /// If the target is already dead with an abnormal reason, the exit
    /// signal is delivered immediately. Calling `link` twice on the same
    /// peer is a no-op (links are idempotent).
    ///
    /// [`trap_exit(true)`]: Self::trap_exit
    /// [`Actor::exit_received`]: Actor::exit_received
    pub fn link(&self, target: &ChildHandle) {
        let own_signal = link::make_signal(
            self.trap_exit.clone(),
            self.own_cancel_fn(),
            self.own_send_exit_fn(),
            self.linked_reason.clone(),
        );
        let peer_signal = link::make_signal(
            target.trap_exit_flag().clone(),
            target.cancel_fn().clone(),
            target.send_exit_fn().clone(),
            target.linked_reason().clone(),
        );
        link::register_link(
            self.id,
            &self.links,
            own_signal,
            target.id(),
            target.links(),
            peer_signal,
        );

        // If the target is already dead, we need to deliver the signal
        // ourselves — but only if the target's own `propagate_exit` hasn't
        // already done so. Atomically remove ourselves from the target's link
        // table: if we removed an entry, the target hasn't drained yet (or
        // hadn't yet seen our entry), so we deliver. If nothing was removed,
        // the target already signaled us — don't double-deliver.
        if let Some(reason) = target.exit_reason() {
            if link::take_self_from_peer_table(self.id, target.links()) {
                let signal = link::make_signal(
                    self.trap_exit.clone(),
                    self.own_cancel_fn(),
                    self.own_send_exit_fn(),
                    self.linked_reason.clone(),
                );
                signal(target.id(), reason);
            }
        }
    }

    /// Remove a previously-set bidirectional link.
    pub fn unlink(&self, target: &ChildHandle) {
        link::unregister_link(self.id, &self.links, target.id(), target.links());
    }

    /// Control how exit signals from linked actors are handled.
    ///
    /// - `false` (default): an abnormal exit signal from a linked actor cancels
    ///   this actor (propagating the death).
    /// - `true`: the signal is converted to an `Exit` message and delivered to
    ///   [`Actor::exit_received`].
    ///
    /// `Kill` is untrappable — it cancels the actor regardless of this flag.
    pub fn trap_exit(&self, enabled: bool) {
        self.trap_exit.store(enabled, Ordering::Release);
    }

    /// Build a type-erased cancel closure for this actor.
    fn own_cancel_fn(&self) -> Arc<dyn Fn() + Send + Sync> {
        let token = self.cancellation_token.clone();
        Arc::new(move || token.cancel())
    }

    /// Build a type-erased `SendExitFn` that enqueues an `ExitEnvelope`
    /// onto this actor's mailbox.
    fn own_send_exit_fn(&self) -> SendExitFn {
        let sender = self.sender.clone();
        Arc::new(move |exit: Exit| {
            let envelope: Box<dyn Envelope<A> + Send> = Box::new(ExitEnvelope { exit });
            sender.send(envelope).map_err(|_| ActorError::ActorStopped)
        })
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
///
/// Obtained via `actor_ref.recipient::<M>()` or `ctx.recipient::<M>()`.
pub type Recipient<M> = Arc<dyn Receiver<M>>;

/// Send a request through a type-erased `Receiver` with a custom timeout.
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

/// External handle to a running actor. Cloneable, `Send + Sync`.
///
/// Use this to send messages, make requests, or wait for the actor to stop.
/// To stop the actor, send an explicit shutdown message through your protocol,
/// or call [`Context::stop`] from within a handler.
pub struct ActorRef<A: Actor> {
    id: ActorId,
    sender: mpsc::Sender<Box<dyn Envelope<A> + Send>>,
    cancellation_token: CancellationToken,
    completion_rx: watch::Receiver<Option<ExitReason>>,
    monitors: MonitorTable,
    trap_exit: TrapExitFlag,
    links: LinkTable,
    linked_reason: LinkedExitReason,
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
            completion_rx: self.completion_rx.clone(),
            monitors: self.monitors.clone(),
            trap_exit: self.trap_exit.clone(),
            links: self.links.clone(),
            linked_reason: self.linked_reason.clone(),
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

    /// Send a request and wait for the reply (default 5s timeout).
    pub async fn request<M>(&self, msg: M) -> Result<M::Result, ActorError>
    where
        A: Handler<M>,
        M: Message,
    {
        self.request_with_timeout(msg, DEFAULT_REQUEST_TIMEOUT)
            .await
    }

    /// Send a request and wait for the reply with a custom timeout.
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

    /// Wait until the actor has fully stopped (including `stopped()` callback).
    pub async fn join(&self) {
        let _ = self.wait_exit().await;
    }

    /// Poll the exit reason. Returns `None` if the actor is still running.
    pub fn exit_reason(&self) -> Option<ExitReason> {
        self.completion_rx.borrow().clone()
    }

    /// Wait until the actor stops and return the exit reason.
    pub async fn wait_exit(&self) -> ExitReason {
        let mut rx = self.completion_rx.clone();
        loop {
            if let Some(reason) = rx.borrow_and_update().clone() {
                return reason;
            }
            if rx.changed().await.is_err() {
                return ExitReason::Kill;
            }
        }
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
        // Build a type-erased SendExitFn that enqueues an ExitEnvelope onto
        // this actor's mailbox.
        let sender = actor_ref.sender.clone();
        let send_exit: SendExitFn = Arc::new(move |exit: Exit| {
            let envelope: Box<dyn Envelope<A> + Send> = Box::new(ExitEnvelope { exit });
            sender.send(envelope).map_err(|_| ActorError::ActorStopped)
        });
        ChildHandle::from_tasks(
            actor_ref.id,
            actor_ref.cancellation_token,
            actor_ref.completion_rx,
            actor_ref.trap_exit,
            actor_ref.links,
            actor_ref.linked_reason,
            send_exit,
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
    fn spawn(actor: A, backend: Backend) -> Self {
        let (tx, rx) = mpsc::channel::<Box<dyn Envelope<A> + Send>>();
        let cancellation_token = CancellationToken::new();
        let (completion_tx, completion_rx) = watch::channel(None);
        let monitors: MonitorTable = Arc::new(Mutex::new(HashMap::new()));
        let trap_exit = new_trap_exit_flag();
        let links = new_link_table();
        let linked_reason = new_linked_exit_reason();

        let actor_ref = ActorRef {
            id: ActorId::next(),
            sender: tx.clone(),
            cancellation_token: cancellation_token.clone(),
            completion_rx,
            monitors: monitors.clone(),
            trap_exit: trap_exit.clone(),
            links: links.clone(),
            linked_reason: linked_reason.clone(),
        };

        let ctx = Context {
            id: actor_ref.id,
            sender: tx,
            cancellation_token: cancellation_token.clone(),
            completion_rx: actor_ref.completion_rx.clone(),
            monitors,
            trap_exit,
            links: links.clone(),
            linked_reason: linked_reason.clone(),
        };

        let actor_id = actor_ref.id;
        let inner_future = async move {
            let mut reason = run_actor(actor, ctx, rx, cancellation_token).await;
            // If the actor was cancelled by a link signal (rather than
            // ctx.stop()), use the linked reason as our exit reason so the
            // death propagates transitively through further links.
            if matches!(reason, ExitReason::Normal) {
                if let Some(linked) = linked_reason
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .take()
                {
                    reason = linked;
                }
            }
            // Propagate exit signal to linked actors before publishing the
            // completion signal.
            link::propagate_exit(actor_id, &links, &reason);
            let _ = completion_tx.send(Some(reason));
        };

        match backend {
            Backend::Async => {
                #[cfg(debug_assertions)]
                let inner_future = warn_on_block::WarnOnBlocking::new(inner_future);
                let _handle = rt::spawn(inner_future);
            }
            Backend::Blocking => {
                let _handle = rt::spawn_blocking(move || rt::block_on(inner_future));
            }
            Backend::Thread => {
                let _handle = threads::spawn(move || threads::block_on(inner_future));
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
) -> ExitReason {
    let start_result = AssertUnwindSafe(actor.started(&ctx)).catch_unwind().await;
    if let Err(panic) = start_result {
        let msg = panic_message(&*panic);
        tracing::error!("Panic in started() callback: {msg}");
        cancellation_token.cancel();
        return ExitReason::Panic(format!("panic in started(): {msg}"));
    }

    if cancellation_token.is_cancelled() {
        let _ = AssertUnwindSafe(actor.stopped(&ctx)).catch_unwind().await;
        return ExitReason::Normal;
    }

    let mut exit_reason = ExitReason::Normal;

    loop {
        let msg = {
            let recv = pin!(rx.recv());
            let cancel = pin!(cancellation_token.cancelled());
            match future::select(recv, cancel).await {
                future::Either::Left((msg, _)) => msg,
                future::Either::Right(_) => None,
            }
        };
        match msg {
            Some(envelope) => {
                let result = AssertUnwindSafe(envelope.handle(&mut actor, &ctx))
                    .catch_unwind()
                    .await;
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
    let stop_result = AssertUnwindSafe(actor.stopped(&ctx)).catch_unwind().await;
    if let Err(panic) = stop_result {
        let msg = panic_message(&*panic);
        tracing::error!("Panic in stopped() callback: {msg}");
        if !exit_reason.is_abnormal() {
            exit_reason = ExitReason::Panic(format!("panic in stopped(): {msg}"));
        }
    }

    exit_reason
}

// ---------------------------------------------------------------------------
// Actor::start
// ---------------------------------------------------------------------------

/// Extension trait for starting an actor. Automatically implemented for all [`Actor`] types.
pub trait ActorStart: Actor {
    /// Start the actor with the default backend ([`Backend::Async`]).
    fn start(self) -> ActorRef<Self> {
        self.start_with_backend(Backend::default())
    }

    /// Start the actor with a specific [`Backend`].
    fn start_with_backend(self, backend: Backend) -> ActorRef<Self> {
        ActorRef::spawn(self, backend)
    }

    /// Start the actor and link it to the caller's context.
    ///
    /// The link is registered immediately after the actor is spawned. This is
    /// **not strictly atomic** — the child may begin executing `started()` and
    /// process messages before the link is established. However, if the child
    /// dies in that window, [`Context::link`] detects the dead target and
    /// delivers the exit signal as a fallback, so no signal is lost.
    fn start_linked<P: Actor>(self, parent_ctx: &Context<P>) -> ActorRef<Self> {
        let actor_ref = self.start();
        parent_ctx.link(&actor_ref.child_handle());
        actor_ref
    }
}

impl<A: Actor> ActorStart for A {}

// ---------------------------------------------------------------------------
// send_message_on (utility)
// ---------------------------------------------------------------------------

/// Send a message to an actor when a future completes.
///
/// Spawns a task that races the future against the actor's cancellation token.
/// If the actor stops before the future completes, the message is not sent.
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
    use crate::message::Message;
    use std::{
        sync::{atomic, Arc},
        thread,
        time::Duration,
    };

    // --- Counter actor for basic tests ---

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
            struct SlowOp;
            impl Message for SlowOp {
                type Result = ();
            }
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
            let result = request(&*recipient, GetCount, Duration::from_secs(5))
                .await
                .unwrap();
            assert_eq!(result, 42);
        });
    }

    // --- SlowShutdownActor for join tests ---

    struct SlowShutdownActor;

    struct StopSlow;
    impl Message for StopSlow {
        type Result = ();
    }

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
                "Ticker should have completed ~10 ticks during the 500ms join(), but only got {count_after_join}. \
                 This suggests join() blocked the runtime."
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

    struct DoBlock;
    impl Message for DoBlock {
        type Result = ();
    }

    impl Actor for BadlyBehavedTask {}

    impl Handler<DoBlock> for BadlyBehavedTask {
        async fn handle(&mut self, _msg: DoBlock, ctx: &Context<Self>) {
            rt::sleep(Duration::from_millis(20)).await;
            thread::sleep(Duration::from_secs(2));
            ctx.stop();
        }
    }

    struct IncrementWell;
    impl Message for IncrementWell {
        type Result = ();
    }

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

    // --- Panic recovery tests ---

    #[test]
    pub fn panic_in_started_stops_actor() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            struct PanicOnStart;
            struct Ping;
            impl Message for Ping {
                type Result = ();
            }
            impl Actor for PanicOnStart {
                async fn started(&mut self, _ctx: &Context<Self>) {
                    panic!("boom in started");
                }
            }
            impl Handler<Ping> for PanicOnStart {
                async fn handle(&mut self, _msg: Ping, _ctx: &Context<Self>) {}
            }

            let actor = PanicOnStart.start();
            rt::sleep(Duration::from_millis(50)).await;
            let result = actor.send(Ping);
            assert!(result.is_err());
        });
    }

    #[test]
    pub fn panic_in_handler_stops_actor() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            struct PanicOnMsg;
            struct Explode;
            impl Message for Explode {
                type Result = ();
            }
            struct Check;
            impl Message for Check {
                type Result = u32;
            }
            impl Actor for PanicOnMsg {}
            impl Handler<Explode> for PanicOnMsg {
                async fn handle(&mut self, _msg: Explode, _ctx: &Context<Self>) {
                    panic!("boom in handler");
                }
            }
            impl Handler<Check> for PanicOnMsg {
                async fn handle(&mut self, _msg: Check, _ctx: &Context<Self>) -> u32 {
                    42
                }
            }

            let actor = PanicOnMsg.start();
            actor.send(Explode).unwrap();
            rt::sleep(Duration::from_millis(50)).await;
            let result = actor.request(Check).await;
            assert!(result.is_err());
        });
    }

    #[test]
    pub fn panic_in_stopped_still_completes() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            struct PanicOnStop;
            struct StopMe;
            impl Message for StopMe {
                type Result = ();
            }
            impl Actor for PanicOnStop {
                async fn stopped(&mut self, _ctx: &Context<Self>) {
                    panic!("boom in stopped");
                }
            }
            impl Handler<StopMe> for PanicOnStop {
                async fn handle(&mut self, _msg: StopMe, ctx: &Context<Self>) {
                    ctx.stop();
                }
            }

            let actor = PanicOnStop.start();
            actor.send(StopMe).unwrap();
            actor.join().await;
        });
    }

    #[test]
    pub fn send_message_on_delivers() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let counter = Counter { count: 0 }.start();
            let ctx = counter.context();
            send_message_on(ctx, rt::sleep(Duration::from_millis(10)), Increment);
            rt::sleep(Duration::from_millis(100)).await;
            let count = counter.request(GetCount).await.unwrap();
            assert_eq!(count, 1);
        });
    }

    #[test]
    pub fn send_message_on_cancelled() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let counter = Counter { count: 0 }.start();
            let ctx = counter.context();
            send_message_on(ctx, rt::sleep(Duration::from_millis(200)), Increment);
            // Stop actor before the future resolves
            let final_count = counter.request(StopCounter).await.unwrap();
            assert_eq!(final_count, 0, "message should not have been delivered");
            counter.join().await;
        });
    }

    // --- ExitReason tests ---

    #[test]
    pub fn exit_reason_normal_on_clean_stop() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let actor = Counter { count: 0 }.start();
            actor.request(StopCounter).await.unwrap();
            let reason = actor.wait_exit().await;
            assert!(matches!(reason, ExitReason::Normal));
        });
    }

    #[test]
    pub fn exit_reason_panic_in_started() {
        struct PanicStart;
        struct Ping;
        impl Message for Ping {
            type Result = ();
        }
        impl Actor for PanicStart {
            async fn started(&mut self, _ctx: &Context<Self>) {
                panic!("boom in started");
            }
        }
        impl Handler<Ping> for PanicStart {
            async fn handle(&mut self, _msg: Ping, _ctx: &Context<Self>) {}
        }

        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let actor = PanicStart.start();
            let reason = actor.wait_exit().await;
            assert!(
                matches!(reason, ExitReason::Panic(ref msg) if msg.contains("boom in started"))
            );
        });
    }

    #[test]
    pub fn exit_reason_panic_in_handler() {
        struct PanicHandler;
        struct Explode;
        impl Message for Explode {
            type Result = ();
        }
        impl Actor for PanicHandler {}
        impl Handler<Explode> for PanicHandler {
            async fn handle(&mut self, _msg: Explode, _ctx: &Context<Self>) {
                panic!("boom in handler");
            }
        }

        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let actor = PanicHandler.start();
            let _ = actor.send(Explode);
            let reason = actor.wait_exit().await;
            assert!(
                matches!(reason, ExitReason::Panic(ref msg) if msg.contains("boom in handler"))
            );
        });
    }

    #[test]
    pub fn exit_reason_poll_returns_none_while_running() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let actor = Counter { count: 0 }.start();
            assert!(actor.exit_reason().is_none());
            actor.request(StopCounter).await.unwrap();
            actor.join().await;
            assert!(actor.exit_reason().is_some());
        });
    }

    // --- Monitor tests ---

    struct GetDowns;
    impl Message for GetDowns {
        type Result = Vec<crate::monitor::Down>;
    }

    /// Actor that exposes `monitor`/`demonitor` via messages, so tests can
    /// drive it from outside. Records all received Down messages.
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
        async fn handle(
            &mut self,
            msg: StartMonitor,
            ctx: &Context<Self>,
        ) -> crate::monitor::MonitorRef {
            ctx.monitor(&msg.0)
        }
    }

    impl Handler<CallDemonitor> for Watcher {
        async fn handle(&mut self, msg: CallDemonitor, ctx: &Context<Self>) {
            ctx.demonitor(msg.0);
        }
    }

    impl Handler<crate::monitor::Down> for Watcher {
        async fn handle(&mut self, msg: crate::monitor::Down, _ctx: &Context<Self>) {
            self.downs.lock().unwrap().push(msg);
        }
    }

    impl Handler<GetDowns> for Watcher {
        async fn handle(
            &mut self,
            _msg: GetDowns,
            _ctx: &Context<Self>,
        ) -> Vec<crate::monitor::Down> {
            self.downs.lock().unwrap().clone()
        }
    }

    #[test]
    pub fn monitor_running_actor_delivers_down_on_exit() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let target = Counter { count: 0 }.start();
            let target_handle = target.child_handle();

            let watcher = Watcher {
                downs: Arc::new(Mutex::new(Vec::new())),
            }
            .start();

            let monitor_ref = watcher.request(StartMonitor(target_handle)).await.unwrap();

            // Stop the target — Down should be delivered
            target.request(StopCounter).await.unwrap();
            target.join().await;

            // Give the watcher task time to deliver the message
            rt::sleep(Duration::from_millis(50)).await;

            let downs = watcher.request(GetDowns).await.unwrap();
            assert_eq!(downs.len(), 1);
            assert_eq!(downs[0].monitor_ref, monitor_ref);
            assert!(matches!(downs[0].reason, ExitReason::Normal));
        });
    }

    #[test]
    pub fn monitor_already_dead_actor_delivers_down_immediately() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let target = Counter { count: 0 }.start();
            target.request(StopCounter).await.unwrap();
            target.join().await;
            let target_handle = target.child_handle();

            let watcher = Watcher {
                downs: Arc::new(Mutex::new(Vec::new())),
            }
            .start();

            let _ = watcher.request(StartMonitor(target_handle)).await.unwrap();

            // Wait for the watcher task to deliver Down
            rt::sleep(Duration::from_millis(50)).await;

            let downs = watcher.request(GetDowns).await.unwrap();
            assert_eq!(downs.len(), 1);
        });
    }

    #[test]
    pub fn demonitor_before_target_dies_suppresses_down() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let target = Counter { count: 0 }.start();
            let target_handle = target.child_handle();

            let watcher = Watcher {
                downs: Arc::new(Mutex::new(Vec::new())),
            }
            .start();

            let monitor_ref = watcher.request(StartMonitor(target_handle)).await.unwrap();
            watcher.request(CallDemonitor(monitor_ref)).await.unwrap();

            // Now stop the target
            target.request(StopCounter).await.unwrap();
            target.join().await;
            rt::sleep(Duration::from_millis(50)).await;

            let downs = watcher.request(GetDowns).await.unwrap();
            assert!(downs.is_empty(), "expected no Down, got {:?}", downs.len());
        });
    }

    #[test]
    pub fn multiple_monitors_each_get_own_ref_and_down() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let target = Counter { count: 0 }.start();
            let target_handle = target.child_handle();

            let watcher = Watcher {
                downs: Arc::new(Mutex::new(Vec::new())),
            }
            .start();

            let r1 = watcher
                .request(StartMonitor(target_handle.clone()))
                .await
                .unwrap();
            let r2 = watcher.request(StartMonitor(target_handle)).await.unwrap();
            assert_ne!(r1, r2);

            target.request(StopCounter).await.unwrap();
            target.join().await;
            rt::sleep(Duration::from_millis(50)).await;

            let downs = watcher.request(GetDowns).await.unwrap();
            assert_eq!(downs.len(), 2);
            let refs: Vec<_> = downs.iter().map(|d| d.monitor_ref).collect();
            assert!(refs.contains(&r1));
            assert!(refs.contains(&r2));
        });
    }

    #[test]
    pub fn monitor_table_is_cleaned_up_after_target_dies() {
        // Watcher should remove its entry from the monitor table after the
        // target dies, so the table doesn't accumulate stale entries.
        struct Inspect;
        impl Message for Inspect {
            type Result = usize;
        }
        impl Handler<Inspect> for Watcher {
            async fn handle(&mut self, _msg: Inspect, ctx: &Context<Self>) -> usize {
                ctx.monitors.lock().unwrap().len()
            }
        }

        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let target = Counter { count: 0 }.start();
            let watcher = Watcher {
                downs: Arc::new(Mutex::new(Vec::new())),
            }
            .start();

            let _ = watcher
                .request(StartMonitor(target.child_handle()))
                .await
                .unwrap();
            assert_eq!(watcher.request(Inspect).await.unwrap(), 1);

            target.request(StopCounter).await.unwrap();
            target.join().await;
            // Give the watcher time to process and clean up
            rt::sleep(Duration::from_millis(50)).await;

            assert_eq!(
                watcher.request(Inspect).await.unwrap(),
                0,
                "monitor table should be empty after target died"
            );
        });
    }

    #[test]
    pub fn monitor_observes_panic_reason() {
        struct PanicMsg;
        impl Message for PanicMsg {
            type Result = ();
        }
        struct PanicMe;
        impl Actor for PanicMe {}
        impl Handler<PanicMsg> for PanicMe {
            async fn handle(&mut self, _msg: PanicMsg, _ctx: &Context<Self>) {
                panic!("intentional panic");
            }
        }

        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let target = PanicMe.start();
            let target_handle = target.child_handle();

            let watcher = Watcher {
                downs: Arc::new(Mutex::new(Vec::new())),
            }
            .start();

            let _ = watcher.request(StartMonitor(target_handle)).await.unwrap();
            let _ = target.send(PanicMsg);

            // Wait for target to panic and watcher to deliver
            rt::sleep(Duration::from_millis(100)).await;

            let downs = watcher.request(GetDowns).await.unwrap();
            assert_eq!(downs.len(), 1);
            assert!(matches!(downs[0].reason, ExitReason::Panic(_)));
        });
    }

    #[test]
    pub fn monitoring_actor_stops_before_target_does_not_panic() {
        // Regression: if the monitoring actor stops while the target is still
        // alive, the watcher must not crash when the target eventually dies.
        // The watcher's send() will fail silently (mailbox closed), and the
        // watcher exits cleanly.
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let target = Counter { count: 0 }.start();
            let target_handle = target.child_handle();

            let watcher = Watcher {
                downs: Arc::new(Mutex::new(Vec::new())),
            }
            .start();

            let _ = watcher.request(StartMonitor(target_handle)).await.unwrap();

            // Stop the monitoring actor first
            let watcher_handle = watcher.child_handle();
            watcher_handle.stop();
            watcher_handle.wait_exit_async().await;

            // Now stop the target — the watcher should clean up without panicking
            target.request(StopCounter).await.unwrap();
            target.join().await;
            // Give the orphaned watcher task time to attempt delivery
            rt::sleep(Duration::from_millis(50)).await;
            // If we got here without panicking, the test passes.
        });
    }

    // --- Link tests ---

    /// Trapping actor that records `Exit` notifications.
    struct TrapActor {
        exits: Arc<Mutex<Vec<Exit>>>,
        trap: bool,
    }

    struct GetExits;
    impl Message for GetExits {
        type Result = Vec<Exit>;
    }
    struct LinkTo(crate::ChildHandle);
    impl Message for LinkTo {
        type Result = ();
    }

    impl Actor for TrapActor {
        async fn started(&mut self, ctx: &Context<Self>) {
            ctx.trap_exit(self.trap);
        }
        async fn exit_received(&mut self, exit: Exit, _ctx: &Context<Self>) {
            self.exits.lock().unwrap().push(exit);
        }
    }

    impl Handler<LinkTo> for TrapActor {
        async fn handle(&mut self, msg: LinkTo, ctx: &Context<Self>) {
            ctx.link(&msg.0);
        }
    }

    impl Handler<GetExits> for TrapActor {
        async fn handle(&mut self, _msg: GetExits, _ctx: &Context<Self>) -> Vec<Exit> {
            self.exits.lock().unwrap().clone()
        }
    }

    fn make_trapper(trap: bool) -> ActorRef<TrapActor> {
        TrapActor {
            exits: Arc::new(Mutex::new(Vec::new())),
            trap,
        }
        .start()
    }

    #[test]
    pub fn link_propagates_panic_to_non_trapping_peer() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            // Two non-trapping actors. When A panics, B should be terminated.
            let a = make_trapper(false);
            let b = make_trapper(false);
            a.request(LinkTo(b.child_handle())).await.unwrap();

            // Make A panic by dropping its only ActorRef while it's mid-handler... easier:
            // make A panic via a panic message
            struct Boom;
            impl Message for Boom {
                type Result = ();
            }
            impl Handler<Boom> for TrapActor {
                async fn handle(&mut self, _msg: Boom, _ctx: &Context<Self>) {
                    panic!("boom");
                }
            }

            let _ = a.send(Boom);
            // Both should die
            let reason_a = a.wait_exit().await;
            let reason_b = b.wait_exit().await;
            assert!(matches!(reason_a, ExitReason::Panic(_)));
            // B should propagate A's reason (transitive propagation)
            assert!(matches!(reason_b, ExitReason::Panic(_)));
        });
    }

    #[test]
    pub fn link_delivers_exit_to_trapping_peer() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let a = make_trapper(false);
            let b = make_trapper(true); // B traps
            b.request(LinkTo(a.child_handle())).await.unwrap();

            struct Boom2;
            impl Message for Boom2 {
                type Result = ();
            }
            impl Handler<Boom2> for TrapActor {
                async fn handle(&mut self, _msg: Boom2, _ctx: &Context<Self>) {
                    panic!("boom2");
                }
            }

            let _ = a.send(Boom2);
            a.wait_exit().await;
            // Give B time to process the Exit message
            rt::sleep(Duration::from_millis(50)).await;

            // B should still be alive and have received an Exit
            assert!(b.exit_reason().is_none());
            let exits = b.request(GetExits).await.unwrap();
            assert_eq!(exits.len(), 1);
            assert_eq!(exits[0].from, a.id());
            assert!(matches!(exits[0].reason, ExitReason::Panic(_)));

            // Clean up
            let bh = b.child_handle();
            bh.stop();
            bh.wait_exit_async().await;
        });
    }

    #[test]
    pub fn link_normal_exit_not_propagated_to_non_trapping_peer() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let a = Counter { count: 0 }.start();
            let b = make_trapper(false);
            b.request(LinkTo(a.child_handle())).await.unwrap();

            // A stops cleanly
            a.request(StopCounter).await.unwrap();
            a.join().await;
            rt::sleep(Duration::from_millis(50)).await;

            // B should still be alive
            assert!(b.exit_reason().is_none());

            let bh = b.child_handle();
            bh.stop();
            bh.wait_exit_async().await;
        });
    }

    #[test]
    pub fn link_to_already_dead_actor_delivers_signal() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            // Start and panic a target
            struct Boom3;
            impl Message for Boom3 {
                type Result = ();
            }
            impl Handler<Boom3> for TrapActor {
                async fn handle(&mut self, _msg: Boom3, _ctx: &Context<Self>) {
                    panic!("boom3");
                }
            }
            let target = make_trapper(false);
            let _ = target.send(Boom3);
            target.wait_exit().await;

            // Now link from a trapping observer — should receive Exit immediately
            let observer = make_trapper(true);
            observer
                .request(LinkTo(target.child_handle()))
                .await
                .unwrap();

            rt::sleep(Duration::from_millis(50)).await;
            let exits = observer.request(GetExits).await.unwrap();
            assert_eq!(exits.len(), 1);
            assert!(matches!(exits[0].reason, ExitReason::Panic(_)));

            let oh = observer.child_handle();
            oh.stop();
            oh.wait_exit_async().await;
        });
    }

    #[test]
    pub fn unlink_prevents_signal_delivery() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let a = Counter { count: 0 }.start();
            let b = make_trapper(true);
            b.request(LinkTo(a.child_handle())).await.unwrap();

            // Now unlink
            struct UnlinkFrom(crate::ChildHandle);
            impl Message for UnlinkFrom {
                type Result = ();
            }
            impl Handler<UnlinkFrom> for TrapActor {
                async fn handle(&mut self, msg: UnlinkFrom, ctx: &Context<Self>) {
                    ctx.unlink(&msg.0);
                }
            }
            b.request(UnlinkFrom(a.child_handle())).await.unwrap();

            // A panics
            struct Boom4;
            impl Message for Boom4 {
                type Result = u64;
            }
            impl Handler<Boom4> for Counter {
                async fn handle(&mut self, _msg: Boom4, _ctx: &Context<Self>) -> u64 {
                    panic!("boom4");
                }
            }
            // Counter's Boom4 returns u64; just use send with no expectation of reply
            let _ = a.send(Boom4);
            a.wait_exit().await;
            rt::sleep(Duration::from_millis(50)).await;

            // B should NOT have received an Exit
            let exits = b.request(GetExits).await.unwrap();
            assert!(exits.is_empty());

            let bh = b.child_handle();
            bh.stop();
            bh.wait_exit_async().await;
        });
    }

    #[test]
    pub fn duplicate_link_is_idempotent() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let a = Counter { count: 0 }.start();
            let b = make_trapper(true);
            b.request(LinkTo(a.child_handle())).await.unwrap();
            b.request(LinkTo(a.child_handle())).await.unwrap(); // duplicate

            struct Boom5;
            impl Message for Boom5 {
                type Result = u64;
            }
            impl Handler<Boom5> for Counter {
                async fn handle(&mut self, _msg: Boom5, _ctx: &Context<Self>) -> u64 {
                    panic!("boom5");
                }
            }
            let _ = a.send(Boom5);
            a.wait_exit().await;
            rt::sleep(Duration::from_millis(50)).await;

            // Should receive only ONE Exit, not two
            let exits = b.request(GetExits).await.unwrap();
            assert_eq!(exits.len(), 1);

            let bh = b.child_handle();
            bh.stop();
            bh.wait_exit_async().await;
        });
    }

    #[test]
    pub fn chain_propagation_through_non_trapping_middle() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            // A linked to B linked to C. A panics → B dies (not trapping) →
            // B's death propagates to C.
            let a = make_trapper(false);
            let b = make_trapper(false);
            let c = make_trapper(true); // C traps

            a.request(LinkTo(b.child_handle())).await.unwrap();
            c.request(LinkTo(b.child_handle())).await.unwrap();

            struct Boom6;
            impl Message for Boom6 {
                type Result = ();
            }
            impl Handler<Boom6> for TrapActor {
                async fn handle(&mut self, _msg: Boom6, _ctx: &Context<Self>) {
                    panic!("boom6");
                }
            }
            let _ = a.send(Boom6);
            a.wait_exit().await;
            b.wait_exit().await;
            rt::sleep(Duration::from_millis(100)).await;

            // C should have received an Exit (B's death propagated)
            let exits = c.request(GetExits).await.unwrap();
            assert_eq!(exits.len(), 1, "expected C to receive Exit from B's death");

            let ch = c.child_handle();
            ch.stop();
            ch.wait_exit_async().await;
        });
    }

    #[test]
    pub fn link_to_already_dead_delivers_exactly_once_to_trapping_peer() {
        // Regression: previously, `ctx.link()` would deliver a duplicate Exit
        // to trapping actors when the target died concurrently — once from
        // propagate_exit (since register_link inserted self in target's table
        // before target's drain ran) and once from the fallback exit_reason()
        // check.
        struct Boom7;
        impl Message for Boom7 {
            type Result = ();
        }
        impl Handler<Boom7> for TrapActor {
            async fn handle(&mut self, _msg: Boom7, _ctx: &Context<Self>) {
                panic!("boom7");
            }
        }

        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let target = make_trapper(false);
            // Kick off the panic
            let _ = target.send(Boom7);
            // Wait for target to fully die so exit_reason() is Some
            target.wait_exit().await;

            // Now link from a trapping observer — should receive Exit EXACTLY ONCE
            let observer = make_trapper(true);
            observer
                .request(LinkTo(target.child_handle()))
                .await
                .unwrap();

            rt::sleep(Duration::from_millis(100)).await;
            let exits = observer.request(GetExits).await.unwrap();
            assert_eq!(exits.len(), 1, "expected exactly one Exit, got {:?}", exits);

            let oh = observer.child_handle();
            oh.stop();
            oh.wait_exit_async().await;
        });
    }

    #[test]
    pub fn start_linked_links_atomically() {
        // Parent + child via start_linked. When parent stops via panic,
        // child (not trapping) should die too.
        struct Parent;
        struct PanicParent;
        impl Message for PanicParent {
            type Result = ();
        }
        impl Actor for Parent {}
        impl Handler<PanicParent> for Parent {
            async fn handle(&mut self, _msg: PanicParent, _ctx: &Context<Self>) {
                panic!("parent boom");
            }
        }

        struct Child;
        impl Actor for Child {}

        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let parent = Parent.start();
            let child = Child.start_linked(&parent.context());

            // Parent panics — child should die too
            let _ = parent.send(PanicParent);
            parent.wait_exit().await;
            // Child should also die (link propagation)
            let reason = child.wait_exit().await;
            // Reason should be the parent's panic
            assert!(matches!(reason, ExitReason::Panic(_)));
        });
    }
}
