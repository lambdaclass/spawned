//! Read-only introspection: the OTP `process_info` equivalent ([`ActorInfo`]),
//! labels (OTP 27 `proc_lib:set_label`) and crash reports
//! (docs/design/introspection.md, tier 0).
//!
//! Every read here is **out of band** (I1): it loads shared atomics and takes
//! short metadata locks the actor also takes (its exit request, link and
//! monitor tables, label), and never enqueues, wakes, runs user code on the
//! observed actor or waits for a turn. So `info()` answers while a handler is
//! wedged. Nothing here runs on send, dispatch or a turn: the fields are kept
//! by state that already exists (the 0.8 turn phase, the exit request, the
//! final reason) or is written at spawn and by explicit calls (I11).
//!
//! Tasks and threads share these types; both modes expose the same calls
//! (`ActorRef::info`, `Context::info`, `Context::set_label`,
//! `ChildHandle::info`, `ChildHandle::links`, `ChildHandle::monitors`).
//!
//! Tier 1 fields (`activity`, `mailbox`) need the `introspection` feature,
//! which is not implemented yet: they read [`Field::Unavailable`] with
//! [`Unavailable::Feature`], so "not compiled" and "could not read" differ.

use crate::child_handle::ActorId;
use crate::error::ExitReason;
use crate::link::SignalTarget;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Identity of an `ActorSystem`, unique in the
/// process. Every managed actor's [`ActorInfo::system`] names its system.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SystemId(u64);

impl SystemId {
    #[cfg_attr(not(feature = "async"), allow(dead_code))]
    pub(crate) fn next() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        Self(NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
    }
}

impl std::fmt::Display for SystemId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// One actor as it is now (OTP `process_info/1`). Each field is one atomic
/// load or one locked read; the fields of one `ActorInfo` are not from one
/// instant.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ActorInfo {
    pub id: ActorId,
    /// The system of a managed actor; `None` for a bare `start()`.
    pub system: Option<SystemId>,
    /// `std::any::type_name` of the actor (OTP `initial_call`).
    pub actor_type: &'static str,
    /// Set with `Context::set_label` (OTP 27 `proc_lib:set_label`).
    pub label: Option<Arc<str>>,
    /// The first name the actor holds in the [`registry`](crate::registry)
    /// (OTP `registered_name`): names registered through the actor's own
    /// scope or for its identity. A value stored with the plain
    /// `registry::register` is not tied to an actor and is not reported.
    pub name: Option<String>,
    pub lifecycle: Lifecycle,
    /// OTP `trap_exit`.
    pub trap_exit: bool,
    /// Live links (OTP `links`), see `ChildHandle::links`.
    pub links: usize,
    /// Monitors this actor holds on others (OTP `monitors`), see
    /// `ChildHandle::monitors`.
    pub monitors: usize,
    pub parent: Parent,
    /// The supervisor child this instance is, if any.
    pub supervisor: Option<SupervisorPosition>,
    pub placement: Placement,
    /// Live deferred reply tokens (`ReplyTo`) of this instance.
    pub deferred_outstanding: usize,
    /// Pipe-to-self jobs still running.
    pub jobs_running: usize,
    /// Tier 1 (`introspection` feature): the current or last activity.
    pub activity: Field<Activity>,
    /// Tier 1 (`introspection` feature): mailbox counters.
    pub mailbox: Field<MailboxCounts>,
}

/// A field that may not be available: compiled out, or not readable now.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Field<T> {
    Value(T),
    Unavailable(Unavailable),
}

impl<T> Field<T> {
    /// The value, if available.
    pub fn value(&self) -> Option<&T> {
        match self {
            Field::Value(value) => Some(value),
            Field::Unavailable(_) => None,
        }
    }
}

/// Why a [`Field`] has no value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Unavailable {
    /// The feature that keeps it is not compiled in.
    Feature,
    /// Concurrent writes kept a consistent read from succeeding.
    ReadContended,
    /// The coarse clock had not started when the activity began.
    ClockNotStarted,
    /// Withheld (state without `Inspect`).
    Redacted,
    /// The actor is past its death point.
    Dead,
}

