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
    #[allow(dead_code)] // used by lifecycle tracing only
    pub(crate) fn raw(self) -> u64 {
        self.0
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

/// Message delivered to a monitoring actor when its target stops: OTP's
/// `{'DOWN', Ref, process, Object, Info}`.
///
/// To monitor another actor, call `ctx.monitor(&child_handle)` and implement
/// `Handler<Down>` on the monitoring actor. Non-exhaustive: match it with
/// `Down { monitor_ref, actor, reason, .. }`.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Down {
    /// The monitor that triggered this notification.
    pub monitor_ref: MonitorRef,
    /// The monitored actor (OTP's `Object`).
    pub actor: crate::ActorId,
    /// Why the monitored actor stopped.
    pub reason: ExitReason,
}

impl Down {
    /// A down notice for monitor `monitor_ref` of `actor` (for test doubles
    /// and comparisons; the framework builds the ones it delivers).
    pub fn new(monitor_ref: MonitorRef, actor: crate::ActorId, reason: ExitReason) -> Self {
        Self {
            monitor_ref,
            actor,
            reason,
        }
    }
}

impl Message for Down {
    type Result = ();
}

mod sync {
    pub(crate) use std::sync::{Mutex, MutexGuard};
}
pub(crate) mod fence;

/// Each monitor owns a cancellation token that wakes its waiter immediately.
pub(crate) type MonitorTable =
    std::sync::Arc<fence::Entries<MonitorRef, spawned_rt::sync::CancellationToken>>;

/// Release outgoing monitors on every actor exit path, including unwinding.
pub(crate) struct MonitorCleanup(pub MonitorTable);
impl Drop for MonitorCleanup {
    fn drop(&mut self) {
        if let Some(monitors) = fence::close(&self.0) {
            // HashMap order is per-process random: wake waiters in
            // monitor-creation order, so a simulated schedule is seeded.
            #[cfg(spawned_sim)]
            let monitors = {
                let mut monitors = monitors;
                monitors.sort_unstable_by_key(|(r, _)| r.0);
                monitors
            };
            for (_, token) in monitors {
                token.cancel();
            }
        }
    }
}

/// Also removes a waiter whose never-polled future is rejected or aborted.
pub(crate) struct MonitorRegistration(pub MonitorTable, pub MonitorRef);
impl Drop for MonitorRegistration {
    fn drop(&mut self) {
        drop(fence::remove(&self.0, &self.1, || {}));
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
        let token = spawned_rt::sync::CancellationToken::new;
        entries.insert(mine, fence::Entry::Active(token()));
        entries.insert(other, fence::Entry::Active(token()));
        *table.lock().unwrap() = Some(entries);
        drop(MonitorRegistration(table.clone(), mine));
        let entries = table.lock().unwrap();
        let entries = entries.as_ref().unwrap();
        assert!(!entries.contains_key(&mine));
        assert!(entries.contains_key(&other));
    }

    /// A finished waiter's registration drop leaves its queued `Down` for
    /// dispatch; `demonitor` flushes it, and dispatch then skips it.
    #[test]
    fn demonitor_flushes_a_queued_down() {
        let table: MonitorTable = std::sync::Arc::new(fence::open());
        for flush in [false, true] {
            let monitor = MonitorRef::next();
            let token = spawned_rt::sync::CancellationToken::new();
            assert_eq!(
                fence::register(&table, monitor, token, || false, || false, || {}, || {}),
                fence::Registration::Active
            );
            let mut enqueued = false;
            assert!(fence::deliver(&table, &monitor, || enqueued = true));
            assert!(enqueued);
            drop(MonitorRegistration(table.clone(), monitor));
            if flush {
                assert!(fence::demonitor(&table, &monitor, || {}).is_none());
            }
            assert_eq!(fence::take_queued(&table, &monitor), !flush);
            assert!(!fence::take_queued(&table, &monitor), "taken once");
        }
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
