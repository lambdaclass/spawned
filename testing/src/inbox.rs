//! Shared message log behind mocks and probes: blocking and async waiters.
use std::any::{Any, TypeId};
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// What a mock or probe did with one message it received.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// A configured rule (or `allow_sends`) answered it.
    Handled,
    /// A `discard` rule swallowed it, as a lost message would be.
    Discarded,
    /// A `crash_on` rule panicked on it.
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
    payload: Option<Box<dyn Any + Send>>,
}

#[derive(Default)]
pub(crate) struct Inbox {
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
    fn lock(&self) -> MutexGuard<'_, Vec<Entry>> {
        self.entries.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub(crate) fn push<M: Send + 'static>(&self, msg: M, outcome: Outcome) {
        let type_name = std::any::type_name::<M>();
        tracing::trace!(message = type_name, ?outcome, "spawned-test received");
        self.lock().push(Entry {
            type_id: TypeId::of::<M>(),
            received: Received { type_name, outcome },
            payload: Some(Box::new(msg)),
        });
        self.arrived.notify_all();
        #[cfg(feature = "async")]
        self.changed.0.send_modify(|n| *n += 1);
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
            .filter(|e| e.type_id == id)
            .filter_map(|e| e.payload.take())
            .map(downcast::<M>)
            .collect()
    }

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

    /// Remove and return the oldest not-yet-taken message of type `M`.
    fn take_first<M: 'static>(entries: &mut [Entry]) -> Option<M> {
        let id = TypeId::of::<M>();
        entries
            .iter_mut()
            .find(|e| e.type_id == id && e.payload.is_some())
            .and_then(|e| e.payload.take())
            .map(downcast::<M>)
    }

    pub(crate) fn expect_blocking<M: 'static>(&self, timeout: Duration, who: &str) -> M {
        let deadline = Instant::now() + timeout;
        let mut entries = self.lock();
        loop {
            if let Some(msg) = Self::take_first::<M>(&mut entries) {
                return msg;
            }
            let now = Instant::now();
            if now >= deadline {
                missing::<M>(who, timeout, &entries);
            }
            entries = self
                .arrived
                .wait_timeout(entries, deadline - now)
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
    }

    pub(crate) fn expect_none_blocking(&self, window: Duration, who: &str) {
        let deadline = Instant::now() + window;
        let mut entries = self.lock();
        loop {
            unwanted(who, window, &entries);
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
    pub(crate) async fn expect_async<M: 'static>(&self, timeout: Duration, who: &str) -> M {
        let mut changes = self.changed.0.subscribe();
        let wait = async {
            loop {
                // Mark the version seen before checking, so no push is missed.
                changes.borrow_and_update();
                if let Some(msg) = Self::take_first::<M>(&mut self.lock()) {
                    return msg;
                }
                // The sender lives in `self`, so this only returns on a push.
                let _ = changes.changed().await;
            }
        };
        match spawned_rt::tasks::timeout(timeout, wait).await {
            Ok(msg) => msg,
            Err(_) => {
                let mut entries = self.lock();
                // A push may have landed as the timer fired.
                match Self::take_first::<M>(&mut entries) {
                    Some(msg) => msg,
                    None => missing::<M>(who, timeout, &entries),
                }
            }
        }
    }

    #[cfg(feature = "async")]
    pub(crate) async fn expect_none_async(&self, window: Duration, who: &str) {
        let mut changes = self.changed.0.subscribe();
        let watch = async {
            loop {
                changes.borrow_and_update();
                unwanted(who, window, &self.lock());
                let _ = changes.changed().await;
            }
        };
        let _ = spawned_rt::tasks::timeout(window, watch).await;
        unwanted(who, window, &self.lock());
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
        .filter(|e| e.payload.is_some())
        .map(|e| e.received.type_name)
        .collect()
}

fn missing<M>(who: &str, timeout: Duration, entries: &[Entry]) -> ! {
    panic!(
        "{who}: expected a `{}` within {timeout:?}, got none; {} message(s) received in total, \
         not yet expected: {:?}",
        std::any::type_name::<M>(),
        entries.len(),
        pending_names(entries),
    )
}

fn unwanted(who: &str, window: Duration, entries: &[Entry]) {
    let pending = pending_names(entries);
    if !pending.is_empty() {
        panic!("{who}: expected no message for {window:?}, but has not-yet-expected {pending:?}");
    }
}
