//! Lifecycle trace recording for TLA+ trace validation.
//!
//! With the `lifecycle-trace` feature, `run_supervisor`/`run_child` record one
//! event per action of `verification/tla/Supervision.tla`, tagged with the
//! supervisor's trace id; the reply and job cores record `DeferredReplies.tla`
//! and `DeferredJobs.tla` actions; and the actor runtime records
//! `Lifecycle.tla` actions (`LifeEvent`, see `TraceLifecycle.tla`) for the
//! actors spawned while `record_lifecycle` is on.
//! `verification/tla/trace_check.py` then checks that each recorded trace is
//! a behavior of its model.
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

/// One pipe-to-self job action of `verification/tla/DeferredJobs.tla`,
/// recorded per owner instance. `job` is a process-wide job number
/// (normalized by the checker); `x` is the action's parameter: the message
/// kind (`emit`, `done`, `deadline`) and fence verdict for `Dispatch`,
/// whether the terminal message was enqueued for `Finish` (`notified`,
/// `quiet`, or `held` when scheduled emits keep the result) and `Release`,
/// and whether a delayed emit was enqueued for `Fire` (`emit`, `dropped`;
/// `release` when it was the last and took a held result).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobEvent {
    pub action: &'static str,
    pub job: u64,
    pub x: &'static str,
    pub keyed: bool,
    pub deadline: bool,
    pub fenced: bool,
    pub release: bool,
}

impl JobEvent {
    pub(crate) fn new(action: &'static str, job: u64) -> Self {
        Self {
            action,
            job,
            x: "",
            keyed: false,
            deadline: false,
            fenced: false,
            release: false,
        }
    }
    /// JSON object understood by `verification/tla/trace_check.py`.
    pub fn to_json(&self) -> String {
        format!(
            r#"{{"a":"{}","j":{},"x":"{}","keyed":{},"dl":{},"fenced":{},"rel":{}}}"#,
            self.action, self.job, self.x, self.keyed, self.deadline, self.fenced, self.release
        )
    }
}

/// One reply-slot action of `verification/tla/DeferredReplies.tla`, recorded
/// per owner instance: `Send`, `Dispatch`, `Discharge` (with the terminal
/// kind in `x`: `ok`, `timeout`, `dropped`, `stopped`, `overloaded`,
/// `abandoned`), `Exit` and `Death`. `req` identifies the slot (normalized by
/// the checker at each `Send`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplyEvent {
    pub action: &'static str,
    pub req: u64,
    pub x: &'static str,
}

impl ReplyEvent {
    /// JSON object understood by `verification/tla/trace_check.py`.
    pub fn to_json(&self) -> String {
        format!(
            r#"{{"a":"{}","r":{},"x":"{}"}}"#,
            self.action, self.req, self.x
        )
    }
}

/// One base-lifecycle action of `verification/tla/Lifecycle.tla`, recorded
/// for the actors spawned while `record_lifecycle` is on. `t` is the actor
/// the action belongs to (the target of a signal or message, the caller of
/// link/monitor, the receiver of a notice), `s` the other actor or the
/// sender (`ActorId` numbers, normalized by the checker; 0 for outside code),
/// `x` the reason, flag, kind or outcome, `m` the monitor, `o` the notice
/// origin (`death`, `late`) or `sup` for a supervisor's signal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LifeEvent {
    pub action: &'static str,
    pub t: u64,
    pub s: u64,
    pub x: &'static str,
    pub m: u64,
    pub k: &'static str,
    pub o: &'static str,
}

