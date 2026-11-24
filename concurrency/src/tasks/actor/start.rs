//! Unified start (0.9, design `docs/design/unified-start.md`): the start
//! verbs, `StartOptions`, typed names, `Init` and `Starting`.
use super::*;
pub(super) use crate::start::Timeout;
use crate::start::{Observed, StartError, StartOutcome, StartupCell};
use std::borrow::Cow;
use std::fmt::Display;
use std::marker::PhantomData;

// ---------------------------------------------------------------------------
// Init: what started() returns
// ---------------------------------------------------------------------------

/// What `started()` returns (OTP `init/1`'s results). `started()` may
/// return `()`, `Result<(), E>`, `Result<Init<Self>, E>` or `Init<Self>`.
///
/// | `Init` | OTP | The start answers |
/// |---|---|---|
/// | `ready()` (`()`, `Ok(())`) | `{ok, S}` | `Ok(actor)` |
/// | `continue_with(m)` | `{ok, S, {continue, C}}` | `Ok(actor)`; `m` is handled before any mailbox message |
/// | `error(e)` (`Err(e)`) | `{error, R}` | `InitFailed { cause }`; the actor ends `Normal` |
/// | `stop(r)` | `{stop, R}` | `InitExited { reason: r }` |
/// | `ignore()` | `ignore` | `Err(Ignored)` (not a failure; [`StartOutcome::from_result`](crate::StartOutcome::from_result) makes it `Ok(Ignored)`); the actor ends `Normal` |
///
/// No failed start runs `stopped()` (OTP skips `terminate/2` when `init/1`
/// fails): release resources acquired in `started()` before returning an
/// error, or hold them in RAII guards.
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
    /// Ready, then handle `msg` as the first turn, before any mailbox entry
    /// (`{ok, S, {continue, C}}`, `handle_continue`). Readiness is committed
    /// before it runs, so it is not a barrier against termination: a kill,
    /// an untrapped exit signal or a supervisor's shutdown can end the actor
    /// before or during it.
    pub fn continue_with<M: Message>(msg: M) -> Self
    where
        A: Handler<M>,
    {
        Self {
            kind: InitKind::Continue(Box::new(SendEnvelope {
                msg: Some(msg),
                #[cfg(feature = "metrics")]
                observation: None,
                #[cfg(feature = "lifecycle-trace")]
                from: 0,
            })),
        }
    }
    /// A failed initialisation (`{error, R}`, OTP 26 `init_fail/3`): the
    /// actor ends `Normal`, the start answers `InitFailed { cause }`.
    pub fn error(cause: impl Display) -> Self {
        Self {
            kind: InitKind::Error(cause.to_string()),
        }
    }
    /// Stop with `reason` (`{stop, R}`): links see it. The start answers
    /// `InitExited { reason }`.
    pub fn stop(reason: ExitReason) -> Self {
        Self {
            kind: InitKind::Stop(reason),
        }
    }
    /// Opt out (`ignore`): the actor ends `Normal`; the start answers
    /// `Err(StartError::Ignored)`, which is not a failure.
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

// ---------------------------------------------------------------------------
// Typed names
// ---------------------------------------------------------------------------

/// A registered name for actors of type `A` (OTP `{local, Name}`), committed
/// with the start and released at publication. A `Name<B>` cannot name an
/// `A` start. Names are process-global.
pub struct Name<A: Actor> {
    key: Cow<'static, str>,
    _actor: PhantomData<fn() -> A>,
}

impl<A: Actor> Name<A> {
    /// A name usable in a `static`.
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

// ---------------------------------------------------------------------------
// StartOptions
// ---------------------------------------------------------------------------

/// Where an actor runs.
#[derive(Clone)]
pub(super) enum Placement {
    Group(crate::system::ExecutionGroup),
    Backend(Backend),
}

impl Placement {
    pub(super) fn trace_name(&self) -> &'static str {
        match self {
            Self::Group(_) => "G",
            Self::Backend(_) => "U",
        }
    }
}

