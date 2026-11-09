//! Supervisor lifecycle trace recording for TLA+ trace validation.
//!
//! With the `lifecycle-trace` feature, `run_supervisor`/`run_child` record one
//! event per action of `verification/tla/Supervision.tla`, tagged with the
//! supervisor's trace id. `verification/tla/trace_check.py` then checks that
//! each recorded trace is a behavior of the model.
//!
//! The model's actions are atomic; the code's are not. Every event is
//! recorded inside a process-wide *section*, and code that reads the
//! supervisor's admission gate to decide an action (or closes it) does the
//! read, the decision and the recording in one section. This gives a single
//! linearization of gate flips and gate-dependent actions. Without the
//! feature, sections and recording compile to nothing.
#![doc(hidden)]

/// One recorded model action. `child` is the 1-based declaration index (0
/// for supervisor and environment actions); `arg` is the action's parameter
/// (branch, outcome, exit reason or flag), empty when the action has none;
/// `count` is the retry budget held after `BackoffDone`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub action: &'static str,
    pub child: usize,
    pub arg: &'static str,
    pub count: usize,
    pub round: u64,
}

impl Event {
    pub(crate) fn sup(action: &'static str) -> Self {
        Self::new(action, 0, "")
    }
    pub(crate) fn new(action: &'static str, child: usize, arg: &'static str) -> Self {
        Self {
            action,
            child,
            arg,
            count: 0,
            round: 0,
        }
    }
    pub(crate) fn count(mut self, count: usize) -> Self {
        self.count = count;
        self
    }
    pub(crate) fn round(mut self, round: u64) -> Self {
        self.round = round;
        self
    }
    /// JSON object understood by `verification/tla/trace_check.py`.
    pub fn to_json(&self) -> String {
        format!(
            r#"{{"a":"{}","c":{},"x":"{}","n":{},"r":{}}}"#,
            self.action, self.child, self.arg, self.count, self.round
        )
    }
}

/// Cross-supervisor ownership event, recorded separately from policy tapes.
#[derive(Debug, Clone)]
pub struct TreeEvent {
    pub action: &'static str,
    pub node: u64,
    pub slot: usize,
    pub generation: u64,
    pub peer: u64,
    pub arity: usize,
    pub cancelled: bool,
}
impl TreeEvent {
    pub(crate) fn new(action: &'static str, node: u64) -> Self {
        Self {
            action,
            node,
            slot: 0,
            generation: 0,
            peer: 0,
            arity: 0,
            cancelled: false,
        }
    }
    pub(crate) fn child(mut self, slot: usize, generation: u64) -> Self {
        self.slot = slot;
        self.generation = generation;
        self
    }
    pub(crate) fn peer(mut self, peer: u64, arity: usize) -> Self {
        self.peer = peer;
        self.arity = arity;
        self
    }
    pub(crate) fn cancelled(mut self, cancelled: bool) -> Self {
        self.cancelled = cancelled;
        self
    }
    pub fn to_json(&self) -> String {
        format!(
            r#"{{"a":"{}","node":{},"slot":{},"gen":{},"peer":{},"arity":{},"cancelled":{}}}"#,
            self.action,
            self.node,
            self.slot,
            self.generation,
            self.peer,
            self.arity,
            self.cancelled
        )
    }
}

pub(crate) fn flag(value: bool) -> &'static str {
    if value {
        "true"
    } else {
        "false"
    }
}

/// Model name of an exit reason (`Supervision.tla` `Reasons`).
pub(crate) fn reason(reason: Option<&crate::ExitReason>) -> &'static str {
    use crate::ExitReason;
    match reason {
        None => "none",
        Some(ExitReason::Normal) => "Normal",
        Some(ExitReason::Shutdown) => "Shutdown",
        Some(ExitReason::Panic(_)) => "Panic",
        Some(ExitReason::Kill) => "Kill",
    }
}

pub(crate) use imp::*;

#[cfg(feature = "lifecycle-trace")]
mod imp {
    use super::{Event, TreeEvent};
    use crate::system::lock;
    use std::{
        cell::Cell,
        collections::BTreeMap,
        sync::{
            atomic::{AtomicU64, Ordering},
            Mutex, MutexGuard,
        },
    };