/// Where the actor is in its lifecycle (OTP `process_info(status)`, with
/// Spawned's vocabulary). Read from 0.8 state only: the turn phase, the exit
/// request and the final reason.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Lifecycle {
    /// `started()` has not returned.
    Starting,
    /// Between turns, waiting for a message.
    Idle,
    /// Running a turn (a handler, `exit_received`, a framework turn). The
    /// tier 0 lifecycle does not tell these apart.
    InTurn,
    /// Exit began (a reason was requested, or the run loop left); `stopped()`
    /// may be running.
    Exiting,
    /// Past the death point D. `published` is `true` once `wait_exit()` is
    /// ready (publication); between D and publication the actor's state and
    /// queued messages are being destroyed.
    Dead { reason: ExitReason, published: bool },
}

/// What started this actor and receives its exit as a parent (OTP `parent`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Parent {
    /// A bare start: no parent.
    None,
    /// `start_linked` from this actor.
    Actor(ActorId),
    /// Its `ActorSystem`: an execution group
    /// placed it, or a supervisor (then [`ActorInfo::supervisor`] is set).
    System,
}

/// A supervised instance's place in its supervisor.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SupervisorPosition {
    /// The `ChildSpec` name.
    pub child: String,
    /// 1 for the first instance of the child, one more per restart.
    pub generation: u64,
    /// Restarts of the child before this instance (`generation - 1`).
    pub restarts: u64,
}

/// Where the actor's turns run.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Placement {
    /// `tasks::Backend::Async`.
    Async,
    /// `tasks::Backend::Blocking`.
    Blocking,
    /// `tasks::Backend::Thread`.
    Thread,
    /// A threads-mode actor on its own OS thread.
    Native,
    /// An execution group of an `ActorSystem`.
    Group { name: String },
}

/// Per-actor id of an activity (tier 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TurnId(pub u64);

/// What an activity is (tier 1).
///
/// A message turn is `MessageTurn`, not `Message`: a public item named
/// `Message` beside the `Message` trait makes rustc print the trait's full
/// path in every user's trait-bound error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ActivityKind {
    Started,
    MessageTurn,
    ExitReceived,
    Status,
    Stopped,
    Idle,
}

/// Whether an async activity is inside a poll (tier 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PollState {
    InPoll { age: Duration },
    Suspended,
}

/// The current or last activity (tier 1, `introspection` feature).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Activity {
    pub kind: ActivityKind,
    pub turn: TurnId,
    pub message: Option<&'static str>,
    pub age: Field<Duration>,
    pub poll: Option<PollState>,
    pub kill_pending: bool,
}

/// Mailbox counters (tier 1, `introspection` feature).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct MailboxCounts {
    pub len: u64,
    pub accepted: u64,
    pub dequeued: u64,
    pub discarded: u64,
}

// ---------------------------------------------------------------------------
// Per-actor metadata
// ---------------------------------------------------------------------------

/// The type-erased side of a running instance, read by `ChildHandle::info`
/// and the crash report: implemented by each mode's shared actor state.
pub(crate) trait Probe: Send + Sync {
    fn info(&self) -> ActorInfo;
    /// Targets of this actor's active monitors, sorted.
    fn monitors(&self) -> Vec<ActorId>;
}

/// Where an instance's turns run, as fixed at spawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Host {
    #[cfg_attr(not(feature = "async"), allow(dead_code))]
    Async,
    #[cfg_attr(not(feature = "async"), allow(dead_code))]
    Blocking,
    #[cfg_attr(not(feature = "async"), allow(dead_code))]
    Thread,
    Native,
    /// An execution group (the group is read from the managed scope).
    #[cfg_attr(not(feature = "async"), allow(dead_code))]
    Managed,
}

/// Introspection metadata of one instance, kept in its [`SignalTarget`] so
/// every type-erased handle reaches it. Written at spawn and by
/// `set_label`; never on send, dispatch or a turn.
pub(crate) struct Meta {
    pub(crate) actor_type: &'static str,
    pub(crate) host: Host,
    label: Mutex<Option<Arc<str>>>,
    /// The instance's shared state, set once right after it is built. Weak:
    /// the signal target outlives the instance in peers' link tables.
    probe: OnceLock<Weak<dyn Probe>>,
}

