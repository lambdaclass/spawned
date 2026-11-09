//! Shared state of the `tasks` and `threads` probes.
use crate::inbox::{Inbox, Outcome};
use spawned_concurrency::{error::ActorError, message::Message};
use std::sync::{Arc, Mutex, MutexGuard};

type ReplyFn<M> = Box<dyn FnMut(&M) -> <M as Message>::Result + Send>;

pub(crate) struct ProbeCore<M: Message> {
    pub(crate) inbox: Arc<Inbox>,
    reply: Mutex<Option<ReplyFn<M>>>,
    // Held while a send or request is admitted and logged, so `close()`
    // linearizes with them: nothing is logged after `close()` returns.
    closed: Mutex<bool>,
}

impl<M: Message> ProbeCore<M> {
    pub(crate) fn new(reply: Option<ReplyFn<M>>) -> Self {
        Self {
            inbox: Arc::new(Inbox::default()),
            reply: Mutex::new(reply),
            closed: Mutex::new(false),
        }
    }

    fn open(&self) -> Result<MutexGuard<'_, bool>, ActorError> {
        let closed = self.closed.lock().unwrap_or_else(|p| p.into_inner());
        if *closed {
            Err(ActorError::ActorStopped)
        } else {
            Ok(closed)
        }
    }

    pub(crate) fn close(&self) {
        *self.closed.lock().unwrap_or_else(|p| p.into_inner()) = true;
    }

    pub(crate) fn send(&self, msg: M) -> Result<(), ActorError> {
        let _open = self.open()?;
        self.inbox.push(msg, Outcome::Handled);
        Ok(())
    }

    /// `Ok(None)` means the request was logged but there is no reply: the
    /// caller drops the reply sender and observes `ActorStopped`.
    ///
    /// A request is logged `InFlight` on admission and `Handled` once the reply
    /// closure returns; a panicking closure logs it as `Crashed`.
    pub(crate) fn request(&self, msg: M) -> Result<Option<M::Result>, ActorError> {
        let open = self.open()?;
        let mut reply = self.reply.lock().unwrap_or_else(|p| p.into_inner());
        if reply.is_none() {
            tracing::error!(
                message = std::any::type_name::<M>(),
                "spawned-test Probe got a request but has no reply; build it with \
                 Probe::replying(..). The caller sees ActorStopped."
            );
            self.inbox.push(msg, Outcome::Unexpected);
            return Ok(None);
        }
        let mut record = self.inbox.arrive(msg);
        // Admitted before any `close()`; the reply may finish after it.
        drop(open);
        let reply = reply.as_mut().expect("checked above");
        // A panic here drops `record`, which logs the request as Crashed.
        let value = reply(record.message::<M>());
        record.replied(Outcome::Handled);
        record.finish();
        Ok(Some(value))
    }
}
