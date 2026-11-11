use std::collections::HashMap;
use spawned_concurrency::message::Message;
//! Shared message log behind mocks and probes: blocking and async waiters.
use std::any::{Any, TypeId};
use std::panic::Location;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// What a mock or probe did with one message it received.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Received, but its handling has not finished: the reply closure or a
    /// `delay` is still running. `expect` and `take_all` skip it until it is
    /// resolved; `expect_no_message` reports it. It stays `InFlight` if the
    /// mock stopped before the reply existed (for example, killed mid-delay);
    /// a `discard`ed message is recorded `Discarded` even then.
    InFlight,
    /// A configured rule (or `allow_sends`) answered it. For a probe, recorded
    /// once the reply is in the requester's reply channel. For a mock,
    /// recorded when its handler returns the reply to the framework, after
    /// any `delay`: the framework sends it right after, so a caller awaiting
    /// the reply gets it, but `expect` may return a moment before the reply
    /// reaches the caller's channel (a handler cannot observe the send).
    Handled,
    /// A `discard` rule swallowed it, as a lost message would be.
    Discarded,
    /// A `crash_on` rule panicked on it, or its reply closure panicked.
    Crashed,
    /// No rule matched: the mock panicked, or a probe without a reply dropped
    /// the request so the caller saw `ActorStopped`.
    Unexpected,
}

/// One entry of the receive log, in arrival order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Received {
    /// `std::any::type_name` of the message.
    pub type_name: &'static str,
    pub outcome: Outcome,
}

pub(crate) struct Entry {
    type_id: TypeId,
    received: Received,
    // Taken by `expect`/`take_all`; the log line stays for `received()`.
    // `None` while in flight: the handler still owns the message.
    payload: Option<Box<dyn Any + Send>>,
}

impl Entry {
    /// Resolved and not yet taken: what `expect` and `take_all` return.
    fn ready(&self, id: TypeId) -> bool {
        self.type_id == id && self.payload.is_some() && self.received.outcome != Outcome::InFlight
    }

    /// Received and not yet taken, resolved or not: what `expect_no_message`
    /// reports.
    fn pending(&self) -> bool {
        self.payload.is_some() || self.received.outcome == Outcome::InFlight
    }
}

/// A message logged as [`Outcome::InFlight`] on arrival and resolved by
/// [`finish`](Pending::finish) once its reply exists. Dropped unresolved, it
/// records [`Outcome::Crashed`] until [`replied`](Pending::replied) (the reply
/// closure panicked) and keeps [`Outcome::InFlight`] after it (the handler was
/// dropped during its delay), except that a discarded message is recorded
/// [`Outcome::Discarded`] either way. The message always stays in the log.
pub(crate) struct Pending {
    inbox: Arc<Inbox>,
    index: usize,
    msg: Option<Box<dyn Any + Send>>,
    on_drop: Outcome,
    done: Outcome,
}

impl Pending {
    pub(crate) fn message<M: 'static>(&self) -> &M {
        self.msg
            .as_ref()
            .and_then(|m| m.downcast_ref::<M>())
            .expect("pending message has its arrival type")
    }

    /// The reply value exists; [`finish`](Pending::finish) records `done`.
    pub(crate) fn replied(&mut self, done: Outcome) {
        // A discarded message is lost whether or not the handler finishes its
        // delay; any other outcome needs the reply to have been returned.
        self.on_drop = if done == Outcome::Discarded {
            Outcome::Discarded
        } else {
            Outcome::InFlight
        };
        self.done = done;
    }

    /// Record the outcome given to [`replied`](Pending::replied).
    pub(crate) fn finish(mut self) {
        let done = self.done;
        self.resolve(done);
    }

    fn resolve(&mut self, outcome: Outcome) {
        if let Some(msg) = self.msg.take() {
            self.inbox.resolve(self.index, outcome, msg);
        }
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        let outcome = self.on_drop;
        self.resolve(outcome);
    }
}

#[derive(Default)]
pub(crate) struct Inbox {
    /// Deferred reply tokens by message type (`Mock::defer`).
    deferred: Mutex<HashMap<std::any::TypeId, Vec<Box<dyn std::any::Any + Send>>>>,
    entries: Mutex<Vec<Entry>>,
    arrived: Condvar,
    #[cfg(feature = "async")]
    changed: AsyncSignal,
}

#[cfg(feature = "async")]
struct AsyncSignal(spawned_rt::tasks::watch::Sender<u64>);
#[cfg(feature = "async")]
impl Default for AsyncSignal {
    fn default() -> Self {
        Self(spawned_rt::tasks::watch::channel(0).0)
    }
}

