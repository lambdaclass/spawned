use spawned_rt::threads::{
    self as rt, mpsc, oneshot, oneshot::RecvTimeoutError, CancellationToken,
};
use std::{
    fmt::Debug,
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{Arc, Condvar, Mutex},
    time::Duration,
};

use crate::error::ActorError;
use crate::message::Message;

const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// Actor trait
// ---------------------------------------------------------------------------

pub trait Actor: Send + Sized + 'static {
    fn started(&mut self, _ctx: &Context<Self>) {}
    fn stopped(&mut self, _ctx: &Context<Self>) {}
}

// ---------------------------------------------------------------------------
// Handler trait (per-message, sync version)
// ---------------------------------------------------------------------------

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

pub struct Context<A: Actor> {
    sender: mpsc::Sender<Box<dyn Envelope<A> + Send>>,
    cancellation_token: CancellationToken,
    completion: Arc<(Mutex<bool>, Condvar)>,
}

impl<A: Actor> Clone for Context<A> {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            cancellation_token: self.cancellation_token.clone(),
            completion: self.completion.clone(),
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
            completion: actor_ref.completion.clone(),
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

    pub fn request<M>(&self, msg: M) -> Result<M::Result, ActorError>
    where
        A: Handler<M>,
        M: Message,
    {
        self.request_with_timeout(msg, DEFAULT_REQUEST_TIMEOUT)
    }

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

    pub fn recipient<M>(&self) -> Recipient<M>
    where
        A: Handler<M>,
        M: Message,
    {
        Arc::new(self.clone())
    }

    pub fn actor_ref(&self) -> ActorRef<A> {
        ActorRef {
            sender: self.sender.clone(),
            cancellation_token: self.cancellation_token.clone(),
            completion: self.completion.clone(),
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

pub trait Receiver<M: Message>: Send + Sync {
    fn send(&self, msg: M) -> Result<(), ActorError>;
    fn request_raw(&self, msg: M) -> Result<oneshot::Receiver<M::Result>, ActorError>;
}

pub type Recipient<M> = Arc<dyn Receiver<M>>;

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

struct CompletionGuard(Arc<(Mutex<bool>, Condvar)>);

impl Drop for CompletionGuard {
    fn drop(&mut self) {
        let (lock, cvar) = &*self.0;
        let mut completed = lock.lock().unwrap_or_else(|p| p.into_inner());
        *completed = true;
        cvar.notify_all();
    }
}

pub struct ActorRef<A: Actor> {
    sender: mpsc::Sender<Box<dyn Envelope<A> + Send>>,
    cancellation_token: CancellationToken,
    completion: Arc<(Mutex<bool>, Condvar)>,
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
            completion: self.completion.clone(),
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

    pub fn request<M>(&self, msg: M) -> Result<M::Result, ActorError>
    where
        A: Handler<M>,
        M: Message,
    {
        self.request_with_timeout(msg, DEFAULT_REQUEST_TIMEOUT)
    }

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

    pub fn join(&self) {
        let (lock, cvar) = &*self.completion;
        let mut completed = lock.lock().unwrap_or_else(|p| p.into_inner());
        while !*completed {
            completed = cvar.wait(completed).unwrap_or_else(|p| p.into_inner());
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
    fn spawn(actor: A) -> Self {
        let (tx, rx) = mpsc::channel::<Box<dyn Envelope<A> + Send>>();
        let cancellation_token = CancellationToken::new();
        let completion = Arc::new((Mutex::new(false), Condvar::new()));

        let actor_ref = ActorRef {
            sender: tx.clone(),
            cancellation_token: cancellation_token.clone(),
            completion: completion.clone(),
        };

        let ctx = Context {
            sender: tx,
            cancellation_token: cancellation_token.clone(),
            completion: actor_ref.completion.clone(),
        };

        let _thread_handle = rt::spawn(move || {
            let _guard = CompletionGuard(completion);
            run_actor(actor, ctx, rx, cancellation_token);
        });

        actor_ref
    }
}

fn run_actor<A: Actor>(
    mut actor: A,
    ctx: Context<A>,
    rx: mpsc::Receiver<Box<dyn Envelope<A> + Send>>,
    cancellation_token: CancellationToken,
) {
    actor.started(&ctx);

    if cancellation_token.is_cancelled() {
        actor.stopped(&ctx);
        return;
    }

    loop {
        let msg = rx.recv().ok();
        match msg {
            Some(envelope) => {
                let result = catch_unwind(AssertUnwindSafe(|| {
                    envelope.handle(&mut actor, &ctx);
                }));
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
    actor.stopped(&ctx);
}

// ---------------------------------------------------------------------------
// Actor::start
// ---------------------------------------------------------------------------

pub trait ActorStart: Actor {
    fn start(self) -> ActorRef<Self> {
        ActorRef::spawn(self)
    }
}

impl<A: Actor> ActorStart for A {}

// ---------------------------------------------------------------------------
// send_message_on (utility)
// ---------------------------------------------------------------------------

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
