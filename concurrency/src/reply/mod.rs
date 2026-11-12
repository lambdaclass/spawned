//! Reply transport for requests: reply slots, tokens and receivers
//! (docs/design/deferred-replies.md).
//!
//! Every request has a reply slot (`slot::Slot`) shared by the envelope,
//! then by the handler's [`PendingReply`] (and, after [`PendingReply::defer`],
//! a storable [`ReplyTo`]), and by the caller's [`ReplyReceiver`]. Each slot
//! gets exactly one terminal discharge.

pub(crate) mod deadline;
pub(crate) mod job_core;
pub mod jobs;
mod pending;
pub(crate) mod registry;
pub(crate) mod slot;
pub(crate) mod turn;

pub use pending::{InsertError, InsertErrorKind, PendingReplies};
pub(crate) use registry::DEFAULT_MAX_OUTSTANDING;
pub use slot::Delivery;

use crate::error::ActorError;
use crate::ActorId;
use registry::Registry;
pub(crate) use slot::Slot;
use slot::{Terminal, NEVER};
use spawned_rt::time::Instant;
use std::borrow::Cow;
use std::marker::PhantomData;
use std::sync::Arc;
use std::time::Duration;

/// Production synchronization types for [`slot`]; the Loom model supplies its
/// own (`tests/reply_slot_loom.rs`).
mod sync {
    pub(crate) use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
    pub(crate) use std::sync::{Arc, Arc as JobArc, Condvar, Mutex, MutexGuard, Weak as JobWeak};
    pub(crate) use std::thread::yield_now;
    pub(crate) type SlotOwner = Option<Arc<super::registry::Registry>>;
    /// The owner's final reason, as the slot records it at its close.
    pub(crate) type Reason = crate::error::ExitReason;

    /// Run a slot transition as one step of the lifecycle-trace
    /// linearization (the identity without the feature).
    #[inline(always)]
    pub(crate) fn section<T>(f: impl FnOnce() -> T) -> T {
        crate::lifecycle_trace::atomic(f)
    }

    /// A slot left `OPEN` (`kind` is the model's terminal name; `token`
    /// whether a deferred token was live): metrics and lifecycle traces.
    #[inline(always)]
    #[cfg_attr(not(feature = "lifecycle-trace"), allow(unused_variables))]
    pub(crate) fn left_open(owner: &SlotOwner, slot: usize, kind: &'static str, token: bool) {
        crate::instrumentation::hooks::left_open(kind, token);
        #[cfg(feature = "lifecycle-trace")]
        {
            if let Some(registry) = owner {
                crate::lifecycle_trace::reply(
                    registry.owner().raw(),
                    "Discharge",
                    slot as u64,
                    kind,
                );
            }
            crate::lifecycle_trace::downstream_closed(slot as u64, kind);
        }
    }

    /// A deferred token's deadline was lowered (`slot`'s lock held):
    /// lifecycle traces (the model's `Shorten`).
    #[inline(always)]
    #[cfg_attr(not(feature = "lifecycle-trace"), allow(unused_variables))]
    pub(crate) fn shortened(owner: &SlotOwner, slot: usize) {
        #[cfg(feature = "lifecycle-trace")]
        if let Some(registry) = owner {
            crate::lifecycle_trace::reply(registry.owner().raw(), "Shorten", slot as u64, "");
        }
    }

    /// A blocking receiver registered its unparker and is about to park:
    /// the failpoint site `reply.recv_parked`, a handshake for tests.
    #[inline(always)]
    pub(crate) fn parked() {
        fail_point!("reply.recv_parked");
    }

    /// Run `f` with a waker that unparks the current thread (cached per
    /// thread: no allocation, and no reference count change when it is
    /// already the slot's registered waker).
    pub(crate) fn with_thread_waker<T>(f: impl FnOnce(&std::task::Waker) -> T) -> T {
        struct Unpark(std::thread::Thread);
        impl std::task::Wake for Unpark {
            fn wake(self: Arc<Self>) {
                self.0.unpark();
            }
            fn wake_by_ref(self: &Arc<Self>) {
                self.0.unpark();
            }
        }
        thread_local! {
            static WAKER: std::task::Waker =
                std::task::Waker::from(Arc::new(Unpark(std::thread::current())));
        }
        WAKER.with(|waker| f(waker))
    }

