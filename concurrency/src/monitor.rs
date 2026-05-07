use crate::error::ExitReason;
use crate::message::Message;
use std::sync::atomic::{AtomicU64, Ordering};

// ---------------------------------------------------------------------------
// MonitorRef
// ---------------------------------------------------------------------------

static NEXT_MONITOR_ID: AtomicU64 = AtomicU64::new(1);

/// Opaque identifier for an active monitor relationship. Returned by
/// [`Context::monitor`] and used to cancel via [`Context::demonitor`].
///
/// Multiple independent monitors are allowed on the same target — each call
/// to `monitor` returns a distinct `MonitorRef`.
///
/// [`Context::monitor`]: crate::tasks::actor::Context::monitor
/// [`Context::demonitor`]: crate::tasks::actor::Context::demonitor
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MonitorRef(u64);

impl MonitorRef {
    pub(crate) fn next() -> Self {
        Self(NEXT_MONITOR_ID.fetch_add(1, Ordering::Relaxed))
    }
}

impl std::fmt::Display for MonitorRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "MonitorRef({})", self.0)
    }
}

// ---------------------------------------------------------------------------
// Down
// ---------------------------------------------------------------------------

/// Message delivered to a monitoring actor when its target stops.
///
/// To monitor another actor, call `ctx.monitor(&child_handle)` and implement
/// `Handler<Down>` on the monitoring actor.
#[derive(Debug, Clone, PartialEq)]
pub struct Down {
    /// The monitor that triggered this notification.
    pub monitor_ref: MonitorRef,
    /// Why the monitored actor stopped.
    pub reason: ExitReason,
}

impl Message for Down {
    type Result = ();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monitor_ref_is_unique() {
        let a = MonitorRef::next();
        let b = MonitorRef::next();
        assert_ne!(a, b);
    }

    #[test]
    fn monitor_ref_display() {
        let r = MonitorRef::next();
        assert!(format!("{r}").starts_with("MonitorRef("));
    }
}
