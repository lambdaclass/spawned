use crate::mailbox::threads as mpsc;
use spawned_rt::threads::{self as rt, CancellationToken};
#[cfg(test)]
use std::{collections::HashMap, sync::Mutex};
use std::{
    fmt::Debug,
    panic::{catch_unwind, AssertUnwindSafe},
    sync::Arc,
    time::Duration,
};

use crate::child_handle::{ActorId, ChildHandle, Completion};
use crate::error::{panic_message, ActorError, ExitReason, NoProc};
use crate::link::{
    self, new_link_table, new_linked_exit_reason, new_trap_exit_flag, Exit, LinkOutcome, LinkTable,
    LinkedExitReason, SendExitFn, SendHeldFn, SignalTarget, TrapExitFlag,
};
use crate::message::Message;
use crate::monitor::{Down, MonitorCleanup, MonitorRef, MonitorTable};
use crate::reply::{registry::Registry, reply_slot, SlotHandle};
use crate::reply::{Deadline, PendingReply, ReplyReceiver, DEFAULT_MAX_OUTSTANDING};
use std::sync::atomic::Ordering;

pub use crate::response::DEFAULT_REQUEST_TIMEOUT;

/// Cleanup deadline for actors without a supervisor: the bound on joining an
/// exiting actor's cancelled jobs.
pub const DEFAULT_STOP_DEADLINE: Duration = Duration::from_secs(5);

#[path = "jobs.rs"]
mod jobs;
pub use jobs::{BlockingJobPermit, JobCtx};

mod envelope;
use envelope::*;
mod actor_ref;
mod context;
mod introspect;
use introspect::probe_of;
mod lifecycle;
#[cfg(test)]
use lifecycle::run_actor_receiving;

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
    /// Deferred-reply credits per instance (live `ReplyTo` tokens plus running
    /// linked jobs). At the bound `defer()` answers the caller `Overloaded`.
    /// Default 1024; capped at 65535.
    const MAX_OUTSTANDING: usize = DEFAULT_MAX_OUTSTANDING;

    /// Native threads this instance's jobs may run at once. `None`:
    /// `min(4, available_parallelism)`.
    ///
    /// The default is small: an actor that runs many blocking jobs at once
    /// usually needs more, and under load the only symptom is
    /// `PipeError::NativeBudget` refusals (the first one per instance is
    /// logged as a `native_budget_exhausted` warning). Set it with
    /// `#[actor(max_native_jobs = N)]`.
    const MAX_NATIVE_JOBS: Option<usize> = None;

    /// Whether this actor overrides `stopped()`. `#[actor]` sets it for a
    /// `#[stopped]` hook; a hand-written impl that overrides `stopped()` can
    /// set it too. When a kill, an untrapped exit signal or link notice
    /// skips the hook, the first skip per actor type is then logged as a
    /// `warn` (`event = "stopped_skipped"`) naming the remedy, instead of
    /// at `debug` only. It changes no behaviour.
    const HAS_STOPPED: bool = false;

    /// How `get_status` shows this actor's state (OTP `format_status/1`).
    /// `None` (the default) redacts it: type-erased state appears only for
    /// actors that opt in, with `#[actor(inspect)]` (through
    /// [`Inspect`](crate::introspection::Inspect)) or
    /// `#[actor(inspect = debug)]` (its `Debug` form).
    const FORMAT_STATUS: Option<fn(&Self) -> String> = None;

    /// Initialisation, before any message (OTP `init/1`). Returns `()`,
    /// `Result<(), E>`, `Result<Init<Self>, E>` or [`Init<Self>`]. A failed
    /// start never runs `stopped()`.
    fn started(&mut self, _ctx: &Context<Self>) -> impl Into<Init<Self>> {}
    fn stopped(&mut self, _ctx: &Context<Self>) {}

    /// Called when a linked actor stops, if this actor has called
    /// `ctx.trap_exit(true)`. Default impl ignores the signal.
    fn exit_received(&mut self, _exit: Exit, _ctx: &Context<Self>) {}
}

// ---------------------------------------------------------------------------
// Handler trait (per-message, sync version)
// ---------------------------------------------------------------------------