    /// Park the current thread for at most `ticks` clock ticks
    /// (nanoseconds), or until unparked when `None`.
    pub(crate) fn park(ticks: Option<u64>) {
        match ticks {
            None => std::thread::park(),
            Some(ticks) => std::thread::park_timeout(Duration::from_nanos(ticks)),
        }
    }
    use std::time::Duration;
}

/// Clock ticks now, on the `spawned_rt` clock (virtual under simulation).
pub(crate) fn now_ticks() -> u64 {
    spawned_rt::time::now_ticks()
}

// ---------------------------------------------------------------------------
// Deadline
// ---------------------------------------------------------------------------

/// A request's deadline, fixed when it is sent.
///
/// `gen_server:call/3` takes a timeout or `infinity`; a request takes an
/// absolute instant or [`Deadline::Never`]. `request` uses
/// [`DEFAULT_REQUEST_TIMEOUT`](crate::response::DEFAULT_REQUEST_TIMEOUT)
/// after the send; `send_request` takes the deadline. Every call that takes
/// a deadline takes `impl Into<Deadline>`: a `Duration` (from now), an
/// `Instant` or a `Deadline`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deadline {
    /// Expire at this instant (on the `spawned_rt::time` clock).
    At(Instant),
    /// Never expire; the caller waits until the reply or the owner's death.
    Never,
}

impl Deadline {
    /// `timeout` from now.
    pub fn after(timeout: Duration) -> Self {
        match Instant::now().checked_add(timeout) {
            Some(at) => Self::At(at),
            None => Self::Never,
        }
    }

    pub(crate) fn ticks(self) -> u64 {
        match self {
            Self::At(at) => spawned_rt::time::to_ticks(at),
            Self::Never => NEVER,
        }
    }

    fn of_ticks(ticks: u64) -> Self {
        if ticks == NEVER {
            Self::Never
        } else {
            Self::At(spawned_rt::time::from_ticks(ticks))
        }
    }
}

/// A timeout from now: [`Deadline::after`]. Every call that takes
/// `impl Into<Deadline>` takes a `Duration`, an `Instant` or a `Deadline`.
impl From<Duration> for Deadline {
    fn from(timeout: Duration) -> Self {
        Self::after(timeout)
    }
}

/// An absolute instant on the `spawned_rt::time` clock.
impl From<Instant> for Deadline {
    fn from(at: Instant) -> Self {
        Self::At(at)
    }
}

fn record(_delivery: Delivery) {
    #[cfg(feature = "metrics")]
    crate::instrumentation::reply(_delivery == Delivery::Delivered);
}

/// The caller's error for a terminal slot. `Stopped` carries the owner's
/// final reason, which its registry stores at the death point before any of
/// its slots becomes `STOPPED`.
///
/// A slot without an owner (a test double's `reply_pair`) has no actor whose
/// reason it could carry: it reports `NoProc`, as OTP's call to a process
/// that does not exist. `Abandoned` and an already taken `Replied` cannot
/// reach a caller (the receiver is gone, or consumed); they map the same way.
fn terminal_error<R>(terminal: Terminal, slot: &Slot<R>) -> ActorError {
    match terminal {
        Terminal::Dropped => ActorError::ReplyDropped,
        Terminal::Overloaded => ActorError::Overloaded,
        Terminal::TimedOut => ActorError::RequestTimeout,
        Terminal::Stopped | Terminal::Abandoned | Terminal::Replied => ActorError::ActorStopped(
            slot.stopped_reason()
                .or_else(|| {
                    slot.owner
                        .as_ref()
                        .and_then(|owner| owner.final_reason().cloned())
                })
                .unwrap_or(crate::error::ExitReason::NoProc),
        ),
    }
}

/// A new reply slot for a request with `deadline`, and the caller's receiver.
///
/// `owner` is the receiving instance's registry: an awaited raw receiver
/// puts its deadline in that owner's heap instead of arming its own timer,
/// so a request with a deadline keeps a reference to it. A request without
/// one takes none (no shared reference count per request): the owner's
/// close records the final reason in the slot, and its envelope is
/// disposed of through the owner's mailbox ([`SlotHandle::dispose`]).
pub(crate) fn reply_slot<R: Send + 'static>(
    deadline: Deadline,
    owner: Option<&Arc<Registry>>,
) -> (SlotHandle<R>, ReplyReceiver<R>) {
    let keep = deadline != Deadline::Never || cfg!(feature = "lifecycle-trace");
    let slot = Arc::new(Slot::with_owner(
        deadline.ticks(),
        owner.filter(|_| keep).cloned(),
    ));
    #[cfg(feature = "lifecycle-trace")]
    if let Some(owner) = owner {
        crate::lifecycle_trace::reply(owner.owner().raw(), "Send", Arc::as_ptr(&slot) as u64, "");
        crate::lifecycle_trace::downstream_sent(owner.owner().raw(), Arc::as_ptr(&slot) as u64);
    }
    (SlotHandle(Some(slot.clone())), ReplyReceiver::new(slot))
}

