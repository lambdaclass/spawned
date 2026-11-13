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
/// `Deadline::Never` in ticks.
pub(crate) const NEVER_TICKS: u64 = NEVER;
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
    // A non-poisoning slot lock; payloads/wakers are never dropped under it.
    // Keep the standard LockResult surface so the shared Loom code uses the
    // same calls, while the ordinary path inlines to parking_lot's fast lock.
    pub(crate) struct SlotMutex<T>(parking_lot::Mutex<T>);
    pub(crate) type SlotMutexGuard<'a, T> = parking_lot::MutexGuard<'a, T>;
    impl<T> SlotMutex<T> {
        pub(crate) fn new(value: T) -> Self {
            Self(parking_lot::Mutex::new(value))
        }
        #[inline]
        pub(crate) fn lock(&self) -> std::sync::LockResult<SlotMutexGuard<'_, T>> {
            // Production never poisons; recovery is exercised by Loom's lock.
            Ok(self.0.lock())
        }
        #[inline]
        pub(crate) fn into_inner(self) -> std::sync::LockResult<T> {
            Ok(self.0.into_inner())
        }
    }
    /// Lifecycle-trace builds keep the owner in each slot (its events name
    /// it); other builds keep nothing, so a slot stays small.
    #[cfg(feature = "lifecycle-trace")]
    pub(crate) type SlotOwner = Option<Arc<super::registry::Registry>>;
    #[cfg(not(feature = "lifecycle-trace"))]
    pub(crate) type SlotOwner = ();
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
#[inline]
pub(crate) fn now_ticks() -> u64 {
    clock::now()
}

/// The clock deadlines are fixed against: one seam, so the source (today
/// the precise `spawned_rt` clock) can change without touching the reply
/// protocol. Ticks are nanoseconds on the `spawned_rt` epoch; `NEVER` is
/// reserved.
pub(crate) mod clock {
    use super::NEVER;
    use std::time::Duration;

    /// Ticks now.
    #[inline]
    pub(crate) fn now() -> u64 {
        spawned_rt::time::now_ticks()
    }

    /// The deadline `timeout` from now, in ticks (`NEVER` if it overflows),
    /// computed directly: no `Instant` arithmetic or conversion.
    #[inline]
    pub(crate) fn after(timeout: Duration) -> u64 {
        u64::try_from(timeout.as_nanos())
            .ok()
            .and_then(|nanos| now().checked_add(nanos))
            .filter(|ticks| *ticks < spawned_rt::time::MAX_TICKS)
            .unwrap_or(NEVER)
    }
}

/// A type-level default lets ordinary requests keep their fixed budget out
/// of the future's captured state, while custom requests carry a Duration.
#[cfg(feature = "async")]
pub(crate) trait RequestBudget: Send {
    fn into_duration(self) -> Duration;
}
#[cfg(feature = "async")]
pub(crate) struct DefaultRequestBudget;
#[cfg(feature = "async")]
impl RequestBudget for DefaultRequestBudget {
    #[inline]
    fn into_duration(self) -> Duration {
        crate::response::DEFAULT_REQUEST_TIMEOUT
    }
}
#[cfg(feature = "async")]
impl RequestBudget for Duration {
    #[inline]
    fn into_duration(self) -> Duration {
        self
    }
}

/// The slot deadline uses the monotonic deadline-service clock. The caller's
/// timer uses a relative runtime budget: a paused Tokio clock can differ from
/// the service clock and its absolute instants must not be stored in the slot.
#[cfg(feature = "async")]
#[inline]
pub(crate) fn request_timer(duration: Duration) -> (u64, Option<spawned_rt::tasks::Sleep>) {
    let ticks = clock::after(duration);
    let timer = (ticks != NEVER).then(|| spawned_rt::tasks::sleep(duration));
    (ticks, timer)
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

    pub(crate) fn of_ticks(ticks: u64) -> Self {
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
                .unwrap_or(crate::error::ExitReason::NoProc),
        ),
    }
}

