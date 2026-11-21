//! Unified start (0.9, design `docs/design/unified-start.md`), threads mode:
//! the same verbs as tasks mode; the waiting forms block. Every actor owns
//! a thread, so `StartOptions` has no placement.
use super::*;
use crate::start::{Observed, StartError, StartOutcome, Timeout};
use std::borrow::Cow;
use std::fmt::Display;
use std::marker::PhantomData;

/// What `started()` returns (OTP `init/1`'s results); see the tasks-mode
/// `Init` (`spawned_concurrency::tasks::Init`) for the table. No failed start runs
/// `stopped()`.
#[must_use]
pub struct Init<A: Actor> {
    pub(super) kind: InitKind<A>,
}

pub(super) enum InitKind<A: Actor> {
    Ready,
    Continue(Box<dyn Envelope<A> + Send>),
    Error(String),
    Stop(ExitReason),
    Ignore,
}

impl<A: Actor> std::fmt::Debug for Init<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = match &self.kind {
            InitKind::Ready => "ready",
            InitKind::Continue(_) => "continue",
            InitKind::Error(_) => "error",
            InitKind::Stop(_) => "stop",
            InitKind::Ignore => "ignore",
        };
        f.debug_tuple("Init").field(&kind).finish()
    }
}

impl<A: Actor> Init<A> {
    /// Ready (`{ok, S}`).
    pub fn ready() -> Self {
        Self {
            kind: InitKind::Ready,
        }
    }
    /// Ready, then handle `msg` before any mailbox entry (`handle_continue`).
    /// Not a barrier against termination.
    pub fn continue_with<M: Message>(msg: M) -> Self
    where
        A: Handler<M>,
    {
        Self {
            kind: InitKind::Continue(Box::new(MessageEnvelope {
                slot: SlotHandle::detached(),
                msg,
                #[cfg(feature = "metrics")]
                observation: None,
                #[cfg(feature = "lifecycle-trace")]
                from: 0,
            })),
        }
    }
    /// A failed initialisation (`{error, R}`): the actor ends `Normal`.
    pub fn error(cause: impl Display) -> Self {
        Self {
            kind: InitKind::Error(cause.to_string()),
        }
    }
    /// Stop with `reason` (`{stop, R}`).
    pub fn stop(reason: ExitReason) -> Self {
        Self {
            kind: InitKind::Stop(reason),
        }
    }
    /// Opt out (`ignore`): the actor ends `Normal`.
    pub fn ignore() -> Self {
        Self {
            kind: InitKind::Ignore,
        }
    }
    pub(super) fn into_returned(
        self,
    ) -> (crate::start::Returned, Option<Box<dyn Envelope<A> + Send>>) {
        use crate::start::Returned;
        match self.kind {
            InitKind::Ready => (Returned::Ready, None),
            InitKind::Continue(envelope) => (Returned::Ready, Some(envelope)),
            InitKind::Error(cause) => (Returned::Error(cause), None),
            InitKind::Stop(reason) => (Returned::Stop(reason), None),
            InitKind::Ignore => (Returned::Ignore, None),
        }
    }
}

impl<A: Actor> From<()> for Init<A> {
    fn from(_: ()) -> Self {
        Self::ready()
    }
}

impl<A: Actor, E: Display> From<Result<(), E>> for Init<A> {
    fn from(result: Result<(), E>) -> Self {
        match result {
            Ok(()) => Self::ready(),
            Err(error) => Self::error(error),
        }
    }
}

impl<A: Actor, E: Display> From<Result<Init<A>, E>> for Init<A> {
    fn from(result: Result<Init<A>, E>) -> Self {
        result.unwrap_or_else(Self::error)
    }
}

/// A registered name for actors of type `A`, committed with the start and
/// released at publication.
pub struct Name<A: Actor> {
    key: Cow<'static, str>,
    _actor: PhantomData<fn() -> A>,
}

impl<A: Actor> Name<A> {
    pub const fn new(key: &'static str) -> Self {
        Self {
            key: Cow::Borrowed(key),
            _actor: PhantomData,
        }
    }
    pub fn from_string(key: String) -> Self {
        Self {
            key: Cow::Owned(key),
            _actor: PhantomData,
        }
    }
    pub fn key(&self) -> &str {
        &self.key
    }
    /// The live instance holding this name, if it is an `A`.
    pub fn whereis(&self) -> Option<ActorRef<A>> {
        crate::registry::whereis_start::<ActorRef<A>>(&self.key)
    }
}

