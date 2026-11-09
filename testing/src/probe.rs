//! Shared state of the `tasks` and `threads` probes.
use crate::inbox::{Inbox, Outcome, Pending};
use spawned_concurrency::{error::ActorError, message::Message};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::ThreadId;

type ReplyFn<M> = Box<dyn FnMut(&M) -> <M as Message>::Result + Send>;

/// Lock order: `closed` is only ever held alone (admission and logging, no
/// user code); `reply` is taken after `closed` is released and held while the
/// reply closure runs. The closure may therefore call `close()`, `send()` or
/// another probe freely; a nested request to the *same* probe from its own
/// closure panics instead of deadlocking.
pub(crate) struct ProbeCore<M: Message> {
    pub(crate) inbox: Arc<Inbox>,
    has_reply: bool,
    reply: Mutex<Option<ReplyFn<M>>>,
    // The thread running the reply closure, to report re-entrant requests.
    replying_on: Mutex<Option<ThreadId>>,
    // Held while a send or request is admitted and logged, so `close()`
    // linearizes with them: nothing is logged after `close()` returns.
    closed: Mutex<bool>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Clears `replying_on` when the closure returns or unwinds.
struct Replying<'a>(&'a Mutex<Option<ThreadId>>);
impl Drop for Replying<'_> {
    fn drop(&mut self) {
        *lock(self.0) = None;
    }
}

impl<M: Message> ProbeCore<M> {
    pub(crate) fn new(reply: Option<ReplyFn<M>>) -> Self {
        Self {
            inbox: Arc::new(Inbox::default()),
            has_reply: reply.is_some(),
            reply: Mutex::new(reply),
            replying_on: Mutex::new(None),
            closed: Mutex::new(false),
        }
    }

    fn open(&self) -> Result<MutexGuard<'_, bool>, ActorError> {
        let closed = lock(&self.closed);
        if *closed {
            Err(ActorError::ActorStopped)
        } else {
            Ok(closed)
        }
    }

    pub(crate) fn close(&self) {
        *lock(&self.closed) = true;
    }

    pub(crate) fn send(&self, msg: M) -> Result<(), ActorError> {
        let _open = self.open()?;
        self.inbox.push(msg, Outcome::Handled);
        Ok(())
    }

    /// `Ok(None)` means the request was logged but there is no reply: the
    /// caller drops the reply sender and observes `ActorStopped`.
    ///
    /// A request is logged `InFlight` on admission. The returned [`Pending`]
    /// is finished by the caller *after* it has put the reply in the reply
    /// channel, so `expect` never returns before the requester can receive
    /// it. A panicking closure logs the request as `Crashed`.
    pub(crate) fn request(&self, msg: M) -> Result<Option<(M::Result, Pending)>, ActorError> {
        let open = self.open()?;
        if !self.has_reply {
            tracing::error!(
                message = std::any::type_name::<M>(),
                "spawned-test Probe got a request but has no reply; build it with \
                 Probe::replying(..). The caller sees ActorStopped."
            );
            self.inbox.push(msg, Outcome::Unexpected);
            return Ok(None);
        }
        let mut record = self.inbox.arrive(msg);
        // Admitted before any `close()`; the reply may finish after it. No
        // user code runs while `closed` is held.
        drop(open);
        let me = std::thread::current().id();
        if *lock(&self.replying_on) == Some(me) {
            // Unwinding drops `record`, which logs this nested request as Crashed.
            panic!(
                "spawned-test Probe<{}>: its reply closure sent a request to the same \
                 probe; a reply closure cannot request its own probe",
                std::any::type_name::<M>()
            );
        }
        let mut reply = lock(&self.reply);
        *lock(&self.replying_on) = Some(me);
        let _replying = Replying(&self.replying_on);
        let reply = reply.as_mut().expect("has_reply is set at construction");
        // A panic here drops `record`, which logs the request as Crashed.
        let value = reply(record.message::<M>());
        record.replied(Outcome::Handled);
        Ok(Some((value, record)))
    }
}
