//! Introspection of a threads-mode actor: `info()` and labels
//! (docs/design/introspection.md, tier 0). Out of band: nothing here touches
//! send, dispatch or a turn.
use super::*;
use crate::introspection::{ActorInfo, Probe, StatusReport};
use crate::response::{RequestOptions, Response};

impl<A: Actor> Probe for ActorShared<A> {
    fn info(&self) -> ActorInfo {
        crate::introspection::build(crate::introspection::Sources {
            signal: &self.signal,
            links: &self.links,
            replies: Some(&self.replies),
            published: crate::child_handle::poll_exit_reason(&self.completion).is_some(),
            monitors: crate::monitor::targets(&self.monitors).len(),
            jobs_running: self.jobs.running(),
            #[cfg(feature = "async")]
            scope: None,
        })
    }
    fn monitors(&self) -> Vec<ActorId> {
        crate::monitor::targets(&self.monitors)
    }
    /// Through a `ChildHandle` (type-erased, so possibly an async caller):
    /// a pending `Response` with the async runtime, as a tasks-mode one;
    /// `ActorRef::get_status` blocks, as every threads-mode request.
    fn get_status(&self, timeout: Duration) -> Response<StatusReport> {
        #[cfg(feature = "async")]
        return self.status_pending(timeout, Box::new(status_report::<A>));
        #[cfg(not(feature = "async"))]
        self.status(timeout, Box::new(status_report::<A>))
    }
}

/// `get_status`'s callback: the actor between turns, and its state.
fn status_report<A: Actor>(actor: &A, ctx: &Context<A>) -> StatusReport {
    let info = crate::introspection::between_turns(Probe::info(&*ctx.shared), &ctx.shared.signal);
    let state = crate::introspection::state_view(A::FORMAT_STATUS, actor);
    StatusReport { info, state }
}

/// The type-erased probe of a new instance (set once, at spawn).
pub(super) fn probe_of<A: Actor>(shared: &Arc<ActorShared<A>>) -> std::sync::Weak<dyn Probe> {
    let weak: std::sync::Weak<dyn Probe> =
        Arc::downgrade(shared) as std::sync::Weak<ActorShared<A>>;
    weak
}

impl<A: Actor> ActorShared<A> {
    /// Run a status turn with `f` and wait for its answer, at most
    /// `timeout` (the deadline is fixed now, as a request's). Not a message:
    /// no `metrics` mailbox or request accounting (I4).
    fn status<R: Send + 'static>(&self, timeout: Duration, f: StatusFn<A, R>) -> Response<R> {
        match self.admit_status(timeout, f) {
            Some(rx) => Response::ready(crate::response::status_result(rx.recv())),
            None => Response::ready(Err(ActorError::NoProc)),
        }
    }

    /// [`status`](Self::status) without waiting: the answer is awaited (or,
    /// off an async runtime, waited for) through the returned `Response`.
    #[cfg(feature = "async")]
    fn status_pending<R: Send + 'static>(
        &self,
        timeout: Duration,
        f: StatusFn<A, R>,
    ) -> Response<R> {
        match self.admit_status(timeout, f) {
            Some(rx) => Response::status(rx),
            None => Response::ready(Err(ActorError::NoProc)),
        }
    }

    /// Queue a status envelope (`None` past the death point). Admission and
    /// its trace event are one step (`SendStatus`).
    fn admit_status<R: Send + 'static>(
        &self,
        timeout: Duration,
        f: StatusFn<A, R>,
    ) -> Option<ReplyReceiver<crate::response::StatusAnswer<R>>> {
        let deadline = crate::reply::clock::after(timeout);
        let (slot, rx) = reply_slot(deadline, Some(&self.replies), true);
        let envelope: Box<dyn Envelope<A> + Send> = Box::new(StatusEnvelope { slot, f });
        let id = self.id.raw();
        crate::lifecycle_trace::life_section(id, || {
            // Past the death point the actor is gone (E4), as for `enqueue`.
            let admitted = !self.replies.refuses_admission()
                && self.sender.send(MailboxItem::Message(envelope)).is_ok();
            crate::lifecycle_trace::intro(id, "Status", if admitted { "queued" } else { "noproc" });
            admitted.then_some(rx)
        })
    }
}