/// How to start an actor of type `A`: placement, a typed name, the init
/// timeout, a label, the stack size and per-instance bounds. The last of
/// `group`/`backend`/`placement_of` wins; without one, `start_link` and
/// `start_monitor` use the parent's placement and the other verbs an
/// unmanaged `Backend::Async`.
#[non_exhaustive]
pub struct StartOptions<A: Actor> {
    pub(super) placement: Option<Placement>,
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
            placement: self.placement.clone(),
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
            placement: None,
            name: None,
            label: None,
            init_timeout: Timeout::Infinity,
            stack_size: None,
            max_outstanding: None,
            max_native_jobs: None,
        }
    }
    /// Admit the actor to an execution group of an `ActorSystem`.
    pub fn group(mut self, group: &crate::system::ExecutionGroup) -> Self {
        self.placement = Some(Placement::Group(group.clone()));
        self
    }
    /// Run the actor unmanaged on `backend`.
    pub fn backend(mut self, backend: Backend) -> Self {
        self.placement = Some(Placement::Backend(backend));
        self
    }
    /// Use the placement of `ctx`'s actor (its group, or its backend).
    pub fn placement_of<P: Actor>(mut self, ctx: &Context<P>) -> Self {
        self.placement = Some(ctx.placement());
        self
    }
    /// Register the actor under `name` atomically with the start; the name
    /// is released at publication, before any notice or answer.
    pub fn name(mut self, name: impl Into<Name<A>>) -> Self {
        self.name = Some(name.into());
        self
    }
    /// A label for diagnostics: a managed actor's diagnostic name when it
    /// has no registered name, and the OS thread name of a
    /// `Backend::Thread` actor. An unmanaged `Async` or `Blocking` actor has
    /// no place to show it, and ignores it.
    pub fn label(mut self, label: impl Into<Arc<str>>) -> Self {
        self.label = Some(label.into());
        self
    }
    /// Bound `started()` (OTP `{timeout, T}`; default `Infinity`). At
    /// expiry the start commits `InitTimeout` and the actor is killed when
    /// `started()` returns (it is never interrupted).
    pub fn init_timeout(mut self, timeout: impl Into<Timeout>) -> Self {
        self.init_timeout = timeout.into();
        self
    }
    /// Stack size of the actor's own thread (`Backend::Thread` only).
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
    /// The checks every start makes before anything exists.
    pub(super) fn validate(&self, placement: &Placement) -> Result<(), StartError> {
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
        if self.stack_size.is_some() && !matches!(placement, Placement::Backend(Backend::Thread)) {
            return Err(StartError::InvalidOptions(
                "stack_size needs an actor-owned thread (Backend::Thread)",
            ));
        }
        Ok(())
    }
}

impl<A: Actor> From<&crate::system::ExecutionGroup> for StartOptions<A> {
    fn from(group: &crate::system::ExecutionGroup) -> Self {
        Self::new().group(group)
    }
}

impl<A: Actor> From<Backend> for StartOptions<A> {
    fn from(backend: Backend) -> Self {
        Self::new().backend(backend)
    }
}

// ---------------------------------------------------------------------------
// The verbs
// ---------------------------------------------------------------------------