/// Per-message handler trait. Implement once for each message type the actor handles.
///
/// Unlike the `tasks` version, handlers are synchronous — no `async`/`.await`.
///
/// `handle` is the envelope dispatch for both `send` and `request`: answer
/// through `reply` ([`PendingReply::reply`], or [`PendingReply::defer`] to
/// answer later). A request whose `PendingReply` is dropped unanswered gets
/// `ReplyDropped`; a sent message has a detached `PendingReply`.
pub trait Handler<M: Message>: Actor {
    fn handle(&mut self, msg: M, reply: PendingReply<'_, M::Result>, ctx: &Context<Self>);
}

// ---------------------------------------------------------------------------
// Context
// ---------------------------------------------------------------------------

/// Handle passed to every handler and lifecycle hook, providing access to the
/// actor's mailbox and lifecycle controls.
///
/// Clone is cheap: one reference-count increment on the actor's shared state.
pub struct Context<A: Actor> {
    shared: Arc<ActorShared<A>>,
}

/// State shared by every `Context` and `ActorRef` of one actor, as in tasks
/// mode: the handles stay one pointer wide and cloning one is a single
/// reference-count increment.
struct ActorShared<A: Actor> {
    id: ActorId,
    sender: mpsc::Sender<MailboxItem<A>>,
    cancellation_token: CancellationToken,
    completion: spawned_rt::sync::watch::Receiver<Option<ExitReason>>,
    monitors: MonitorTable,
    trap_exit: TrapExitFlag,
    links: LinkTable,
    linked_reason: LinkedExitReason,
    /// This instance as the target of exit signals and link notices.
    signal: Arc<SignalTarget>,
    mailbox: crate::instrumentation::Mailbox,
    /// This instance's reply registry: deferred tokens, credits, turn phase.
    replies: Arc<Registry>,
    /// This instance's pipe-to-self jobs.
    jobs: Arc<crate::reply::jobs::JobSet>,
    /// Set when cleanup completes (death plus the bounded job join); `join()`
    /// waits for it.
    cleaned: Arc<crate::reply::jobs::Cleaned>,
}

impl<A: Actor> Clone for Context<A> {
    fn clone(&self) -> Self {
        Self {
            shared: self.shared.clone(),
        }
    }
}

impl<A: Actor> Debug for Context<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Context").finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------
// Receiver trait (object-safe) + Recipient alias
// ---------------------------------------------------------------------------

/// Object-safe trait for sending a single message type to an actor.
///
/// Implemented automatically by `ActorRef<A>` and `Context<A>` for any
/// message type that `A` handles. Sealed: it cannot be implemented outside
/// spawned (a method can then be added without a breaking change).
pub trait Receiver<M: Message>: Send + Sync + crate::__private::Sealed {
    fn send(&self, msg: M) -> Result<(), ActorError>;
    /// Send a request without waiting (`gen_server:send_request/2` with an explicit deadline); its
    /// deadline is fixed now (the slot carries it). On a [`Recipient`],
    /// pass a `Deadline` (`Deadline::after(d)`, or `d.into()`).
    fn send_request(
        &self,
        msg: M,
        deadline: Deadline,
    ) -> Result<ReplyReceiver<M::Result>, ActorError>;
    /// `send_request` for the `request` helper, which arms its own timer:
    /// the deadline does not go to the owner's heap.
    #[doc(hidden)]
    fn send_request_timed(
        &self,
        msg: M,
        deadline: Deadline,
    ) -> Result<ReplyReceiver<M::Result>, ActorError> {
        self.send_request(msg, deadline)
    }
}

/// Type-erased reference for sending a single message type.
pub type Recipient<M> = Arc<dyn Receiver<M>>;

/// Send a request through a type-erased `Receiver` with a custom timeout.
pub fn request<M: Message>(
    recipient: &dyn Receiver<M>,
    msg: M,
    timeout: Duration,
) -> Result<M::Result, ActorError> {
    let observation = crate::instrumentation::RequestObservation::new();
    let result = match recipient.send_request_timed(msg, Deadline::after(timeout)) {
        Err(error) => Err(error),
        Ok(rx) => rx.recv(),
    };
    observation.finish(&result);
    result
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
    shared: Arc<ActorShared<A>>,
}

impl<A: Actor> Debug for ActorRef<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActorRef").finish_non_exhaustive()
    }
}

impl<A: Actor> Clone for ActorRef<A> {
    fn clone(&self) -> Self {
        Self {
            shared: self.shared.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Actor::start
// ---------------------------------------------------------------------------

mod start;
pub use start::{ActorStart, Init, Name, StartOptions, Starter};
use start::{ArmMonitor, ParentParts, Verb};

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
mod tests;

#[cfg(test)]
mod monitor_cleanup_regressions;

#[cfg(test)]
mod early_stop_regression;

#[cfg(test)]
mod idle_stop_regression;

#[cfg(test)]
mod stop_wakeup_regression;

/// Context and request APIs that the behavioural tests above do not reach.
#[cfg(test)]
mod api_coverage;