// ---------------------------------------------------------------------------
// Envelope side
// ---------------------------------------------------------------------------

/// The envelope's hold on its request's slot. Dropped undispatched (mailbox
/// disposal at death, a rejected enqueue), its request is answered
/// `ActorStopped` with the owner's final reason at the death point.
pub(crate) struct SlotHandle<R: Send + 'static>(Option<Arc<Slot<R>>>);

impl<R: Send + 'static> SlotHandle<R> {
    /// No caller: the message was sent, not requested.
    pub(crate) fn detached() -> Self {
        Self(None)
    }
    /// Whether no caller waits (a send).
    #[cfg_attr(not(feature = "lifecycle-trace"), allow(dead_code))]
    pub(crate) fn is_detached(&self) -> bool {
        self.0.is_none()
    }
    pub(crate) fn take(&mut self) -> Option<Arc<Slot<R>>> {
        self.0.take()
    }
    /// Undispatched at its owner's death point (mailbox disposal): answered
    /// with the final reason, through the owner's registry.
    pub(crate) fn dispose(&mut self, owner: Option<&Registry>) {
        if let (Some(owner), Some(slot)) = (owner, self.0.take()) {
            owner.dispose(slot);
        }
    }
}

/// A turn's caller, lent by its envelope for the turn: retained (one
/// reference) only if the runtime destroys the turn unfinished, for the
/// death point to answer.
#[cfg_attr(not(feature = "async"), allow(dead_code))]
pub(crate) trait Caller: Sync {
    fn retain(&self) -> Arc<dyn registry::Closeable>;
}

impl<R: Send + 'static> Caller for Arc<Slot<R>> {
    fn retain(&self) -> Arc<dyn registry::Closeable> {
        self.clone()
    }
}