impl<A: Actor> Clone for Name<A> {
    fn clone(&self) -> Self {
        Self {
            key: self.key.clone(),
            _actor: PhantomData,
        }
    }
}

impl<A: Actor> std::fmt::Debug for Name<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Name").field(&self.key).finish()
    }
}

impl<A: Actor> From<&'static str> for Name<A> {
    fn from(key: &'static str) -> Self {
        Self::new(key)
    }
}

impl<A: Actor> From<String> for Name<A> {
    fn from(key: String) -> Self {
        Self::from_string(key)
    }
}

/// How to start an actor of type `A` (threads mode: no placement).
#[non_exhaustive]
pub struct StartOptions<A: Actor> {
    pub(super) name: Option<Name<A>>,
    pub(super) label: Option<Arc<str>>,
    pub(super) init_timeout: Timeout,
    pub(super) stack_size: Option<usize>,
    pub(super) max_outstanding: Option<usize>,
    pub(super) max_native_jobs: Option<usize>,
}

impl<A: Actor> Clone for StartOptions<A> {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            label: self.label.clone(),
            init_timeout: self.init_timeout,
            stack_size: self.stack_size,
            max_outstanding: self.max_outstanding,
            max_native_jobs: self.max_native_jobs,
        }
    }
}

impl<A: Actor> Default for StartOptions<A> {
    fn default() -> Self {
        Self::new()
    }
}

impl<A: Actor> std::fmt::Debug for StartOptions<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StartOptions")
            .field("name", &self.name)
            .field("init_timeout", &self.init_timeout)
            .finish_non_exhaustive()
    }
}

impl<A: Actor> StartOptions<A> {
    pub fn new() -> Self {
        Self {
            name: None,
            label: None,
            init_timeout: Timeout::Infinity,
            stack_size: None,
            max_outstanding: None,
            max_native_jobs: None,
        }
    }
    /// Register the actor under `name` atomically with the start.
    pub fn name(mut self, name: impl Into<Name<A>>) -> Self {
        self.name = Some(name.into());
        self
    }
    /// A label for diagnostics (the actor thread's name).
    pub fn label(mut self, label: impl Into<Arc<str>>) -> Self {
        self.label = Some(label.into());
        self
    }
    /// Bound `started()` (default `Infinity`).
    pub fn init_timeout(mut self, timeout: impl Into<Timeout>) -> Self {
        self.init_timeout = timeout.into();
        self
    }
    /// Stack size of the actor's thread.
    pub fn stack_size(mut self, bytes: usize) -> Self {
        self.stack_size = Some(bytes);
        self
    }
    /// Per-instance override of `Actor::MAX_OUTSTANDING`.
    pub fn max_outstanding(mut self, max: usize) -> Self {
        self.max_outstanding = Some(max);
        self
    }
    /// Per-instance override of `Actor::MAX_NATIVE_JOBS`.
    pub fn max_native_jobs(mut self, max: usize) -> Self {
        self.max_native_jobs = Some(max);
        self
    }
    pub(super) fn validate(&self) -> Result<(), StartError> {
        if self.init_timeout == Timeout::After(Duration::ZERO) {
            return Err(StartError::InvalidOptions("init_timeout must be nonzero"));
        }
        if self.max_outstanding == Some(0) {
            return Err(StartError::InvalidOptions(
                "max_outstanding must be nonzero",
            ));
        }
        if self.max_native_jobs == Some(0) {
            return Err(StartError::InvalidOptions(
                "max_native_jobs must be nonzero",
            ));
        }
        if self.stack_size == Some(0) {
            return Err(StartError::InvalidOptions("stack_size must be nonzero"));
        }
        Ok(())
    }
}

/// How a start was requested.
pub(super) enum Verb {
    Spawn,
    Start,
    Link(ParentParts),
    Monitor(ParentParts),
}

impl Verb {
    pub(super) fn trace_name(&self) -> &'static str {
        match self {
            Self::Spawn => "spawn",
            Self::Start => "start",
            Self::Link(_) => "link",
            Self::Monitor(_) => "monitor",
        }
    }
    pub(super) fn waiting(&self) -> bool {
        !matches!(self, Self::Spawn)
    }
}

/// The parent side of a `start_link`/`start_monitor` (type-erased).
pub(super) struct ParentParts {
    pub(super) id: ActorId,
    pub(super) signal: Arc<SignalTarget>,
    pub(super) links: LinkTable,
    pub(super) request: LinkedExitReason,
    pub(super) monitors: MonitorTable,
    pub(super) token: CancellationToken,
}