impl<A: Actor> ActorRef<A> {
    /// Read the actor's state through `f` (OTP `sys:get_state`), in a turn
    /// of its own: queued behind the messages already in the mailbox and run
    /// between handler turns, so it sees a consistent state, and it times out
    /// (5 s; `with_request_timeout(d).get_state(f)` for another) on an actor
    /// that is stuck or busy. For a stuck actor use [`info`](Self::info),
    /// which never waits for a turn.
    ///
    /// Outcomes: `Ok`, `RequestTimeout`, `ActorStopped(final reason)` (still
    /// queued at its death point), `NoProc` (sent after it) or
    /// `CallbackFailed` if `f` panicked; the actor continues after a panic
    /// in `f`, and nothing `f` did is undone. A request whose caller gave up
    /// before it was dispatched is not run. `f` runs on the actor's turn:
    /// keep it short and non-blocking.
    pub fn get_state<R, F>(&self, f: F) -> Response<R>
    where
        F: FnOnce(&A) -> R + Send + 'static,
        R: Send + 'static,
    {
        self.shared
            .status(DEFAULT_REQUEST_TIMEOUT, Box::new(move |actor, _| f(actor)))
    }

    /// The actor's [`StatusReport`] (OTP `sys:get_status`): its `info()`
    /// between turns and its state as `Actor::FORMAT_STATUS` formats it
    /// (redacted without one; see [`Inspect`](crate::introspection::Inspect)).
    /// A status turn, as [`get_state`](Self::get_state).
    pub fn get_status(&self) -> Response<StatusReport> {
        Probe::get_status(&*self.shared, DEFAULT_REQUEST_TIMEOUT)
    }

    /// The actor as it is now (OTP `process_info/1`): identity, type, label,
    /// registered name, lifecycle, links, monitors, `trap_exit`, parent,
    /// supervisor position, placement, deferred replies and jobs.
    ///
    /// Out of band: it reads shared state and short metadata locks and never
    /// waits for a turn, so it answers while a handler is wedged. Fields are
    /// not from one instant. See [`ActorInfo`].
    pub fn info(&self) -> ActorInfo {
        Probe::info(&*self.shared)
    }
}

impl<A: Actor> Context<A> {
    /// This actor as it is now; see [`ActorRef::info`]. Called from a
    /// handler, its lifecycle reads `InTurn`.
    pub fn info(&self) -> ActorInfo {
        Probe::info(&*self.shared)
    }

    /// Label this actor for operators (OTP 27 `proc_lib:set_label/1`):
    /// shown by `info()`, crash reports and tools, never used for lookup
    /// (use the [`registry`](crate::registry) for names). Replaces any
    /// earlier label. Cost: one short lock, at this call only.
    pub fn set_label(&self, label: impl Into<Arc<str>>) {
        self.shared.signal.meta.set_label(label.into());
    }
}

impl<A: Actor> RequestOptions<ActorRef<A>> {
    /// [`ActorRef::get_state`] with this handle's timeout.
    pub fn get_state<R, F>(&self, f: F) -> Response<R>
    where
        F: FnOnce(&A) -> R + Send + 'static,
        R: Send + 'static,
    {
        let timeout = RequestOptions::timeout(self);
        RequestOptions::actor_ref(self)
            .shared
            .status(timeout, Box::new(move |actor, _| f(actor)))
    }

    /// [`ActorRef::get_status`] with this handle's timeout.
    pub fn get_status(&self) -> Response<StatusReport> {
        Probe::get_status(
            &*RequestOptions::actor_ref(self).shared,
            RequestOptions::timeout(self),
        )
    }
}