/// Dropped undisposed (a refused admission, whose caller is answered
/// `NoProc` by the send itself): a slot that knows its owner is disposed of
/// there; one that does not (no deadline) is closed `STOPPED` now.
impl<R: Send + 'static> Drop for SlotHandle<R> {
    fn drop(&mut self) {
        if let Some(slot) = self.0.take() {
            match slot.owner.clone() {
                Some(owner) => owner.dispose(slot),
                None => {
                    slot.close();
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// PendingReply
// ---------------------------------------------------------------------------

/// Why [`PendingReply::defer`] did not return a token. More reasons may be
/// added.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum DeferError {
    /// The owner's `MAX_OUTSTANDING` credits are in use; the caller has
    /// already been answered `Overloaded`.
    #[error("deferred replies at the bound; the caller was answered Overloaded")]
    Overloaded,
    /// The request is no longer open: its deadline passed or its caller left.
    #[error("the request is closed (expired or abandoned)")]
    Closed,
    /// The message was sent, not requested: there is no caller to answer.
    #[error("the message was sent, not requested")]
    NoCaller,
}

/// The reply obligation of the request being handled, scoped to the turn.
///
/// Answer it with [`reply`](Self::reply), hand it out of the turn with
/// [`defer`](Self::defer), or drop it: a request whose `PendingReply` is
/// dropped is answered `ReplyDropped` when the turn returns, or, when it
/// panics, `ActorStopped` with the final reason at the actor's death point. A message reached by `send` has a detached
/// `PendingReply`: `reply` drops the value and `defer` returns
/// [`DeferError::NoCaller`].
pub struct PendingReply<'t, R: Send + 'static> {
    /// Lent by the dispatching envelope for the turn (tasks mode), or owned.
    slot: Option<Cow<'t, Arc<Slot<R>>>>,
    registry: Option<&'t Arc<Registry>>,
    _turn: PhantomData<&'t mut ()>,
}

impl<R: Send + 'static> std::fmt::Debug for PendingReply<'_, R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingReply")
            .field("detached", &self.slot.is_none())
            .finish_non_exhaustive()
    }
}

impl<'t, R: Send + 'static> PendingReply<'t, R> {
    pub(crate) fn new(slot: Option<Arc<Slot<R>>>, registry: &'t Arc<Registry>) -> Self {
        Self::with_slot(slot.map(Cow::Owned), registry)
    }

    /// The obligation of a request whose slot the dispatching envelope
    /// keeps for the turn: no reference is taken unless it is deferred or
    /// dropped unanswered.
    #[cfg_attr(not(feature = "async"), allow(dead_code))]
    pub(crate) fn lent(slot: Option<&'t Arc<Slot<R>>>, registry: &'t Arc<Registry>) -> Self {
        Self::with_slot(slot.map(Cow::Borrowed), registry)
    }

    fn with_slot(slot: Option<Cow<'t, Arc<Slot<R>>>>, registry: &'t Arc<Registry>) -> Self {
        #[cfg(feature = "lifecycle-trace")]
        if let Some(slot) = &slot {
            crate::lifecycle_trace::reply(
                registry.owner().raw(),
                "Dispatch",
                Arc::as_ptr(&**slot) as u64,
                "",
            );
        }
        Self {
            slot,
            registry: Some(registry),
            _turn: PhantomData,
        }
    }

    /// A `PendingReply` for a sent message: there is no caller. For tests
    /// calling a handler by hand, `spawned_test::detached_reply()`.
    pub(crate) fn detached() -> Self {
        Self {
            slot: None,
            registry: None,
            _turn: PhantomData,
        }
    }

    /// Answer the caller now. A value that is not delivered is dropped.
    pub fn reply(mut self, value: R) -> Delivery {
        match self.slot.take() {
            None => {
                drop(value);
                Delivery::CallerGone
            }
            Some(slot) => {
                let (delivery, unsent) = slot.reply_in_turn(value, &now_ticks);
                drop(unsent);
                record(delivery);
                delivery
            }
        }
    }

    /// Answer the caller `Err(Overloaded)` now.
    pub fn overloaded(mut self) {
        if let Some(slot) = self.slot.take() {
            slot.terminate(Terminal::Overloaded);
        }
    }

    /// Take the obligation out of the turn as a storable [`ReplyTo`]. This
    /// takes one of the owner's `MAX_OUTSTANDING` credits; at the bound the
    /// caller is answered `Overloaded` and this returns
    /// [`DeferError::Overloaded`].
    pub fn defer(mut self) -> Result<ReplyTo<R>, DeferError> {
        let slot = self.slot.take().ok_or(DeferError::NoCaller)?.into_owned();
        let registry = self.registry.take().ok_or(DeferError::NoCaller)?;
        match slot.defer(registry.pool(), &now_ticks) {
            Ok(()) => {
                #[cfg(feature = "lifecycle-trace")]
                crate::lifecycle_trace::downstream_deferred(Arc::as_ptr(&slot) as u64);
                registry.register(&slot);
                registry.track_deadline(&slot);
                Ok(ReplyTo {
                    slot: Some(slot),
                    registry: registry.clone(),
                    stamp: crate::instrumentation::hooks::deferred(),
                })
            }
            Err(slot::Refused::Closed) => Err(DeferError::Closed),
            Err(slot::Refused::Overloaded) => Err(DeferError::Overloaded),
        }
    }

    /// The request's deadline, fixed at send; [`Deadline::Never`] for an
    /// unbounded request and for sends. Pass it on to forward the request
    /// with the same deadline (`other.send_request(msg, reply.deadline())`).
    pub fn deadline(&self) -> Deadline {
        self.slot
            .as_ref()
            .map_or(Deadline::Never, |s| Deadline::of_ticks(s.deadline()))
    }

    /// Whether the caller can no longer be answered (the deadline passed or
    /// the caller left). Checks the deadline first. Always `true` for sends.
    pub fn is_closed(&self) -> bool {
        self.slot.as_ref().is_none_or(|s| s.is_closed(&now_ticks))
    }

    /// Answer `ActorStopped` (test doubles standing in for an absent actor:
    /// without an owning actor the reason is `NoProc`).
    pub(crate) fn discharge_stopped(mut self) {
        if let Some(slot) = self.slot.take() {
            slot.terminate(Terminal::Stopped);
        }
    }
}