/// How a start was requested.
pub(super) enum Verb {
    Spawn,
    Start,
    Link(lifecycle::ParentParts),
    Monitor(lifecycle::ParentParts),
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

/// Starting an actor (OTP `proc_lib:spawn` and `gen_server:start*`).
/// Implemented for every [`Actor`].
pub trait ActorStart: Actor {
    /// Start without waiting for `started()` (`proc_lib:spawn`), unmanaged
    /// on `Backend::Async`. [`ActorRef::ready`] reports the start's result
    /// later. Panics if the executor cannot be constructed (as
    /// `std::thread::spawn`); `with_options(..).spawn()` returns the error.
    fn spawn(self) -> ActorRef<Self> {
        Starter::new(self, StartOptions::new())
            .spawn()
            .unwrap_or_else(|error| panic!("actor spawn failed: {error}"))
    }
    /// Start and wait for `started()` (`gen_server:start`): the ready
    /// actor, or the error (`StartError::Ignored` for `Init::ignore()`),
    /// which comes only after the actor's publication (its state dropped,
    /// its name free). `StartOutcome::from_result` separates `ignore` from
    /// the failures.
    fn start(self) -> Starting<Self, ActorRef<Self>> {
        Starter::new(self, StartOptions::new()).start()
    }
    /// Start linked to `parent`'s actor (`gen_server:start_link`), in its
    /// placement unless one is given. The link exists before `started()`.
    /// A failed start's `Exit` is consumed by the error for a trapping
    /// parent; a non-trapping parent still exits on a non-`Normal` failure.
    /// The parent's turn waits: a request from the child's `started()` to
    /// its parent waits for its timeout (as OTP's `call` from `init`).
    fn start_link<P: Actor>(self, parent: &Context<P>) -> Starting<Self, ActorRef<Self>> {
        Starter::new(self, StartOptions::new()).start_link(parent)
    }
    /// Start monitored by `parent`'s actor (`gen_server:start_monitor`), in
    /// its placement unless one is given. A failed start's `Down` is
    /// consumed by the error.
    fn start_monitor<P: Actor + Handler<Down>>(
        self,
        parent: &Context<P>,
    ) -> Starting<Self, (ActorRef<Self>, MonitorRef)> {
        Starter::new(self, StartOptions::new()).start_monitor(parent)
    }
    /// Start with options: `a.with_options(&group).spawn()?`,
    /// `a.with_options(StartOptions::new().name("x")).start().await?`.
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
    /// Start without waiting (`proc_lib:spawn_opt`).
    pub fn spawn(self) -> Result<ActorRef<A>, StartError> {
        ActorRef::launch(self.actor, self.options, Verb::Spawn, None).map(|(actor, _)| actor)
    }
    /// Start and wait for `started()`.
    pub fn start(self) -> Starting<A, ActorRef<A>> {
        Starting::new(
            ActorRef::launch(self.actor, self.options, Verb::Start, None),
            |a, _| a,
        )
    }
    /// Start linked to `parent`'s actor and wait for `started()`.
    pub fn start_link<P: Actor>(self, parent: &Context<P>) -> Starting<A, ActorRef<A>> {
        let verb = Verb::Link(parent.parent_parts());
        Starting::new(
            ActorRef::launch(self.actor, self.options, verb, None),
            |a, _| a,
        )
    }
    /// Start monitored by `parent`'s actor and wait for `started()`.
    ///
    /// If the returned `Starting` is dropped unanswered, the monitor stays:
    /// the child's `Down` is still delivered to `parent`, carrying a
    /// [`MonitorRef`](crate::MonitorRef) the caller never received. Match it
    /// by `down.actor` (`Starting::actor_ref` gives the id before awaiting).
    pub fn start_monitor<P: Actor + Handler<Down>>(
        self,
        parent: &Context<P>,
    ) -> Starting<A, (ActorRef<A>, MonitorRef)> {
        let verb = Verb::Monitor(parent.parent_parts());
        let parent = parent.clone();
        let arm: ArmMonitor = Box::new(move |child, monitor_ref, active, cell| {
            parent.arm_start_monitor(child, monitor_ref, active, cell)
        });
        Starting::new(
            ActorRef::launch(self.actor, self.options, verb, Some(arm)),
            |a, m| (a, m.expect("start_monitor commits a monitor")),
        )
    }
}

/// Arms the parent's monitor waiter (typed by the parent) before dispatch.
pub(super) type ArmMonitor = Box<
    dyn FnOnce(ChildHandle, MonitorRef, spawned_rt::tasks::CancellationToken, Arc<StartupCell>)
        + Send,
>;

// ---------------------------------------------------------------------------
// Starting
// ---------------------------------------------------------------------------

/// A start whose actor already exists (stage 1 ran in the call). Awaiting it
/// reads the startup result; dropping it abandons the wait (the actor runs
/// on, and a diverted notice is handed to the parent as an ordinary one).
#[must_use = "the actor runs; await this to learn whether it started"]
pub struct Starting<A: Actor, T> {
    inner: Result<Waiting<A, T>, StartError>,
}

struct Waiting<A: Actor, T> {
    actor: ActorRef<A>,
    value: Option<T>,
    guard: AbandonGuard,
}

/// Abandons the wait if the `Starting` (or its future) is dropped
/// unanswered.
struct AbandonGuard {
    cell: Option<Arc<StartupCell>>,
}

impl Drop for AbandonGuard {
    fn drop(&mut self) {
        if let Some(cell) = self.cell.take() {
            cell.abandon();
        }
    }
}

impl<A: Actor, T> Starting<A, T> {
    fn new(
        launched: Result<(ActorRef<A>, Option<MonitorRef>), StartError>,
        value: impl FnOnce(ActorRef<A>, Option<MonitorRef>) -> T,
    ) -> Self {
        Self {
            inner: launched.map(|(actor, monitor)| {
                let cell = actor.shared.signal.startup.clone();
                Waiting {
                    value: Some(value(actor.clone(), monitor)),
                    actor,
                    guard: AbandonGuard { cell: Some(cell) },
                }
            }),
        }
    }
    /// The unready instance, without waiting or consuming the result (`Err`
    /// only for a stage-1 refusal): for wiring during initialisation, or to
    /// keep control of a stuck `started()`.
    pub fn actor_ref(&self) -> Result<ActorRef<A>, &StartError> {
        match &self.inner {
            Ok(waiting) => Ok(waiting.actor.clone()),
            Err(error) => Err(error),
        }
    }
}

impl<A: Actor, T: Send + 'static> std::future::IntoFuture for Starting<A, T> {
    type Output = Result<T, StartError>;
    type IntoFuture = Pin<Box<dyn Future<Output = Self::Output> + Send>>;
    fn into_future(self) -> Self::IntoFuture {
        Box::pin(async move {
            let mut waiting = self.inner?;
            let outcome = observe(&waiting.actor, true).await;
            // Answered: nothing to abandon.
            waiting.guard.cell = None;
            outcome
                .and_then(StartOutcome::into_result)
                .map(|()| waiting.value.take().expect("value taken once"))
        })
    }
}

/// Wait for the startup result of `actor` (`waiter`: the start's own wait,
/// which consumes it).
pub(super) async fn observe<A: Actor>(
    actor: &ActorRef<A>,
    waiter: bool,
) -> Result<StartOutcome<()>, StartError> {
    let cell = actor.shared.signal.startup.clone();
    loop {
        let mut changed = cell.subscribe();
        match cell.observe(waiter, || actor.child_handle()) {
            Observed::Answer(answer) => return answer,
            Observed::Wait(None) => {
                let _ = changed.changed().await;
            }
            Observed::Wait(Some(deadline)) => {
                let _ = futures::future::select(
                    Box::pin(changed.changed()),
                    Box::pin(rt::sleep_until(deadline)),
                )
                .await;
            }
        }
    }
}