impl LifeEvent {
    #[allow(dead_code)] // recording is compiled out without the feature
    pub(crate) fn new(action: &'static str, t: u64) -> Self {
        Self {
            action,
            t,
            s: 0,
            x: "",
            m: 0,
            k: "",
            o: "",
        }
    }
    #[allow(dead_code)]
    pub(crate) fn s(mut self, s: u64) -> Self {
        self.s = s;
        self
    }
    #[allow(dead_code)]
    pub(crate) fn x(mut self, x: &'static str) -> Self {
        self.x = x;
        self
    }
    #[allow(dead_code)]
    pub(crate) fn m(mut self, m: u64) -> Self {
        self.m = m;
        self
    }
    #[allow(dead_code)]
    pub(crate) fn k(mut self, k: &'static str) -> Self {
        self.k = k;
        self
    }
    #[allow(dead_code)]
    pub(crate) fn o(mut self, o: &'static str) -> Self {
        self.o = o;
        self
    }
    /// JSON object understood by `verification/tla/trace_check.py`.
    pub fn to_json(&self) -> String {
        format!(
            r#"{{"a":"{}","t":{},"s":{},"x":"{}","m":{},"k":"{}","o":"{}"}}"#,
            self.action, self.t, self.s, self.x, self.m, self.k, self.o
        )
    }
}