    /// Identifies one supervisor's trace.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) struct TraceId(pub(crate) u64);
    impl TraceId {
        pub(crate) fn next() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(1);
            Self(NEXT.fetch_add(1, Ordering::Relaxed))
        }
    }

    static TREES: Mutex<Vec<TreeEvent>> = Mutex::new(Vec::new());
    static SECTION: Mutex<()> = Mutex::new(());
    static TRACES: Mutex<BTreeMap<u64, Vec<Event>>> = Mutex::new(BTreeMap::new());
    thread_local! {
        static DEPTH: Cell<usize> = const { Cell::new(0) };
        static SUPPRESS_CLOSE: Cell<bool> = const { Cell::new(false) };
    }

    struct Held(#[allow(dead_code)] Option<MutexGuard<'static, ()>>);
    impl Drop for Held {
        fn drop(&mut self) {
            DEPTH.with(|d| d.set(d.get() - 1));
        }
    }

    /// Run `f` as one atomic step of the recorded linearization. Reentrant;
    /// `f` must not await.
    pub(crate) fn atomic<R>(f: impl FnOnce() -> R) -> R {
        let guard = DEPTH.with(|d| {
            let outer = d.get() == 0;
            d.set(d.get() + 1);
            outer.then(|| lock(&SECTION))
        });
        let _held = Held(guard);
        f()
    }

    pub(crate) fn emit(id: TraceId, event: impl FnOnce() -> Event) {
        atomic(|| {
            lock(&TRACES).entry(id.0).or_default().push(event());
        });
    }

    pub(crate) fn emit_some(id: TraceId, event: impl FnOnce() -> Option<Event>) {
        atomic(|| {
            if let Some(event) = event() {
                lock(&TRACES).entry(id.0).or_default().push(event);
            }
        });
    }

    /// Run `f` (a `Core::fail`) so that the gate flip it causes is not
    /// recorded as a separate `Close`: the caller records the model action
    /// whose `Fail` closes the gate.
    pub(crate) fn failing<R>(f: impl FnOnce() -> R) -> R {
        atomic(|| {
            let previous = SUPPRESS_CLOSE.with(|s| s.replace(true));
            let result = f();
            SUPPRESS_CLOSE.with(|s| s.set(previous));
            result
        })
    }

    pub(crate) fn close_suppressed() -> bool {
        SUPPRESS_CLOSE.with(Cell::get)
    }

    pub(crate) fn tree(id: TraceId, make: impl FnOnce(u64) -> TreeEvent) {
        atomic(|| lock(&TREES).push(make(id.0)));
    }
    /// Extract one connected ownership tree in observed order. Other tests'
    /// nodes remain buffered; nested IDs are followed through TAttach events.
    pub fn take_tree(root: u64) -> Vec<TreeEvent> {
        atomic(|| {
            let mut events = lock(&TREES);
            let mut nodes = std::collections::BTreeSet::from([root]);
            loop {
                let before = nodes.len();
                for event in events.iter() {
                    if event.action == "TAttach" && nodes.contains(&event.node) {
                        nodes.insert(event.peer);
                    }
                }
                if nodes.len() == before {
                    break;
                }
            }
            let mut result = Vec::new();
            events.retain(|event| {
                if nodes.contains(&event.node) {
                    result.push(event.clone());
                    false
                } else {
                    true
                }
            });
            result
        })
    }

    /// Remove and return the events recorded for a supervisor.
    pub fn take(id: u64) -> Vec<Event> {
        lock(&TRACES).remove(&id).unwrap_or_default()
    }
}

#[cfg(not(feature = "lifecycle-trace"))]
mod imp {
    use super::{Event, TreeEvent};

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) struct TraceId;
    impl TraceId {
        #[inline(always)]
        pub(crate) fn next() -> Self {
            Self
        }
    }

    #[inline(always)]
    pub(crate) fn tree(_: TraceId, _: impl FnOnce(u64) -> TreeEvent) {}
    #[inline(always)]
    pub(crate) fn atomic<R>(f: impl FnOnce() -> R) -> R {
        f()
    }
    #[inline(always)]
    pub(crate) fn emit(_: TraceId, _: impl FnOnce() -> Event) {}
    #[inline(always)]
    pub(crate) fn emit_some(_: TraceId, _: impl FnOnce() -> Option<Event>) {}
    #[inline(always)]
    pub(crate) fn failing<R>(f: impl FnOnce() -> R) -> R {
        f()
    }
}

#[cfg(feature = "lifecycle-trace")]
pub use imp::{take, take_tree};