impl Inbox {
    pub(crate) fn store_deferred<M: Message>(&self, token: spawned_concurrency::reply::ReplyTo<M::Result>) {
        self.deferred
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entry(std::any::TypeId::of::<M>())
            .or_default()
            .push(Box::new(token));
    }
    pub(crate) fn take_deferred<M: Message>(&self) -> Vec<spawned_concurrency::reply::ReplyTo<M::Result>> {
        let tokens = self
            .deferred
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&std::any::TypeId::of::<M>())
            .unwrap_or_default();
        tokens
            .into_iter()
            .filter_map(|t| t.downcast().ok().map(|t| *t))
            .collect()
    }
    fn lock(&self) -> MutexGuard<'_, Vec<Entry>> {
        self.entries.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn notify(&self) {
        self.arrived.notify_all();
        #[cfg(feature = "async")]
        self.changed.0.send_modify(|n| *n += 1);
    }

    /// Log `msg` with its final outcome.
    pub(crate) fn push<M: Send + 'static>(&self, msg: M, outcome: Outcome) {
        let type_name = std::any::type_name::<M>();
        tracing::trace!(message = type_name, ?outcome, "spawned-test received");
        self.lock().push(Entry {
            type_id: TypeId::of::<M>(),
            received: Received { type_name, outcome },
            payload: Some(Box::new(msg)),
        });
        self.notify();
    }

    /// Log `msg` as [`Outcome::InFlight`] now; the returned [`Pending`]
    /// records its outcome once the handler is done with it.
    pub(crate) fn arrive<M: Send + 'static>(self: &Arc<Self>, msg: M) -> Pending {
        let type_name = std::any::type_name::<M>();
        tracing::trace!(message = type_name, "spawned-test received, in flight");
        let index = {
            let mut entries = self.lock();
            entries.push(Entry {
                type_id: TypeId::of::<M>(),
                received: Received {
                    type_name,
                    outcome: Outcome::InFlight,
                },
                payload: None,
            });
            entries.len() - 1
        };
        self.notify();
        Pending {
            inbox: self.clone(),
            index,
            msg: Some(Box::new(msg)),
            on_drop: Outcome::Crashed,
            done: Outcome::Handled,
        }
    }

    // Entries are never removed, so `index` stays valid.
    fn resolve(&self, index: usize, outcome: Outcome, msg: Box<dyn Any + Send>) {
        {
            let mut entries = self.lock();
            let entry = &mut entries[index];
            tracing::trace!(
                message = entry.received.type_name,
                ?outcome,
                "spawned-test resolved"
            );
            entry.received.outcome = outcome;
            entry.payload = Some(msg);
        }
        self.notify();
    }

    pub(crate) fn received(&self) -> Vec<Received> {
        self.lock().iter().map(|e| e.received.clone()).collect()
    }

    pub(crate) fn count<M: 'static>(&self) -> usize {
        let id = TypeId::of::<M>();
        self.lock().iter().filter(|e| e.type_id == id).count()
    }

    pub(crate) fn take_all<M: 'static>(&self) -> Vec<M> {
        let id = TypeId::of::<M>();
        self.lock()
            .iter_mut()
            .filter(|e| e.ready(id))
            .filter_map(|e| e.payload.take())
            .map(downcast::<M>)
            .collect()
    }

    #[track_caller]
    pub(crate) fn assert_no_unexpected(&self, who: &str) {
        let unexpected: Vec<_> = self
            .lock()
            .iter()
            .filter(|e| e.received.outcome == Outcome::Unexpected)
            .map(|e| e.received.type_name)
            .collect();
        assert!(
            unexpected.is_empty(),
            "{who}: received messages without a rule: {unexpected:?}"
        );
    }

    /// Remove and return the oldest resolved, not-yet-taken message of type `M`.
    fn take_first<M: 'static>(entries: &mut [Entry]) -> Option<M> {
        let id = TypeId::of::<M>();
        entries
            .iter_mut()
            .find(|e| e.ready(id))
            .and_then(|e| e.payload.take())
            .map(downcast::<M>)
    }

    #[track_caller]
    pub(crate) fn expect_blocking<M: 'static>(
        &self,
        timeout: Duration,
        who: &str,
        at: &Location<'_>,
    ) -> M {
        let deadline = Instant::now() + timeout;
        let mut entries = self.lock();
        loop {
            if let Some(msg) = Self::take_first::<M>(&mut entries) {
                return msg;
            }
            let now = Instant::now();
            if now >= deadline {
                missing::<M>(who, timeout, &entries, at);
            }
            // Spurious wakeups only loop back to the deadline check.
            entries = self
                .arrived
                .wait_timeout(entries, deadline - now)
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
    }

    #[track_caller]
    pub(crate) fn expect_none_blocking(&self, window: Duration, who: &str, at: &Location<'_>) {
        let deadline = Instant::now() + window;
        let mut entries = self.lock();
        loop {
            unwanted(who, window, &entries, at);
            let now = Instant::now();
            if now >= deadline {
                return;
            }
            entries = self
                .arrived
                .wait_timeout(entries, deadline - now)
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
    }

    // The async waiters bound themselves with the runtime's timer only (no
    // `std::time::Instant`), so they agree with `exit_within` and friends and
    // auto-advance under `#[tokio::test(start_paused = true)]`.
    #[cfg(feature = "async")]
    pub(crate) async fn expect_async<M: 'static>(
        &self,
        timeout: Duration,
        who: &str,
        at: &Location<'_>,
    ) -> M {
        let mut changes = self.changed.0.subscribe();
        let wait = async {
            loop {
                // Mark the version seen before checking, so no push is missed.
                changes.borrow_and_update();
                if let Some(msg) = Self::take_first::<M>(&mut self.lock()) {
                    return msg;
                }
                changed_or_park(&mut changes).await;
            }
        };
        match spawned_rt::tasks::timeout(timeout, wait).await {
            Ok(msg) => msg,
            Err(_) => {
                let mut entries = self.lock();
                // A push may have landed as the timer fired.
                match Self::take_first::<M>(&mut entries) {
                    Some(msg) => msg,
                    None => missing::<M>(who, timeout, &entries, at),
                }
            }
        }
    }

    #[cfg(feature = "async")]
    pub(crate) async fn expect_none_async(&self, window: Duration, who: &str, at: &Location<'_>) {
        let mut changes = self.changed.0.subscribe();
        let watch = async {
            loop {
                changes.borrow_and_update();
                unwanted(who, window, &self.lock(), at);
                changed_or_park(&mut changes).await;
            }
        };
        let _ = spawned_rt::tasks::timeout(window, watch).await;
        // Also report a message that landed as the timer fired, matching the
        // blocking variant, which checks once more after its last wait.
        unwanted(who, window, &self.lock(), at);
    }
}

