//! Shared state of the `tasks` and `threads` probes.
use crate::inbox::{Inbox, Outcome};
use spawned_concurrency::{error::ActorError, message::Message};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

type ReplyFn<M> = Box<dyn FnMut(&M) -> <M as Message>::Result + Send>;

pub(crate) struct ProbeCore<M: Message> {
    pub(crate) inbox: Inbox,
    reply: Mutex<Option<ReplyFn<M>>>,
    closed: AtomicBool,
}

impl<M: Message> ProbeCore<M> {
    pub(crate) fn new(reply: Option<ReplyFn<M>>) -> Self {
        Self {
            inbox: Inbox::default(),
            reply: Mutex::new(reply),
            closed: AtomicBool::new(false),
        }
    }

    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }

    pub(crate) fn send(&self, msg: M) -> Result<(), ActorError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(ActorError::ActorStopped);
        }
        self.inbox.push(msg, Outcome::Handled);
        Ok(())
    }

    /// `Ok(None)` means the request was logged but there is no reply: the
    /// caller drops the reply sender and observes `ActorStopped`.
    pub(crate) fn request(&self, msg: M) -> Result<Option<M::Result>, ActorError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(ActorError::ActorStopped);
        }
        let mut reply = self.reply.lock().unwrap_or_else(|p| p.into_inner());
        match reply.as_mut() {
            Some(reply) => {
                let value = reply(&msg);
                self.inbox.push(msg, Outcome::Handled);
                Ok(Some(value))
            }
            None => {
                tracing::error!(
                    message = std::any::type_name::<M>(),
                    "spawned-test Probe got a request but has no reply; build it with \
                     Probe::replying(..). The caller sees ActorStopped."
                );
                self.inbox.push(msg, Outcome::Unexpected);
                Ok(None)
            }
        }
    }
}