/// A new reply slot for a request with `deadline`, and the caller's receiver.
///
/// `owner` is the receiving instance's registry. The slot takes no
/// reference to it (no shared reference count per request): the owner's
/// close records the final reason in the slot, and an undispatched
/// envelope is disposed of through the owner's mailbox
/// ([`SlotHandle::dispose`]). A `raw` request with a deadline (the public
/// `send_request`) records the owner's registry key in the slot's word, so
/// its receiver, awaited directly and still pending, puts the deadline in
/// that owner's heap without a runtime timer (a request answered before
/// that never touches the heap); the `request` paths arm their own timer
/// and pass `raw = false`.
#[inline]
pub(crate) fn reply_slot<R: Send + 'static>(
    deadline: u64,
    owner: Option<&Arc<Registry>>,
    raw: bool,
) -> (SlotHandle<R>, ReplyReceiver<R>) {
    #[cfg(feature = "lifecycle-trace")]
    let slot_owner = owner.cloned();
    #[cfg(not(feature = "lifecycle-trace"))]
    let slot_owner = ();
    let key = match owner {
        Some(owner) if raw && deadline != NEVER => owner.key(),
        _ => 0,
    };
    let slot = Arc::new(Slot::with_owner_key(deadline, slot_owner, key));
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
    /// Hold `slot` again (an undispatched envelope's request).
    #[cfg(feature = "async")]
    pub(crate) fn of(slot: Option<Arc<Slot<R>>>) -> Self {
        Self(slot)
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
/// `NoProc` by the send itself): closed `STOPPED` now (trace builds, whose
/// slots know their owner, dispose of it there).
impl<R: Send + 'static> Drop for SlotHandle<R> {
    #[inline]
    fn drop(&mut self) {
        if let Some(slot) = self.0.take() {
            #[cfg(feature = "lifecycle-trace")]
            if let Some(owner) = slot.owner.clone() {
                owner.dispose(slot);
                return;
            }
            slot.close();
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
    #[inline]
    pub fn reply(self, value: R) -> Delivery {
        // Consumed here: the obligation's `Drop` (an unanswered reply) has
        // nothing to do once the slot is taken, so it is skipped.
        let mut this = std::mem::ManuallyDrop::new(self);
        match this.slot.take() {
            None => {
                drop(value);
                Delivery::CallerGone
            }
            Some(slot) => {
                let (delivery, unsent) = slot.reply_in_turn(value);
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
                let token = ReplyTo::new(slot, registry.clone());
                // Construct the token first: a registration panic must also
                // run its Drop and retire the external-ownership hold.
                registry.register(token.slot());
                registry.track_deadline(token.slot());
                Ok(token)
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
    #[inline]
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
    /// Every token owns one external hold through its entire Drop. Forgetting
    /// a token leaks that hold too, conservatively keeping the slow turn path.
    fn new(slot: Arc<Slot<R>>, registry: Arc<Registry>) -> Self {
        let token = Self {
            slot: Some(slot),
            registry,
            stamp: crate::instrumentation::hooks::deferred(),
        };
        token.registry.token_added();
        token
    }

    /// Answer the caller. Returns how the reply was delivered.
    pub fn reply(mut self, value: R) -> Delivery {
        let slot = self
            .slot
            .take()
            .expect("ReplyTo holds its slot until consumed");
        // Admitted by the owner's death-point fence: once `NoProc` is
        // observable no reply succeeds (the owner's close answers the slot).
        let (delivery, unsent) = slot.reply_admitted(value, || self.registry.reply_permit());
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
        let _ownership = self.registry.token_drop_guard();
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
    #[inline]
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
        let deadline = self.slot().deadline();
        let sleep = (deadline != NEVER).then(|| {
            spawned_rt::tasks::sleep(Duration::from_nanos(deadline.saturating_sub(now_ticks())))
        });
        ReplyFuture { rx: self, sleep }
    }

    /// Use the relative runtime timer created before enqueue, while the slot
    /// retains its send-time deadline on the monotonic service clock.
    #[cfg(feature = "async")]
    pub(crate) fn into_timed_future_with(
        self,
        sleep: Option<spawned_rt::tasks::Sleep>,
    ) -> ReplyFuture<R> {
        ReplyFuture { rx: self, sleep }
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
            sleep: None,
        }
    }
}

#[cfg(feature = "async")]
pin_project_lite::pin_project! {
    /// Awaiting a [`ReplyReceiver`]: polls the slot and, while it is open,
    /// sleeps until its deadline and then expires it.
    pub struct ReplyFuture<R> {
        rx: ReplyReceiver<R>,
        // The runtime timer: created with a timed future (`request`,
        // `Response`), or on demand for a raw receiver whose owner's heap
        // cannot take its deadline. Its size is Tokio's `Timeout`'s.
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

    #[inline]
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
            let word = slot.word();
            if this.sleep.is_none() {
                // A raw receiver: its owner's deadline heap (found by the key
                // in the word) expires the slot, waking this future, with no
                // per-request runtime timer.
                let key = slot::owner_key(word);
                if key != 0 {
                    if word & slot::RX_TRACKED != 0 {
                        return Poll::Pending;
                    }
                    if let Some(owner) = Registry::by_key(key) {
                        slot.mark_rx_tracked();
                        owner.track_deadline(slot);
                        return Poll::Pending;
                    }
                    // The owner is gone: fall back to the runtime timer.
                }
                let wait = Duration::from_nanos(deadline.saturating_sub(now_ticks()));
                this.sleep.set(Some(spawned_rt::tasks::sleep(wait)));
            } else if word & slot::SHORTENED != 0 && slot.take_shortened() {
                // A shortening wakes this future: re-arm once from the clock
                // for the deadline read after taking the mark.
                let deadline = slot.deadline();
                let wait = Duration::from_nanos(deadline.saturating_sub(now_ticks()));
                this.sleep.set(Some(spawned_rt::tasks::sleep(wait)));
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
    let (mut handle, receiver) = reply_slot(deadline.ticks(), None, false);
    let reply = PendingReply::new(handle.take(), Registry::detached());
    (reply, receiver)
}