impl<R: Send + 'static> Drop for PendingReply<'_, R> {
    fn drop(&mut self) {
        if let Some(slot) = self.slot.take() {
            if slot.is_terminal() {
                return;
            }
            match self.registry {
                Some(registry) => registry.dropped(slot.into_owned()),
                None => {
                    slot.terminate(Terminal::Dropped);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// ReplyTo
// ---------------------------------------------------------------------------

/// A deferred reply obligation: answer it from a later turn, a job, or
/// `stopped()`. Not `Clone`; `Send + 'static`.
///
/// Dropping it answers the caller `ReplyDropped`; while the owner is exiting
/// it is held instead and answered at the death point. The owner's death
/// answers every open token `ActorStopped` with the final exit reason,
/// wherever it is.
#[must_use = "dropping a ReplyTo answers ReplyDropped (or ActorStopped at death while the owner exits)"]
pub struct ReplyTo<R: Send + 'static> {
    slot: Option<Arc<Slot<R>>>,
    registry: Arc<Registry>,
    /// When it was deferred (sampled; zero-sized without `metrics`).
    stamp: crate::instrumentation::hooks::DeferStamp,
}

impl<R: Send + 'static> std::fmt::Debug for ReplyTo<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReplyTo")
            .field("owner", &self.registry.owner())
            .finish_non_exhaustive()
    }
}

impl<R: Send + 'static> ReplyTo<R> {
    /// Answer the caller. Returns how the reply was delivered.
    pub fn reply(mut self, value: R) -> Delivery {
        let slot = self
            .slot
            .take()
            .expect("ReplyTo holds its slot until consumed");
        // Admitted by the owner's death-point fence: once `NoProc` is
        // observable no reply succeeds (the owner's close answers the slot).
        let (delivery, unsent) =
            slot.reply_admitted(value, &now_ticks, || self.registry.reply_permit());
        drop(unsent);
        record(delivery);
        if delivery == Delivery::Delivered {
            crate::instrumentation::hooks::deferred_replied(&self.stamp);
        }
        delivery
    }

    /// Answer the caller `Err(Overloaded)`, e.g. on `InsertError::Full`.
    /// Past the owner's death point the caller gets its final reason.
    pub fn overloaded(mut self) {
        if let Some(slot) = self.slot.take() {
            // The permit covers the transition only, never its wakes.
            slot.terminate_admitted(Terminal::Overloaded, || self.registry.reply_permit());
        }
    }

    /// The request's deadline (as shortened so far).
    pub fn deadline(&self) -> Deadline {
        Deadline::of_ticks(self.slot().deadline())
    }

    /// Shorten the request's deadline to `deadline` (anything
    /// `Into<Deadline>`: a `Duration` from now, an `Instant`, or a
    /// [`Deadline`], e.g. another request's); a later deadline, or
    /// [`Deadline::Never`], leaves it unchanged. The waiting caller is woken to re-arm its timer.
    pub fn shorten_deadline(&self, deadline: impl Into<Deadline>) {
        // Only a lowered deadline gets a heap entry, so repeated calls with
        // the same or a later deadline store nothing.
        if self.slot().shorten(deadline.into().ticks(), &now_ticks) {
            self.registry.track_deadline(self.slot());
        }
    }

    /// Whether the caller can no longer be answered. Checks the deadline
    /// first.
    pub fn is_closed(&self) -> bool {
        self.slot().is_closed(&now_ticks)
    }

    /// Wait until the caller can no longer be answered (expired, abandoned,
    /// answered elsewhere or closed by the owner's death).
    #[cfg(feature = "async")]
    pub async fn closed(&self) {
        let slot = self.slot();
        loop {
            // One wait per deadline value: a shortening wakes the watchers,
            // and the loop re-arms for the earlier deadline.
            let deadline = slot.deadline();
            let mut woken = false;
            let waiting = std::future::poll_fn(|cx| {
                if woken && slot.deadline() != deadline {
                    return std::task::Poll::Ready(false);
                }
                woken = true;
                slot.poll_closed(cx.waker(), &now_ticks).map(|()| true)
            });
            let closed = if deadline == NEVER {
                waiting.await
            } else {
                let wait = Duration::from_nanos(deadline.saturating_sub(now_ticks()));
                match spawned_rt::tasks::timeout(wait, waiting).await {
                    Ok(closed) => closed,
                    Err(_) => {
                        slot.check_deadline(&now_ticks);
                        slot.is_terminal()
                    }
                }
            };
            if closed {
                return;
            }
        }
    }

    /// The actor instance that owns this obligation.
    pub fn owner(&self) -> ActorId {
        self.registry.owner()
    }

    fn slot(&self) -> &Arc<Slot<R>> {
        self.slot
            .as_ref()
            .expect("ReplyTo holds its slot until consumed")
    }

    pub(crate) fn slot_arc(&self) -> &Arc<Slot<R>> {
        self.slot()
    }
}

impl<R: Send + 'static> Drop for ReplyTo<R> {
    fn drop(&mut self) {
        if let Some(slot) = self.slot.take() {
            self.registry.dropped(slot);
        }
    }
}

