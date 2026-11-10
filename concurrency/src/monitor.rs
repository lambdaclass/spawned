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
/// [`Context::monitor`]: crate::threads::Context::monitor
/// [`Context::demonitor`]: crate::threads::Context::demonitor
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

/// Each monitor owns a cancellation token that wakes its waiter immediately.
pub(crate) type MonitorTable = std::sync::Arc<
    std::sync::Mutex<
        Option<std::collections::HashMap<MonitorRef, spawned_rt::sync::CancellationToken>>,
    >,
>;

/// Release outgoing monitors on every actor exit path, including unwinding.
pub(crate) struct MonitorCleanup(pub MonitorTable);
impl Drop for MonitorCleanup {
    fn drop(&mut self) {
        let monitors = self.0.lock().unwrap_or_else(|p| p.into_inner()).take();
        if let Some(monitors) = monitors {
            // HashMap order is per-process random; under simulation, wake
            // waiters in monitor-creation order so the schedule is seeded.
            #[cfg(spawned_sim)]
            let monitors: std::collections::BTreeMap<u64, _> =
                monitors.into_iter().map(|(r, t)| (r.0, t)).collect();
            for token in monitors.into_values() {
                token.cancel();
            }
        }
    }
}

/// Also removes a waiter whose never-polled future is rejected or aborted.
pub(crate) struct MonitorRegistration(pub MonitorTable, pub MonitorRef);
impl Drop for MonitorRegistration {
    fn drop(&mut self) {
        if let Some(entries) = self.0.lock().unwrap_or_else(|p| p.into_inner()).as_mut() {
            entries.remove(&self.1);
        }
    }
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
    fn dropped_registration_removes_only_its_own_entry() {
        // A waiter future that is rejected or aborted before it runs must not
        // leave its entry behind for the rest of the observer's lifetime.
        let table: MonitorTable = Default::default();
        let (mine, other) = (MonitorRef::next(), MonitorRef::next());
        let mut entries = std::collections::HashMap::new();
        entries.insert(mine, spawned_rt::sync::CancellationToken::new());
        entries.insert(other, spawned_rt::sync::CancellationToken::new());
        *table.lock().unwrap() = Some(entries);
        drop(MonitorRegistration(table.clone(), mine));
        let entries = table.lock().unwrap();
        let entries = entries.as_ref().unwrap();
        assert!(!entries.contains_key(&mine));
        assert!(entries.contains_key(&other));
    }

    #[test]
    fn registration_dropped_after_cleanup_is_harmless() {
        let table: MonitorTable = Default::default();
        drop(MonitorRegistration(table.clone(), MonitorRef::next()));
        assert!(table.lock().unwrap().is_none());
    }

    #[test]
    fn monitor_ref_display() {
        let r = MonitorRef::next();
        assert!(format!("{r}").starts_with("MonitorRef("));
    }
}