impl Meta {
    pub(crate) fn new(actor_type: &'static str, host: Host) -> Self {
        Self {
            actor_type,
            host,
            label: Mutex::new(None),
            probe: OnceLock::new(),
        }
    }

    pub(crate) fn set_probe(&self, probe: Weak<dyn Probe>) {
        let _ = self.probe.set(probe);
    }

    pub(crate) fn probe(&self) -> Option<Arc<dyn Probe>> {
        self.probe.get().and_then(Weak::upgrade)
    }

    pub(crate) fn label(&self) -> Option<Arc<str>> {
        self.label.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    pub(crate) fn set_label(&self, label: Arc<str>) {
        let old = self
            .label
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .replace(label);
        drop(old);
    }
}

impl Default for Meta {
    fn default() -> Self {
        Self::new("", Host::Native)
    }
}

// ---------------------------------------------------------------------------
// Building an ActorInfo
// ---------------------------------------------------------------------------

/// What one read needs; each mode fills it from its shared state.
pub(crate) struct Sources<'a> {
    pub(crate) signal: &'a SignalTarget,
    pub(crate) links: &'a crate::link::LinkTable,
    /// The live registry; `None` once the instance's state is gone.
    pub(crate) replies: Option<&'a crate::reply::registry::Registry>,
    pub(crate) published: bool,
    pub(crate) monitors: usize,
    pub(crate) jobs_running: usize,
    #[cfg(feature = "async")]
    pub(crate) scope: Option<&'a crate::system::Scope>,
}

/// The `ActorInfo` of one instance. Takes the exit-request lock and the
/// link table lock in turn, never both, and runs no user code.
pub(crate) fn build(s: Sources<'_>) -> ActorInfo {
    let id = s.signal.id;
    let lifecycle = lifecycle(s.signal, s.replies, s.published);
    let links = crate::link::live_links(s.links).len();
    #[cfg(feature = "async")]
    let (system, supervisor, group) = match s.scope {
        Some(scope) => (scope.system_id(), scope.position(), scope.group().name()),
        None => (None, None, None),
    };
    #[cfg(not(feature = "async"))]
    let (system, supervisor, group): (
        Option<SystemId>,
        Option<SupervisorPosition>,
        Option<String>,
    ) = (None, None, None);
    let meta = &s.signal.meta;
    let placement = match meta.host {
        Host::Async => Placement::Async,
        Host::Blocking => Placement::Blocking,
        Host::Thread => Placement::Thread,
        Host::Native => Placement::Native,
        Host::Managed => Placement::Group {
            name: group.unwrap_or_default(),
        },
    };
    let parent = match (s.signal.parent, meta.host) {
        (Some(parent), _) => Parent::Actor(parent),
        (None, Host::Managed) => Parent::System,
        (None, _) => Parent::None,
    };
    ActorInfo {
        id,
        system,
        actor_type: meta.actor_type,
        label: meta.label(),
        name: crate::registry::name_of(id),
        lifecycle,
        trap_exit: s.signal.trap.load(std::sync::atomic::Ordering::Acquire),
        links,
        monitors: s.monitors,
        parent,
        supervisor,
        placement,
        deferred_outstanding: s.replies.map_or(0, |r| r.deferred_tokens()),
        jobs_running: s.jobs_running,
        activity: Field::Unavailable(Unavailable::Feature),
        mailbox: Field::Unavailable(Unavailable::Feature),
    }
}

/// The tier 0 lifecycle: Dead from the death point (the exit request's
/// `dead`, set in D's critical section), then the turn phase (`InTurn`),
/// then a begun exit, then phase 0 (`Starting`).
fn lifecycle(
    signal: &SignalTarget,
    replies: Option<&crate::reply::registry::Registry>,
    published: bool,
) -> Lifecycle {
    use crate::reply::turn::{EXITING, IN_TURN, IN_TURN_PENDING};
    let request = crate::link::lock_request(&signal.request);
    if request.is_dead() {
        return Lifecycle::Dead {
            reason: request.reason().cloned().unwrap_or(ExitReason::Normal),
            published,
        };
    }
    let phase = replies.map(|r| r.phase());
    if matches!(phase, Some(IN_TURN | IN_TURN_PENDING)) {
        return Lifecycle::InTurn;
    }
    if request.reason().is_some() || phase.is_some_and(|p| p >= EXITING) {
        return Lifecycle::Exiting;
    }
    if request.is_starting() {
        return Lifecycle::Starting;
    }
    Lifecycle::Idle
}

