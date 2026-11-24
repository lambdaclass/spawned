//! Introspection of a tasks-mode actor: `info()` and labels
//! (docs/design/introspection.md, tier 0). Out of band: nothing here touches
//! send, dispatch or a turn.
use super::*;
use crate::introspection::{ActorInfo, Probe};

impl<A: Actor> Probe for ActorShared<A> {
    fn info(&self) -> ActorInfo {
        crate::introspection::build(crate::introspection::Sources {
            signal: &self.signal,
            links: &self.links,
            replies: Some(&self.replies),
            published: crate::child_handle::poll_exit_reason(&self.completion_rx).is_some(),
            monitors: crate::monitor::targets(&self.monitors).len(),
            jobs_running: self.jobs.running(),
            scope: self.managed.as_deref(),
        })
    }
    fn monitors(&self) -> Vec<ActorId> {
        crate::monitor::targets(&self.monitors)
    }
}

/// The type-erased probe of a new instance (set once, at spawn).
pub(super) fn probe_of<A: Actor>(shared: &Arc<ActorShared<A>>) -> std::sync::Weak<dyn Probe> {
    let weak: std::sync::Weak<dyn Probe> =
        Arc::downgrade(shared) as std::sync::Weak<ActorShared<A>>;
    weak
}

impl<A: Actor> ActorRef<A> {
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
