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
    let start_result = catch_unwind(AssertUnwindSafe(|| {
        actor.started(&ctx);
    }));
    if let Err(panic) = start_result {
        tracing::error!("Panic in started() callback: {panic:?}");
        return;
    }

    if cancellation_token.is_cancelled() {
        let _ = catch_unwind(AssertUnwindSafe(|| actor.stopped(&ctx)));
        return;
    }

    loop {
        let msg = match rx.recv_timeout(Duration::from_millis(100)) {
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
    let stop_result = catch_unwind(AssertUnwindSafe(|| {
        actor.stopped(&ctx);
    }));
    if let Err(panic) = stop_result {
        tracing::error!("Panic in stopped() callback: {panic:?}");
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::Message;
    use std::thread;

    struct Counter {
        count: u64,
    }

    struct GetCount;
    impl Message for GetCount { type Result = u64; }

    struct Increment;
    impl Message for Increment { type Result = u64; }

    struct StopCounter;
    impl Message for StopCounter { type Result = u64; }

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
        impl Message for StopSlow { type Result = (); }
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
        impl Message for StopSlow2 { type Result = (); }
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
        impl Message for PingThread { type Result = (); }
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
        impl Message for ExplodeThread { type Result = (); }
        struct CheckThread;
        impl Message for CheckThread { type Result = u32; }
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
        impl Message for StopMeThread { type Result = (); }
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
}