/// `info()` of an instance whose state is gone (every `ActorRef` and
/// `Context` dropped): it is past its death point.
pub(crate) fn released(
    signal: &SignalTarget,
    links: &crate::link::LinkTable,
    published: bool,
) -> ActorInfo {
    build(Sources {
        signal,
        links,
        replies: None,
        published,
        monitors: 0,
        jobs_running: 0,
        #[cfg(feature = "async")]
        scope: None,
    })
}

// ---------------------------------------------------------------------------
// Crash reports
// ---------------------------------------------------------------------------

/// The `spawned::crash` report (OTP's `proc_lib` crash report), emitted once
/// every caller of a dying actor is answered, before its state is destroyed:
/// for an abnormal final reason that the actor itself produced (a panic, a
/// self-exit or parent exit with an abnormal reason), not for an exit an
/// untrapped signal, a kill or a runtime drop imposed on it (as in OTP, where
/// a process killed by a signal runs no code and reports nothing).
///
/// Target and field names are a contract (stable from 1.0): `actor_id`,
/// `system`, `actor_type`, `label`, `name`, `reason`, `ancestors`,
/// `generation`, `group`, `links`.
pub(crate) fn crash_report(signal: &SignalTarget, reason: &ExitReason, links: &[ActorId]) {
    let info = signal.meta.probe().map(|probe| probe.info());
    let meta = &signal.meta;
    let (system, generation, group, name, ancestors) = match &info {
        Some(info) => (
            info.system.map(|s| s.to_string()),
            info.supervisor.as_ref().map(|p| p.generation),
            match &info.placement {
                Placement::Group { name } => Some(name.clone()),
                _ => None,
            },
            info.name.clone(),
            ancestors(&info.parent, info.supervisor.as_ref()),
        ),
        None => (
            None,
            None,
            None,
            crate::registry::name_of(signal.id),
            ancestors(&signal.parent.map_or(Parent::None, Parent::Actor), None),
        ),
    };
    let label = meta.label();
    let links: Vec<String> = links.iter().map(ToString::to_string).collect();
    crate::actor_cleanup::log_safely(|| {
        tracing::error!(
            target: "spawned::crash",
            actor_id = %signal.id,
            system = system.as_deref(),
            actor_type = meta.actor_type,
            label = label.as_deref(),
            name = name.as_deref(),
            reason = %reason,
            ancestors = %ancestors,
            generation,
            group = group.as_deref(),
            links = %links.join(","),
            "crash report: actor {} ({}) exited: {reason}",
            signal.id,
            meta.actor_type,
        );
    });
}

/// OTP `$ancestors`: what started the actor, nearest first.
fn ancestors(parent: &Parent, supervisor: Option<&SupervisorPosition>) -> String {
    match (parent, supervisor) {
        (Parent::Actor(id), _) => format!("actor {id}"),
        (_, Some(position)) => format!("supervisor child {}", position.child),
        (Parent::System, None) => "system".to_string(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_value_reads_only_values() {
        assert_eq!(Field::Value(3).value(), Some(&3));
        assert_eq!(Field::<u8>::Unavailable(Unavailable::Feature).value(), None);
    }

    #[test]
    fn label_replaces_and_meta_starts_without_probe() {
        let meta = Meta::new("T", Host::Native);
        assert!(meta.label().is_none());
        meta.set_label("a".into());
        meta.set_label("b".into());
        assert_eq!(meta.label().as_deref(), Some("b"));
        assert!(meta.probe().is_none());
    }

    #[test]
    fn ancestors_name_the_nearest_starter() {
        let pos = SupervisorPosition {
            child: "w".into(),
            generation: 2,
            restarts: 1,
        };
        assert_eq!(ancestors(&Parent::None, None), "");
        assert_eq!(ancestors(&Parent::System, None), "system");
        assert_eq!(ancestors(&Parent::System, Some(&pos)), "supervisor child w");
        let id = ActorId::next();
        assert_eq!(ancestors(&Parent::Actor(id), None), format!("actor {id}"));
    }
}