/// Wait for the next push. The sender lives in the `Inbox` the waiter
/// borrows, so `changed()` cannot fail today; if it ever did, park until the
/// caller's timeout (which does the final check) instead of spinning.
#[cfg(feature = "async")]
async fn changed_or_park(changes: &mut spawned_rt::tasks::watch::Receiver<u64>) {
    if changes.changed().await.is_err() {
        std::future::pending::<()>().await;
    }
}

fn downcast<M: 'static>(payload: Box<dyn Any + Send>) -> M {
    *payload
        .downcast::<M>()
        .expect("spawned-test log entry type id matches its payload")
}

fn pending_names(entries: &[Entry]) -> Vec<&'static str> {
    entries
        .iter()
        .filter(|e| e.pending())
        .map(|e| e.received.type_name)
        .collect()
}

#[track_caller]
fn missing<M>(who: &str, timeout: Duration, entries: &[Entry], at: &Location<'_>) -> ! {
    panic!(
        "{who}: expected a `{}` within {timeout:?}, got none; {} message(s) received in total, \
         not yet expected (including in flight): {:?} (expect called at {at})",
        std::any::type_name::<M>(),
        entries.len(),
        pending_names(entries),
    )
}

#[track_caller]
fn unwanted(who: &str, window: Duration, entries: &[Entry], at: &Location<'_>) {
    let pending = pending_names(entries);
    if !pending.is_empty() {
        panic!(
            "{who}: expected no message for {window:?}, but has not-yet-expected {pending:?} \
             (expect_no_message called at {at})"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dropped_pending_keeps_discard_and_in_flight_apart() {
        let inbox = Arc::new(Inbox::default());
        // Dropped during a delay after a discard decision: still Discarded.
        let mut discarded = inbox.arrive(1u32);
        discarded.replied(Outcome::Discarded);
        drop(discarded);
        // Dropped during a delay before its reply was returned: InFlight.
        let mut handled = inbox.arrive(2u32);
        handled.replied(Outcome::Handled);
        drop(handled);
        // Dropped before the reply closure returned: Crashed.
        drop(inbox.arrive(3u32));
        let outcomes: Vec<_> = inbox.received().iter().map(|r| r.outcome).collect();
        assert_eq!(
            outcomes,
            vec![Outcome::Discarded, Outcome::InFlight, Outcome::Crashed]
        );
        assert_eq!(inbox.take_all::<u32>(), vec![1, 3]);
    }
}