// ---------------------------------------------------------------------------
// ReplyReceiver
// ---------------------------------------------------------------------------

/// The caller's side of a request: `.await` it in `tasks` mode (it is
/// [`IntoFuture`](std::future::IntoFuture)), or call [`recv`](Self::recv) in
/// `threads` mode. Yields `Ok(reply)`, or `RequestTimeout` at the deadline
/// fixed at send, `ReplyDropped`, `Overloaded`, or `ActorStopped(reason)`
/// with the owner's final exit reason when it died first.
///
/// One pointer wide: the deadline timer lives in the awaiting future
/// (`ReplyFuture`), not in the receiver, so receivers are cheap to store
/// and move. Dropping it abandons the request: a later reply is dropped and
/// any deferred credit returns at once.
pub struct ReplyReceiver<R> {
    slot: Option<Arc<Slot<R>>>,
}

impl<R> Drop for ReplyReceiver<R> {
    fn drop(&mut self) {
        if let Some(slot) = self.slot.take() {
            slot.abandon();
        }
    }
}

impl<R> std::fmt::Debug for ReplyReceiver<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReplyReceiver")
            .field("done", &self.slot.is_none())
            .finish_non_exhaustive()
    }
}

impl<R> ReplyReceiver<R> {
    fn new(slot: Arc<Slot<R>>) -> Self {
        Self { slot: Some(slot) }
    }

    /// Await with the caller's own runtime timer (the `request` and
    /// `Response` paths, which follow the runtime's clock, paused test clocks
    /// included).
    #[cfg(feature = "async")]
    pub(crate) fn into_timed_future(self) -> ReplyFuture<R> {
        ReplyFuture {
            rx: self,
            armed: NEVER,
            first_ms: 0,
            sleep: None,
            timer: true,
            tracked: false,
        }
    }

    /// [`into_timed_future`](Self::into_timed_future) for a request just
    /// sent with `Deadline::after(timeout)`: its first timer sleeps
    /// `timeout` (in whole milliseconds, rounded up: the runtime timer's
    /// resolution) instead of reading the clock again. The request was sent
    /// at most the send-to-poll gap ago, so the timer fires at or after the
    /// deadline, never before. A deadline shortened meanwhile re-arms from
    /// the clock.
    #[cfg(feature = "async")]
    pub(crate) fn into_timed_future_after(self, timeout: Duration) -> ReplyFuture<R> {
        let deadline = self.slot().deadline();
        let first_ms = u32::try_from(timeout.as_nanos().div_ceil(1_000_000)).unwrap_or(0);
        ReplyFuture {
            armed: deadline,
            first_ms,
            ..self.into_timed_future()
        }
    }

    fn slot(&self) -> &Arc<Slot<R>> {
        self.slot
            .as_ref()
            .expect("ReplyReceiver used after completion")
    }

    /// The deadline fixed at send (as the owner shortened it so far).
    pub fn deadline(&self) -> Deadline {
        Deadline::of_ticks(self.slot().deadline())
    }

    /// Block the calling thread until the reply, the deadline or the owner's
    /// death. Do not call it on an async executor thread.
    pub fn recv(mut self) -> Result<R, ActorError> {
        let slot = self
            .slot
            .take()
            .expect("ReplyReceiver used after completion");
        slot.recv_blocking(&now_ticks)
            .map_err(|terminal| terminal_error(terminal, &slot))
    }

    /// Non-blocking check: `None` while the request is still open.
    pub fn try_recv(&mut self) -> Option<Result<R, ActorError>> {
        let slot = self.slot();
        let result = slot
            .try_take(&now_ticks)?
            .map_err(|terminal| terminal_error(terminal, slot));
        self.slot = None;
        Some(result)
    }
}

