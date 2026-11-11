//! Reply transport for requests: reply slots, tokens and receivers
//! (docs/design/deferred-replies.md).
//!
//! Every request has a reply slot ([`slot::Slot`]) shared by the envelope,
//! then by the handler's [`PendingReply`] (and, after [`PendingReply::defer`],
//! a storable [`ReplyTo`]), and by the caller's [`ReplyReceiver`]. Each slot
//! gets exactly one terminal discharge.

pub(crate) mod deadline;
pub mod jobs;
mod pending;
pub(crate) mod registry;
pub(crate) mod slot;
pub(crate) mod turn;

pub use pending::{InsertError, PendingReplies};
pub use registry::DEFAULT_MAX_OUTSTANDING;
pub use slot::Delivery;

use crate::error::ActorError;
use crate::ActorId;
use registry::Registry;
use slot::{Slot, Terminal, NEVER};
use spawned_rt::time::Instant;
use std::marker::PhantomData;
use std::sync::Arc;
use std::time::Duration;

/// Production synchronization types for [`slot`]; the Loom model supplies its
/// own (`tests/reply_slot_loom.rs`).
mod sync {
    pub(crate) use std::sync::atomic::{AtomicU64, AtomicU8, AtomicUsize, Ordering};
    pub(crate) use std::sync::{Arc, Condvar, Mutex, MutexGuard};
    pub(crate) use std::thread::yield_now;
    pub(crate) type SlotOwner = Option<Arc<super::registry::Registry>>;

    /// Wait on `condvar` for at most `ticks` clock ticks (nanoseconds), or
    /// until notified when `None`.
    pub(crate) fn wait_ticks<'a, T>(
        condvar: &Condvar,
        guard: MutexGuard<'a, T>,
        ticks: Option<u64>,
    ) -> MutexGuard<'a, T> {
        match ticks {
            None => condvar.wait(guard).unwrap_or_else(|p| p.into_inner()),
            Some(ticks) => {
                condvar
                    .wait_timeout(guard, Duration::from_nanos(ticks))
                    .unwrap_or_else(|p| p.into_inner())
                    .0
            }
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
/// absolute instant or [`Deadline::Never`]. The default for `request` and
/// `request_raw` is [`DEFAULT_REQUEST_TIMEOUT`](crate::response::DEFAULT_REQUEST_TIMEOUT)
/// after the send.
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

    fn from_ticks(ticks: u64) -> Option<Instant> {
        (ticks != NEVER).then(|| spawned_rt::time::from_ticks(ticks))
    }
}

fn record(_delivery: Delivery) {
    #[cfg(feature = "metrics")]
    crate::instrumentation::reply(_delivery == Delivery::Delivered);
}

fn terminal_error(terminal: Terminal) -> ActorError {
    match terminal {
        Terminal::Dropped => ActorError::ReplyDropped,
        Terminal::Overloaded => ActorError::Overloaded,
        Terminal::TimedOut => ActorError::RequestTimeout,
        Terminal::Stopped | Terminal::Abandoned | Terminal::Replied => ActorError::ActorStopped,
    }
}

/// A new reply slot for a request with `deadline`, and the caller's receiver.
///
/// `owner` is the receiving instance's registry: an awaited raw receiver
/// puts its deadline in that owner's heap instead of arming its own timer.
pub(crate) fn reply_slot<R>(
    deadline: Deadline,
    owner: Option<&Arc<Registry>>,
) -> (SlotHandle<R>, ReplyReceiver<R>) {
    let slot = Arc::new(Slot::with_owner(deadline.ticks(), owner.cloned()));
    (SlotHandle(Some(slot.clone())), ReplyReceiver::new(slot))
}

// ---------------------------------------------------------------------------
// Envelope side
// ---------------------------------------------------------------------------

/// The envelope's hold on its request's slot. Dropped undispatched (mailbox
/// disposal at death, a rejected enqueue), it answers `ActorStopped`.
pub(crate) struct SlotHandle<R>(Option<Arc<Slot<R>>>);

impl<R> SlotHandle<R> {
    /// No caller: the message was sent, not requested.
    pub(crate) fn detached() -> Self {
        Self(None)
    }
    pub(crate) fn take(&mut self) -> Option<Arc<Slot<R>>> {
        self.0.take()
    }
}

impl<R> Drop for SlotHandle<R> {
    fn drop(&mut self) {
        if let Some(slot) = self.0.take() {
            slot.close();
        }
    }
}

// ---------------------------------------------------------------------------
// PendingReply
// ---------------------------------------------------------------------------