/// Model name of an exit reason (`Lifecycle.tla` `Reasons`).
#[allow(dead_code)] // recording is compiled out without the feature
pub(crate) fn life_reason(reason: &crate::ExitReason) -> &'static str {
    use crate::ExitReason;
    match reason {
        ExitReason::Normal => "Normal",
        ExitReason::Shutdown => "Shutdown",
        ExitReason::Panic(_) => "Panic",
        ExitReason::Kill => "Kill",
        ExitReason::Killed => "Killed",
        ExitReason::NoProc => "NoProc",
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
        // Supervision.tla has one class per restart behaviour: Killed and
        // NoProc are abnormal, as Kill.
        Some(_) => "Kill",
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
            atomic::{AtomicBool, AtomicU64, Ordering},
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
    static JOBS: Mutex<BTreeMap<u64, Vec<super::JobEvent>>> = Mutex::new(BTreeMap::new());
    /// Owner instances whose reply-slot and job events are recorded.
    static WATCHED: Mutex<std::collections::BTreeSet<u64>> =
        Mutex::new(std::collections::BTreeSet::new());

    /// Record the reply-slot and job events of owner instance `owner` (an
    /// `ActorId`'s number) until [`take_replies`] or [`take_jobs`]. Opt-in, so
    /// other tests built with the feature neither accumulate events nor
    /// allocate inside the trace section.
    pub fn watch_deferred(owner: u64) {
        lock(&WATCHED).insert(owner);
    }

    fn watched(owner: u64) -> bool {
        lock(&WATCHED).contains(&owner)
    }

    /// A job number for [`super::JobEvent::job`].
    pub(crate) fn next_job() -> u64 {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        NEXT.fetch_add(1, Ordering::Relaxed)
    }

    /// Record a job action of owner instance `owner` (inside the section).
    pub(crate) fn job(owner: u64, event: impl FnOnce() -> super::JobEvent) {
        atomic(|| {
            if watched(owner) {
                lock(&JOBS).entry(owner).or_default().push(event());
            }
        });
    }

    static REPLIES: Mutex<BTreeMap<u64, Vec<super::ReplyEvent>>> = Mutex::new(BTreeMap::new());
    static DEAD: Mutex<std::collections::BTreeSet<u64>> =
        Mutex::new(std::collections::BTreeSet::new());

    /// Record a reply-slot action of owner instance `owner`.
    pub(crate) fn reply(owner: u64, action: &'static str, req: u64, x: &'static str) {
        atomic(|| {
            if !watched(owner) {
                return;
            }
            // The owner's death closes everything still open in one model
            // step (`Death`); the closes it performs are not separate events.
            if action == "Discharge" && x == "stopped" && lock(&DEAD).contains(&owner) {
                return;
            }
            if action == "Death" && !lock(&DEAD).insert(owner) {
                return;
            }
            // A cleanup deadline after the death point (a slow destructor
            // before publication) is not a step of the reply registry, which
            // is sealed: the model's `CleanupDeadline` precedes death.
            if action == "Stuck" && lock(&DEAD).contains(&owner) {
                return;
            }
            lock(&REPLIES)
                .entry(owner)
                .or_default()
                .push(super::ReplyEvent { action, req, x });
        });
    }

    /// Remove and return the reply events recorded for an owner instance.
    pub fn take_replies(owner: u64) -> Vec<super::ReplyEvent> {
        lock(&WATCHED).remove(&owner);
        lock(&DEAD).remove(&owner);
        lock(&REPLIES).remove(&owner).unwrap_or_default()
    }

    // Linked jobs and their downstream requests (DeferredReplies.tla
    // `JobCallB`, `BDispatch`, `BReply`): while a `pipe_reply*` job's work
    // runs, the thread carries its link (owner instance, request slot); a
    // request it makes to another actor B is recorded on the owner's trace,
    // and B's decisions on that request are recorded there too.
    thread_local! {
        static LINK: Cell<Option<(u64, u64)>> = const { Cell::new(None) };
    }
    /// B's slots requested from a linked job: slot -> (owner, request, deferred).
    static DOWNSTREAM: Mutex<BTreeMap<u64, (u64, u64, bool)>> = Mutex::new(BTreeMap::new());

    /// Run `f` (a linked job's work, or one poll of it) with its link set.
    pub(crate) fn in_link<R>(link: Option<(u64, u64)>, f: impl FnOnce() -> R) -> R {
        struct Restore(Option<(u64, u64)>);
        impl Drop for Restore {
            fn drop(&mut self) {
                LINK.with(|l| l.set(self.0));
            }
        }
        let _restore = Restore(LINK.with(|l| l.replace(link)));
        f()
    }

    /// A request slot `slot` was created at instance `owner`: if the current
    /// thread runs a linked job of a watched instance, it is that request's
    /// downstream call (`CallB`).
    pub(crate) fn downstream_sent(owner: u64, slot: u64) {
        let Some((a, request)) = LINK.with(Cell::get) else {
            return;
        };
        if a == owner {
            return;
        }
        atomic(|| {
            if watched(a) {
                lock(&DOWNSTREAM).insert(slot, (a, request, false));
                crate::lifecycle_trace::reply(a, "CallB", request, "");
            }
        });
    }

    /// B deferred a downstream request (`BDispatch` to `stateB`).
    pub(crate) fn downstream_deferred(slot: u64) {
        atomic(|| {
            let found = lock(&DOWNSTREAM).get_mut(&slot).map(|entry| {
                entry.2 = true;
                (entry.0, entry.1)
            });
            if let Some((a, request)) = found {
                crate::lifecycle_trace::reply(a, "BDispatch", request, "stateB");
            }
        });
    }

    /// A downstream request's slot left OPEN with `kind` (inside the slot's
    /// trace section): B's answer in its turn (`BDispatch` to `done`) or a
    /// later one (`BReply`). The caller's side (abandoned, timeout) is not
    /// B's step: the model folds it into the job's end (`EndDown`).
    pub(crate) fn downstream_closed(slot: u64, kind: &'static str) {
        let Some((a, request, deferred)) = lock(&DOWNSTREAM).remove(&slot) else {
            return;
        };
        if matches!(kind, "abandoned" | "timeout") {
            return;
        }
        if deferred {
            crate::lifecycle_trace::reply(a, "BReply", request, kind);
        } else {
            crate::lifecycle_trace::reply(a, "BDispatch", request, "done");
        }
    }

    /// Remove and return the job events recorded for an owner instance.
    pub fn take_jobs(owner: u64) -> Vec<super::JobEvent> {
        lock(&WATCHED).remove(&owner);
        lock(&JOBS).remove(&owner).unwrap_or_default()
    }

    // Base lifecycle (`Lifecycle.tla`): one process-wide recording at a time.
    static LIFE_ON: AtomicBool = AtomicBool::new(false);
    static MEMBERS: Mutex<std::collections::BTreeSet<u64>> =
        Mutex::new(std::collections::BTreeSet::new());
    static LIFE: Mutex<Vec<super::LifeEvent>> = Mutex::new(Vec::new());

    /// Start recording the base-lifecycle events of every actor spawned from
    /// now on until [`take_lifecycle`]. Tests that record must not run
    /// concurrently with each other.
    pub fn record_lifecycle() {
        atomic(|| {
            lock(&MEMBERS).clear();
            lock(&LIFE).clear();
            LIFE_ON.store(true, Ordering::Release);
        });
    }

    /// Stop recording and return the events in their recorded order (a
    /// linearization: each event is recorded in the critical section of the
    /// state change it describes).
    pub fn take_lifecycle() -> Vec<super::LifeEvent> {
        atomic(|| {
            LIFE_ON.store(false, Ordering::Release);
            lock(&MEMBERS).clear();
            std::mem::take(&mut *lock(&LIFE))
        })
    }

    /// Whether actor `id` is recorded.
    pub(crate) fn life_traced(id: u64) -> bool {
        LIFE_ON.load(Ordering::Acquire) && lock(&MEMBERS).contains(&id)
    }

    /// Run `f` as one atomic step of the recorded linearization if actor
    /// `id` is recorded (and as is otherwise). Every lifecycle section is
    /// keyed by the actor whose locks it takes first, and nested sections use
    /// the same key, so the section is always the outermost lock.
    pub(crate) fn life_section<R>(id: u64, f: impl FnOnce() -> R) -> R {
        if life_traced(id) {
            atomic(f)
        } else {
            f()
        }
    }

    /// Record an event of actor `t` (inside its section).
    pub(crate) fn life_emit(t: u64, event: impl FnOnce() -> super::LifeEvent) {
        if life_traced(t) {
            lock(&LIFE).push(event());
        }
    }

    /// A new actor instance (after its `start_linked` link, before
    /// `started()`): recorded from now on.
    pub(crate) fn life_spawn(id: u64, parent: u64) {
        if !LIFE_ON.load(Ordering::Acquire) {
            return;
        }
        atomic(|| {
            lock(&MEMBERS).insert(id);
            lock(&LIFE).push(super::LifeEvent::new("Spawn", id).s(parent));
        });
    }

    /// The model's mailbox item kind of a message of type `M`.
    pub(crate) fn life_kind<M: 'static>() -> &'static str {
        if std::any::TypeId::of::<M>() == std::any::TypeId::of::<crate::monitor::Down>() {
            "down"
        } else {
            "req"
        }
    }

    /// A send or request to `owner` was admitted or refused (`NoProc`). A
    /// `Down` is recorded by its monitor as a notice delivery instead. `from`
    /// is the recorded actor that sent it ([`life_sender`]); an admitted
    /// send of one is the model's actor-to-actor `Send`, anything else a
    /// request from `Ext`.
    pub(crate) fn life_send<M: 'static>(owner: u64, admitted: bool, from: u64) {
        if life_kind::<M>() == "down" {
            return;
        }
        life_emit(owner, || {
            super::LifeEvent::new("Send", owner)
                .s(if admitted { from } else { 0 })
                .x(if admitted { "queued" } else { "noproc" })
        });
    }

    /// The recorded actor whose turn (or hook) is sending a plain message
    /// (not a request, not a `Down`) to another actor `owner`; 0 otherwise.
    pub(crate) fn life_sender<M: 'static>(owner: u64, is_send: bool) -> u64 {
        if !is_send || life_kind::<M>() == "down" {
            return 0;
        }
        let current = CURRENT.with(Cell::get);
        if current != 0 && current != owner && life_traced(current) {
            current
        } else {
            0
        }
    }

    /// Run `f` as part of actor `id`'s turn or hook (threads mode).
    pub(crate) fn with_actor<R>(id: u64, f: impl FnOnce() -> R) -> R {
        struct Restore(u64);
        impl Drop for Restore {
            fn drop(&mut self) {
                CURRENT.with(|c| c.set(self.0));
            }
        }
        let _restore = Restore(CURRENT.with(|c| c.replace(id)));
        f()
    }

    type Turn<'a> = std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>>;

    /// Poll a turn or hook future as part of actor `id` (tasks mode).
    pub(crate) fn in_actor(id: u64, turn: Turn<'_>) -> Turn<'_> {
        Box::pin(InActor { id, turn })
    }
    struct InActor<'a> {
        id: u64,
        turn: Turn<'a>,
    }
    impl std::future::Future for InActor<'_> {
        type Output = ();
        fn poll(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<()> {
            let id = self.id;
            with_actor(id, || self.turn.as_mut().poll(cx))
        }
    }

    thread_local! {
        /// The recorded actor whose turn this thread is running.
        static CURRENT: Cell<u64> = const { Cell::new(0) };
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

    /// Whether a supervisor's trace already holds an event named `action`
    /// (lets a test wait for an ordering point without taking the trace).
    pub fn recorded(id: u64, action: &str) -> bool {
        lock(&TRACES)
            .get(&id)
            .is_some_and(|events| events.iter().any(|e| e.action == action))
    }

    /// What a dumped trace needs besides its events: the constants
    /// `trace_check.py` instantiates the model with.
    struct Meta {
        origin: String,
        strategy: &'static str,
        max_restarts: usize,
        modes: Vec<&'static str>,
    }
    static META: Mutex<BTreeMap<u64, Meta>> = Mutex::new(BTreeMap::new());

    /// Remember a new supervisor's strategy and budget, and the thread
    /// (usually the test) that built it, for [`dump_all`].
    pub(crate) fn describe(id: TraceId, strategy: &'static str, max_restarts: usize) {
        let origin = std::thread::current()
            .name()
            .unwrap_or("unnamed")
            .to_string();
        lock(&META).insert(
            id.0,
            Meta {
                origin,
                strategy,
                max_restarts,
                modes: Vec::new(),
            },
        );
    }

    /// Remember the restart mode of the next declared child.
    pub(crate) fn describe_child(id: TraceId, mode: &'static str) {
        if let Some(meta) = lock(&META).get_mut(&id.0) {
            meta.modes.push(mode);
        }
    }

    const COHORT_EVENTS: &[&str] = &[
        "CRequest",
        "CSelect",
        "CAssign",
        "CStop",
        "CClean",
        "CResume",
        "CBegin",
        "CReady",
        "CJoined",
        "CRetire",
        "CAck",
        "CEnd",
        "CInitialReady",
        "CClose",
        "CDone",
    ];

    fn sanitize(name: &str) -> String {
        name.chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect()
    }

    /// If `LIFECYCLE_TRACE_ALL` names a directory, write a copy of this
    /// finished supervisor's policy tape (and, for a root, its ownership
    /// tree) there, in the formats of the dedicated trace tests. Lets any
    /// test binary built with the feature contribute traces to
    /// `verification/tla/trace_coverage.py` without changing the test; the
    /// events stay buffered, so `take`/`take_tree` still see them.
    pub(crate) fn dump_all(id: TraceId) {
        let meta = lock(&META).remove(&id.0);
        let Some(directory) = std::env::var_os("LIFECYCLE_TRACE_ALL") else {
            return;
        };
        let directory = std::path::PathBuf::from(directory);
        let (events, tree, meta) = atomic(|| {
            let events = lock(&TRACES).get(&id.0).cloned().unwrap_or_default();
            let trees = lock(&TREES);
            let attached = trees
                .iter()
                .any(|e| e.action == "TAttach" && e.peer == id.0);
            let mut tree = Vec::new();
            if !attached {
                let mut nodes = std::collections::BTreeSet::from([id.0]);
                loop {
                    let before = nodes.len();
                    for event in trees.iter() {
                        if event.action == "TAttach" && nodes.contains(&event.node) {
                            nodes.insert(event.peer);
                        }
                    }
                    if nodes.len() == before {
                        break;
                    }
                }
                tree = trees
                    .iter()
                    .filter(|e| nodes.contains(&e.node))
                    .cloned()
                    .collect();
            }
            (events, tree, meta)
        });
        let Some(meta) = meta else { return };
        let exe = std::env::current_exe()
            .ok()
            .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
            .unwrap_or_default();
        let name = format!(
            "{}-{}-{}-{}",
            sanitize(&exe),
            sanitize(&meta.origin),
            std::process::id(),
            id.0
        );
        let modes = meta
            .modes
            .iter()
            .map(|m| format!("\"{m}\""))
            .collect::<Vec<_>>()
            .join(",");
        let json =
            |events: &mut dyn Iterator<Item = String>| events.collect::<Vec<_>>().join(",\n");
        let policy = if meta.strategy == "OneForOne" {
            format!(
                "{{\"name\":\"{name}\",\"origin\":\"{}\",\"n\":{},\"modes\":[{modes}],\"max_restarts\":{},\"events\":[\n{}\n]}}\n",
                sanitize(&meta.origin),
                meta.modes.len(),
                meta.max_restarts,
                json(&mut events.iter().map(Event::to_json))
            )
        } else {
            format!(
                "{{\"schema\":\"cohort-v1\",\"name\":\"{name}\",\"origin\":\"{}\",\"strategy\":\"{}\",\"n\":{},\"modes\":[{modes}],\"events\":[\n{}\n]}}\n",
                sanitize(&meta.origin),
                meta.strategy,
                meta.modes.len(),
                json(&mut events
                    .iter()
                    .filter(|e| COHORT_EVENTS.contains(&e.action))
                    .map(Event::to_json))
            )
        };
        let _ = std::fs::create_dir_all(&directory);
        if !meta.modes.is_empty() {
            let _ = std::fs::write(directory.join(format!("policy-{name}.json")), policy);
        }
        if !tree.is_empty() {
            let tree = format!(
                "{{\"schema\":\"tree-v1\",\"name\":\"{name}\",\"origin\":\"{}\",\"root\":{},\"events\":[\n{}\n]}}\n",
                sanitize(&meta.origin),
                id.0,
                json(&mut tree.iter().map(TreeEvent::to_json))
            );
            let _ = std::fs::write(directory.join(format!("tree-{name}.json")), tree);
        }
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
    #[allow(dead_code)] // recording is compiled out without the feature
    #[inline(always)]
    pub(crate) fn reply(_: u64, _: &'static str, _: u64, _: &'static str) {}
    #[allow(dead_code)] // recording is compiled out without the feature
    #[inline(always)]
    pub(crate) fn job(_: u64, _: impl FnOnce() -> super::JobEvent) {}
    #[allow(dead_code)]
    #[inline(always)]
    pub(crate) fn in_link<R>(_: Option<(u64, u64)>, f: impl FnOnce() -> R) -> R {
        f()
    }
    #[allow(dead_code)]
    #[inline(always)]
    pub(crate) fn life_section<R>(_: u64, f: impl FnOnce() -> R) -> R {
        f()
    }
    #[allow(dead_code)]
    #[inline(always)]
    pub(crate) fn life_emit(_: u64, _: impl FnOnce() -> super::LifeEvent) {}
    #[allow(dead_code)]
    #[inline(always)]
    pub(crate) fn life_spawn(_: u64, _: u64) {}
    #[allow(dead_code)]
    #[inline(always)]
    pub(crate) fn life_send<M: 'static>(_: u64, _: bool, _: u64) {}
    #[allow(dead_code, clippy::extra_unused_type_parameters)]
    #[inline(always)]
    pub(crate) fn life_sender<M: 'static>(_: u64, _: bool) -> u64 {
        0
    }
    #[allow(dead_code)]
    #[inline(always)]
    pub(crate) fn with_actor<R>(_: u64, f: impl FnOnce() -> R) -> R {
        f()
    }
    #[allow(dead_code)]
    #[inline(always)]
    pub(crate) fn in_actor<T>(_: u64, turn: T) -> T {
        turn
    }
}

#[cfg(feature = "lifecycle-trace")]
pub use imp::{
    record_lifecycle, recorded, take, take_jobs, take_lifecycle, take_replies, take_tree,
    watch_deferred,
};