#[cfg(feature = "async")]
impl<R: Send + 'static> std::future::IntoFuture for ReplyReceiver<R> {
    type Output = Result<R, ActorError>;
    type IntoFuture = ReplyFuture<R>;
    fn into_future(self) -> ReplyFuture<R> {
        ReplyFuture {
            rx: self,
            armed: NEVER,
            first_ms: 0,
            sleep: None,
            timer: false,
            tracked: false,
        }
    }
}

#[cfg(feature = "async")]
pin_project_lite::pin_project! {
    /// Awaiting a [`ReplyReceiver`]: polls the slot and, while it is open,
    /// sleeps until its deadline and then expires it.
    pub struct ReplyFuture<R> {
        rx: ReplyReceiver<R>,
        armed: u64,
        // A `request`'s timeout in milliseconds (0: none), with `armed` its
        // send-time deadline: the first timer sleeps it (no clock read).
        first_ms: u32,
        // Arm a runtime timer (`request`/`Response`), rather than putting the
        // deadline in the owner's heap (raw receivers).
        timer: bool,
        tracked: bool,
        #[pin]
        sleep: Option<spawned_rt::tasks::Sleep>,
    }
}

#[cfg(feature = "async")]
impl<R> std::fmt::Debug for ReplyFuture<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReplyFuture").finish_non_exhaustive()
    }
}

#[cfg(feature = "async")]
impl<R: Send + 'static> std::future::Future for ReplyFuture<R> {
    type Output = Result<R, ActorError>;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        use std::task::Poll;
        let mut this = self.project();
        loop {
            let slot = this.rx.slot();
            // Answered and no longer shared (the owner's turn already
            // released its reference, the usual case once the caller gets
            // to it): the value moves out without the slot lock.
            if slot::state(slot.word()) == slot::REPLIED && Arc::strong_count(slot) == 1 {
                let shared = this.rx.slot.take().expect("checked above");
                match Arc::try_unwrap(shared) {
                    Ok(owned) => {
                        return Poll::Ready(
                            owned
                                .into_reply()
                                .ok_or(ActorError::ActorStopped(crate::error::ExitReason::NoProc)),
                        );
                    }
                    Err(shared) => this.rx.slot = Some(shared),
                }
            }
            let slot = this.rx.slot();
            if let Poll::Ready(result) = slot.poll(cx.waker()) {
                let result = result.map_err(|terminal| terminal_error(terminal, slot));
                this.rx.slot = None;
                return Poll::Ready(result);
            }
            let deadline = slot.deadline();
            if deadline == NEVER {
                return Poll::Pending;
            }
            // A raw receiver registers its deadline once in the owner's
            // deadline heap; the service expires the slot (waking this
            // future) without a per-request runtime timer.
            if !*this.timer {
                if let Some(owner) = &slot.owner {
                    if !*this.tracked {
                        owner.track_deadline(slot);
                        *this.tracked = true;
                    }
                    return Poll::Pending;
                }
            }
            if *this.armed != deadline || this.sleep.is_none() {
                let wait = match std::mem::take(this.first_ms) {
                    ms if ms != 0 && *this.armed == deadline => Duration::from_millis(ms.into()),
                    _ => Duration::from_nanos(deadline.saturating_sub(now_ticks())),
                };
                this.sleep.set(Some(spawned_rt::tasks::sleep(wait)));
                *this.armed = deadline;
            }
            let sleep = this.sleep.as_mut().as_pin_mut().expect("armed above");
            match sleep.poll(cx) {
                Poll::Pending => return Poll::Pending,
                // The timer was armed for the slot's deadline (which only
                // decreases), so the deadline has passed: expire the slot.
                // This follows the runtime's timer, so paused test clocks
                // keep working.
                Poll::Ready(()) => {
                    this.sleep.set(None);
                    this.rx.slot().terminate(Terminal::TimedOut);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Test helper
// ---------------------------------------------------------------------------

/// A `PendingReply` not tied to an actor turn, and its receiver, for calling
/// a manual `Handler::handle` directly in tests (`spawned_test::reply_pair`).
/// Await the receiver only after driving the turns the reply depends on.
pub(crate) fn reply_pair<R: Send + 'static>(
    deadline: Deadline,
) -> (PendingReply<'static, R>, ReplyReceiver<R>) {
    let (mut handle, receiver) = reply_slot(deadline, None);
    let reply = PendingReply::new(handle.take(), Registry::detached());
    (reply, receiver)
}