/// Why [`PendingReply::defer`] did not return a token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
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
/// dropped is answered `ReplyDropped` when the turn returns, or
/// `ActorStopped` when it panics. A message reached by `send` has a detached
/// `PendingReply`: `reply` drops the value and `defer` returns
/// [`DeferError::NoCaller`].
pub struct PendingReply<'t, R: Send + 'static> {
    slot: Option<Arc<Slot<R>>>,
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
        Self {
            slot,
            registry: Some(registry),
            _turn: PhantomData,
        }
    }

    /// A `PendingReply` for a sent message: there is no caller.
    pub fn detached() -> Self {
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
                let (delivery, unsent) = slot.reply(value, &now_ticks);
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
        let slot = self.slot.take().ok_or(DeferError::NoCaller)?;
        let registry = self.registry.take().ok_or(DeferError::NoCaller)?;
        match slot.defer(registry.pool(), &now_ticks) {
            Ok(()) => {
                registry.register(&slot);
                registry.track_deadline(&slot);
                Ok(ReplyTo {
                    slot: Some(slot),
                    registry: registry.clone(),
                })
            }
            Err(slot::Refused::Closed) => Err(DeferError::Closed),
            Err(slot::Refused::Overloaded) => Err(DeferError::Overloaded),
        }
    }

    /// The request's deadline, or `None` for [`Deadline::Never`] and sends.
    pub fn deadline(&self) -> Option<Instant> {
        self.slot
            .as_ref()
            .and_then(|s| Deadline::from_ticks(s.deadline()))
    }

    /// Whether the caller can no longer be answered (the deadline passed or
    /// the caller left). Checks the deadline first. Always `true` for sends.
    pub fn is_closed(&self) -> bool {
        self.slot.as_ref().is_none_or(|s| s.is_closed(&now_ticks))
    }

    /// Answer `ActorStopped` (test doubles standing in for an absent actor).
    #[doc(hidden)]
    pub fn __discharge_stopped(mut self) {
        if let Some(slot) = self.slot.take() {
            slot.terminate(Terminal::Stopped);
        }
    }

    /// Whether this is a request (has a caller) rather than a send.
    pub fn has_caller(&self) -> bool {
        self.slot.is_some()
    }
}

impl<R: Send + 'static> Drop for PendingReply<'_, R> {
    fn drop(&mut self) {
        if let Some(slot) = self.slot.take() {
            match self.registry {
                Some(registry) => registry.dropped(slot),
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
/// Dropping it answers the caller `ReplyDropped` (or `ActorStopped` while the
/// owner is exiting). The owner's death answers every open token
/// `ActorStopped`, wherever it is.
#[must_use = "dropping a ReplyTo answers ReplyDropped (or ActorStopped while the owner exits)"]
pub struct ReplyTo<R: Send + 'static> {
    slot: Option<Arc<Slot<R>>>,
    registry: Arc<Registry>,
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
        let (delivery, unsent) = slot.reply(value, &now_ticks);
        drop(unsent);
        record(delivery);
        delivery
    }

    /// Answer the caller `Err(Overloaded)`, e.g. on `InsertError::Full`.
    pub fn overloaded(mut self) {
        if let Some(slot) = self.slot.take() {
            slot.terminate(Terminal::Overloaded);
        }
    }

    /// The request's deadline, or `None` for [`Deadline::Never`].
    pub fn deadline(&self) -> Option<Instant> {
        Deadline::from_ticks(self.slot().deadline())
    }

    /// Shorten the deadline to `at` (never extends it).
    pub fn with_deadline(&self, at: Instant) {
        // Only a lowered deadline gets a heap entry, so repeated
        // `with_deadline` calls with the same or a later instant store nothing.
        if self
            .slot()
            .shorten(spawned_rt::time::to_ticks(at), &now_ticks)
        {
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
/// fixed at send, `ReplyDropped`, `Overloaded`, or `ActorStopped` when the
/// owner died.
///
/// One pointer wide: the deadline timer lives in the awaiting future
/// ([`ReplyFuture`]), not in the receiver, so receivers are cheap to store
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
            sleep: None,
            timer: true,
            tracked: false,
        }
    }

    fn slot(&self) -> &Arc<Slot<R>> {
        self.slot
            .as_ref()
            .expect("ReplyReceiver used after completion")
    }

    /// The deadline fixed at send, or `None` for [`Deadline::Never`].
    pub fn deadline(&self) -> Option<Instant> {
        Deadline::from_ticks(self.slot().deadline())
    }

    /// Block the calling thread until the reply, the deadline or the owner's
    /// death. Do not call it on an async executor thread.
    pub fn recv(mut self) -> Result<R, ActorError> {
        let slot = self
            .slot
            .take()
            .expect("ReplyReceiver used after completion");
        slot.recv_blocking(&now_ticks).map_err(terminal_error)
    }

    /// Non-blocking check: `None` while the request is still open.
    pub fn try_recv(&mut self) -> Option<Result<R, ActorError>> {
        let result = self.slot().try_take(&now_ticks)?.map_err(terminal_error);
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
            if let Poll::Ready(result) = slot.poll(cx.waker()) {
                this.rx.slot = None;
                return Poll::Ready(result.map_err(terminal_error));
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
                let wait = Duration::from_nanos(deadline.saturating_sub(now_ticks()));
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
#[doc(hidden)]
pub fn reply_pair<R: Send + 'static>(
    deadline: Deadline,
) -> (PendingReply<'static, R>, ReplyReceiver<R>) {
    let (mut handle, receiver) = reply_slot(deadline, None);
    let reply = PendingReply::new(handle.take(), Registry::detached());
    (reply, receiver)
}