/// Arms the parent's monitor waiter (typed by the parent) before dispatch.
pub(super) type ArmMonitor = Box<
    dyn FnOnce(
            ChildHandle,
            MonitorRef,
            spawned_rt::sync::CancellationToken,
            Arc<crate::start::StartupCell>,
        ) + Send,
>;

/// Starting an actor; the waiting forms block until `started()` returns
/// (and, for a failure, until the actor is published).
pub trait ActorStart: Actor {
    /// Start without waiting (`proc_lib:spawn`).
    fn spawn(self) -> ActorRef<Self> {
        Starter::new(self, StartOptions::new())
            .spawn()
            .unwrap_or_else(|error| panic!("actor spawn failed: {error}"))
    }
    /// Start and wait for `started()` (`gen_server:start`).
    fn start(self) -> Result<StartOutcome<ActorRef<Self>>, StartError> {
        Starter::new(self, StartOptions::new()).start()
    }
    /// Start linked to `parent`'s actor and wait (`gen_server:start_link`).
    fn start_link<P: Actor>(
        self,
        parent: &Context<P>,
    ) -> Result<StartOutcome<ActorRef<Self>>, StartError> {
        Starter::new(self, StartOptions::new()).start_link(parent)
    }
    /// Start monitored by `parent`'s actor and wait
    /// (`gen_server:start_monitor`).
    fn start_monitor<P: Actor + Handler<Down>>(
        self,
        parent: &Context<P>,
    ) -> Result<StartOutcome<(ActorRef<Self>, MonitorRef)>, StartError> {
        Starter::new(self, StartOptions::new()).start_monitor(parent)
    }
    /// Start with options.
    fn with_options(self, options: impl Into<StartOptions<Self>>) -> Starter<Self> {
        Starter::new(self, options.into())
    }
}

impl<A: Actor> ActorStart for A {}

/// An actor and its start options; choose a verb.
#[must_use = "call spawn, start, start_link or start_monitor"]
pub struct Starter<A: Actor> {
    actor: A,
    options: StartOptions<A>,
}

impl<A: Actor> Starter<A> {
    fn new(actor: A, options: StartOptions<A>) -> Self {
        Self { actor, options }
    }
    /// Start without waiting.
    pub fn spawn(self) -> Result<ActorRef<A>, StartError> {
        ActorRef::launch(self.actor, self.options, Verb::Spawn, None).map(|(actor, _)| actor)
    }
    /// Start and wait for `started()`.
    pub fn start(self) -> Result<StartOutcome<ActorRef<A>>, StartError> {
        let (actor, _) = ActorRef::launch(self.actor, self.options, Verb::Start, None)?;
        wait(&actor, true).map(|o| o.map(|()| actor))
    }
    /// Start linked to `parent`'s actor and wait.
    pub fn start_link<P: Actor>(
        self,
        parent: &Context<P>,
    ) -> Result<StartOutcome<ActorRef<A>>, StartError> {
        let verb = Verb::Link(parent.parent_parts());
        let (actor, _) = ActorRef::launch(self.actor, self.options, verb, None)?;
        wait(&actor, true).map(|o| o.map(|()| actor))
    }
    /// Start monitored by `parent`'s actor and wait.
    pub fn start_monitor<P: Actor + Handler<Down>>(
        self,
        parent: &Context<P>,
    ) -> Result<StartOutcome<(ActorRef<A>, MonitorRef)>, StartError> {
        let verb = Verb::Monitor(parent.parent_parts());
        let parent = parent.clone();
        let arm: ArmMonitor = Box::new(move |child, monitor_ref, active, cell| {
            parent.arm_start_monitor(child, monitor_ref, active, cell)
        });
        let (actor, monitor) = ActorRef::launch(self.actor, self.options, verb, Some(arm))?;
        let monitor = monitor.expect("start_monitor commits a monitor");
        wait(&actor, true).map(|o| o.map(|()| (actor, monitor)))
    }
}

/// Block for the startup result of `actor` (`waiter`: the start's own
/// wait, which consumes it).
pub(super) fn wait<A: Actor>(
    actor: &ActorRef<A>,
    waiter: bool,
) -> Result<StartOutcome<()>, StartError> {
    let cell = actor.shared.signal.startup.clone();
    loop {
        let seen = cell.version();
        match cell.observe(waiter, || actor.child_handle()) {
            Observed::Answer(answer) => return answer,
            Observed::Wait(deadline) => cell.wait_blocking(seen, deadline),
        }
    }
}
