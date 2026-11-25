//! The JSON form of listings and snapshots (`console` feature;
//! docs/design/introspection.md, "Wire and data contract", schema 1).
//!
//! Hand-written, no serialization crate: dedicated wire structs map the Rust
//! types to the wire vocabulary, so the schema never follows a derived
//! representation, and one writer emits them. The rules:
//!
//! - One line of UTF-8 JSON: control characters are escaped, so a value
//!   never breaks the line framing.
//! - Every `u64` and every count not bounded below 2^32 by construction (ids,
//!   generations, restarts, counters, deferred replies, jobs, durations in
//!   `*_ms`) is a decimal **string**. JSON numbers are kept for small bounded
//!   values only: `schema`, `links`, `monitors`, worker budgets and CPU
//!   indices.
//! - A field that cannot be given is `{"unavailable":"<reason>"}`, with
//!   `"feature":"introspection"` added for `feature`; never `null` or an
//!   omission. `null` means the value does not exist (no label, no system,
//!   not dead).
//! - Enums are snake_case strings; variants with data are objects with a
//!   `kind`.
//! - Labels, names, exit details and errors are cut at 256 bytes (at a
//!   character boundary).

use super::{
    Activity, ActivityKind, ActorInfo, Field, Lifecycle, MailboxCounts, Parent, Placement,
    PollState, SupervisorPosition, Unavailable,
};
use crate::error::ExitReason;
use std::fmt::Write as _;
use std::time::Duration;

/// The longest label, name, exit detail or error string on the wire.
pub(crate) const MAX_STRING: usize = 256;

// ---------------------------------------------------------------------------
// The writer
// ---------------------------------------------------------------------------

/// Appends one JSON value at a time; objects track their own commas.
pub(crate) struct Json {
    out: String,
}

/// One JSON object being written: `key` writes the separator and the key,
/// then the caller writes exactly one value.
pub(crate) struct Obj<'a> {
    json: &'a mut Json,
    first: bool,
}

impl Json {
    pub(crate) fn new() -> Self {
        Self { out: String::new() }
    }
    pub(crate) fn finish(self) -> String {
        self.out
    }
    pub(crate) fn object(&mut self, body: impl FnOnce(&mut Obj<'_>)) {
        self.out.push('{');
        body(&mut Obj {
            json: self,
            first: true,
        });
        self.out.push('}');
    }
    pub(crate) fn array<T>(
        &mut self,
        items: impl IntoIterator<Item = T>,
        mut each: impl FnMut(&mut Json, T),
    ) {
        self.out.push('[');
        for (i, item) in items.into_iter().enumerate() {
            if i > 0 {
                self.out.push(',');
            }
            each(self, item);
        }
        self.out.push(']');
    }
    /// A quoted, escaped string.
    pub(crate) fn str(&mut self, s: &str) {
        self.out.push('"');
        for c in s.chars() {
            match c {
                '"' => self.out.push_str("\\\""),
                '\\' => self.out.push_str("\\\\"),
                '\n' => self.out.push_str("\\n"),
                '\r' => self.out.push_str("\\r"),
                '\t' => self.out.push_str("\\t"),
                '\u{8}' => self.out.push_str("\\b"),
                '\u{c}' => self.out.push_str("\\f"),
                // The other C0 controls, DEL, and the two line separators
                // JavaScript treats as line ends.
                c if (c as u32) < 0x20 || c == '\u{7f}' || c == '\u{2028}' || c == '\u{2029}' => {
                    let _ = write!(self.out, "\\u{:04x}", c as u32);
                }
                c => self.out.push(c),
            }
        }
        self.out.push('"');
    }
    /// A string cut at [`MAX_STRING`] bytes.
    pub(crate) fn short_str(&mut self, s: &str) {
        let mut cut = s.len().min(MAX_STRING);
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        self.str(&s[..cut]);
    }
    /// A `u64` as a decimal string.
    pub(crate) fn u64(&mut self, n: u64) {
        let _ = write!(self.out, "\"{n}\"");
    }
    /// A small bounded number, as a JSON number.
    pub(crate) fn num(&mut self, n: u32) {
        let _ = write!(self.out, "{n}");
    }
    pub(crate) fn bool(&mut self, b: bool) {
        self.out.push_str(if b { "true" } else { "false" });
    }
    pub(crate) fn null(&mut self) {
        self.out.push_str("null");
    }
    fn opt<T>(&mut self, value: Option<T>, some: impl FnOnce(&mut Json, T)) {
        match value {
            Some(value) => some(self, value),
            None => self.null(),
        }
    }
}

impl Obj<'_> {
    /// Write `"key":` and return the writer for its value.
    pub(crate) fn key(&mut self, key: &str) -> &mut Json {
        if !self.first {
            self.json.out.push(',');
        }
        self.first = false;
        self.json.str(key);
        self.json.out.push(':');
        self.json
    }
}

fn usize_str(json: &mut Json, n: usize) {
    json.u64(u64::try_from(n).unwrap_or(u64::MAX));
}

fn count(json: &mut Json, n: usize) {
    json.num(u32::try_from(n).unwrap_or(u32::MAX));
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

// ---------------------------------------------------------------------------
// Wire structs
// ---------------------------------------------------------------------------

/// A [`Field`] on the wire: the value, or why it is missing.
pub(crate) enum WireField<T> {
    Value(T),
    Unavailable(&'static str),
}

fn unavailable(reason: Unavailable) -> &'static str {
    match reason {
        Unavailable::Feature => "feature",
        Unavailable::ReadContended => "read_contended",
        Unavailable::ClockNotStarted => "clock_not_started",
        Unavailable::Redacted => "redacted",
        Unavailable::Dead => "dead",
        Unavailable::Starting => "starting",
    }
}

impl<T> WireField<T> {
    fn of<'u, U>(field: &'u Field<U>, map: impl FnOnce(&'u U) -> T) -> Self {
        match field {
            Field::Value(value) => WireField::Value(map(value)),
            Field::Unavailable(reason) => WireField::Unavailable(unavailable(*reason)),
        }
    }
    fn write(&self, json: &mut Json, value: impl FnOnce(&mut Json, &T)) {
        match self {
            WireField::Value(v) => value(json, v),
            WireField::Unavailable(reason) => json.object(|o| {
                o.key("unavailable").str(reason);
                // The only feature a field can miss today.
                if *reason == "feature" {
                    o.key("feature").str("introspection");
                }
            }),
        }
    }
}

/// An exit reason: its class and, for `panic` and `shutdown`, its text.
pub(crate) struct WireReason<'a> {
    reason: &'static str,
    detail: Option<&'a str>,
}

impl<'a> WireReason<'a> {
    pub(crate) fn of(reason: &'a ExitReason) -> Self {
        let (reason, detail) = match reason {
            ExitReason::Normal => ("normal", None),
            ExitReason::Shutdown(why) => ("shutdown", why.as_deref()),
            ExitReason::Panic(message) => ("panic", Some(message.as_str())),
            ExitReason::Kill => ("kill", None),
            ExitReason::Killed => ("killed", None),
            ExitReason::NoProc => ("noproc", None),
        };
        Self { reason, detail }
    }
    fn fields(&self, o: &mut Obj<'_>) {
        o.key("reason").str(self.reason);
        o.key("detail").opt(self.detail, Json::short_str);
    }
    #[cfg_attr(not(any(test, feature = "async")), allow(dead_code))]
    fn write(&self, json: &mut Json) {
        json.object(|o| self.fields(o));
    }
}

struct WireDead<'a> {
    reason: WireReason<'a>,
    published: bool,
}

struct WireActivity {
    kind: &'static str,
    turn: u64,
    message: Option<&'static str>,
    age_ms: WireField<u64>,
    poll: Option<(&'static str, Option<u64>)>,
    kill_pending: bool,
}

impl WireActivity {
    fn of(activity: &Activity) -> Self {
        Self {
            kind: match activity.kind {
                ActivityKind::Started => "started",
                ActivityKind::MessageTurn => "message_turn",
                ActivityKind::ExitReceived => "exit_received",
                ActivityKind::Status => "status",
                ActivityKind::Stopped => "stopped",
                ActivityKind::Idle => "idle",
            },
            turn: activity.turn.0,
            message: activity.message,
            age_ms: WireField::of(&activity.age, |d| millis(*d)),
            poll: activity.poll.map(|poll| match poll {
                PollState::InPoll { age } => ("in_poll", Some(millis(age))),
                PollState::Suspended => ("suspended", None),
            }),
            kill_pending: activity.kill_pending,
        }
    }
    fn write(&self, json: &mut Json) {
        json.object(|o| {
            o.key("kind").str(self.kind);
            o.key("turn").u64(self.turn);
            o.key("message").opt(self.message, Json::str);
            self.age_ms.write(o.key("age_ms"), |j, ms| j.u64(*ms));
            o.key("poll").opt(self.poll, |j, (state, age)| {
                j.object(|p| {
                    p.key("state").str(state);
                    if let Some(age) = age {
                        p.key("age_ms").u64(age);
                    }
                })
            });
            o.key("kill_pending").bool(self.kill_pending);
        });
    }
}

/// One actor (`ActorInfo`) on the wire.
pub(crate) struct WireActor<'a> {
    id: u64,
    system: Option<u64>,
    actor_type: &'a str,
    label: Option<&'a str>,
    name: Option<&'a str>,
    lifecycle: &'static str,
    dead: Option<WireDead<'a>>,
    trap_exit: bool,
    links: usize,
    monitors: usize,
    parent: (&'static str, Option<u64>),
    supervisor: Option<&'a SupervisorPosition>,
    placement: (&'static str, Option<&'a str>),
    deferred_outstanding: usize,
    jobs_running: usize,
    activity: WireField<WireActivity>,
    mailbox: WireField<MailboxCounts>,
}

impl<'a> WireActor<'a> {
    pub(crate) fn of(info: &'a ActorInfo) -> Self {
        let (lifecycle, dead) = match &info.lifecycle {
            Lifecycle::Starting => ("starting", None),
            Lifecycle::Idle => ("idle", None),
            Lifecycle::InTurn => ("in_turn", None),
            Lifecycle::Exiting => ("exiting", None),
            Lifecycle::Dead { reason, published } => (
                "dead",
                Some(WireDead {
                    reason: WireReason::of(reason),
                    published: *published,
                }),
            ),
        };
        Self {
            id: info.id.raw(),
            system: info.system.map(|s| s.0),
            actor_type: info.actor_type,
            label: info.label.as_deref(),
            name: info.name.as_deref(),
            lifecycle,
            dead,
            trap_exit: info.trap_exit,
            links: info.links,
            monitors: info.monitors,
            parent: match info.parent {
                Parent::None => ("none", None),
                Parent::Actor(id) => ("actor", Some(id.raw())),
                Parent::System => ("system", None),
            },
            supervisor: info.supervisor.as_ref(),
            placement: match &info.placement {
                Placement::Async => ("async", None),
                Placement::Blocking => ("blocking", None),
                Placement::Thread => ("thread", None),
                Placement::Native => ("native", None),
                Placement::Group { name } => ("group", Some(name.as_str())),
            },
            deferred_outstanding: info.deferred_outstanding,
            jobs_running: info.jobs_running,
            activity: WireField::of(&info.activity, WireActivity::of),
            mailbox: WireField::of(&info.mailbox, |m| *m),
        }
    }

    pub(crate) fn write(&self, json: &mut Json) {
        json.object(|o| {
            o.key("id").u64(self.id);
            o.key("system").opt(self.system, Json::u64);
            o.key("type").str(self.actor_type);
            o.key("label").opt(self.label, Json::short_str);
            o.key("name").opt(self.name, Json::short_str);
            o.key("lifecycle").str(self.lifecycle);
            o.key("dead").opt(self.dead.as_ref(), |j, dead| {
                j.object(|d| {
                    dead.reason.fields(d);
                    d.key("published").bool(dead.published);
                })
            });
            o.key("trap_exit").bool(self.trap_exit);
            count(o.key("links"), self.links);
            count(o.key("monitors"), self.monitors);
            o.key("parent").object(|p| {
                p.key("kind").str(self.parent.0);
                if let Some(id) = self.parent.1 {
                    p.key("id").u64(id);
                }
            });
            o.key("supervisor").opt(self.supervisor, |j, position| {
                j.object(|s| {
                    s.key("child").short_str(&position.child);
                    s.key("generation").u64(position.generation);
                    s.key("restarts").u64(position.restarts);
                })
            });
            o.key("placement").object(|p| {
                p.key("kind").str(self.placement.0);
                if let Some(group) = self.placement.1 {
                    p.key("group").short_str(group);
                }
            });
            usize_str(o.key("deferred_outstanding"), self.deferred_outstanding);
            usize_str(o.key("jobs_running"), self.jobs_running);
            self.activity.write(o.key("activity"), |j, a| a.write(j));
            self.mailbox.write(o.key("mailbox"), |j, m| {
                j.object(|c| {
                    c.key("len").u64(m.len);
                    c.key("accepted").u64(m.accepted);
                    c.key("dequeued").u64(m.dequeued);
                    c.key("discarded").u64(m.discarded);
                })
            });
        });
    }
}

// ---------------------------------------------------------------------------
// Systems, groups and supervision trees
// ---------------------------------------------------------------------------

#[cfg(feature = "async")]
mod managed {
    use super::*;
    use crate::introspection::{GroupSnapshot, SystemSnapshot};
    use crate::supervision::{
        ChildInfo, ChildKey, ChildKind, ChildState, RestartMode, SupervisorPhase,
        SupervisorStrategy, SupervisorTree, TreeNode,
    };
    use crate::system::ExecutionProfile;

    pub(crate) fn system(json: &mut Json, system: &SystemSnapshot) {
        json.object(|o| {
            o.key("id").u64(system.id.0);
            o.key("groups").array(&system.groups, group);
            o.key("supervisors").array(&system.supervisors, tree);
        });
    }

    pub(crate) fn group(json: &mut Json, group: &GroupSnapshot) {
        json.object(|o| {
            o.key("name").short_str(&group.name);
            o.key("profile").object(|p| match &group.profile {
                ExecutionProfile::AsyncPool { workers } => {
                    p.key("kind").str("async_pool");
                    count(p.key("workers"), *workers);
                }
                ExecutionProfile::ThreadPool {
                    workers,
                    helper_workers,
                } => {
                    p.key("kind").str("thread_pool");
                    count(p.key("workers"), *workers);
                    count(p.key("helper_workers"), *helper_workers);
                }
                ExecutionProfile::PinnedShards { cpus } => {
                    p.key("kind").str("pinned_shards");
                    p.key("cpus").array(cpus, |j, cpu| count(j, *cpu));
                }
            });
            count(o.key("worker_budget"), group.worker_budget);
            o.key("closing").bool(group.closing);
            WireField::of(&group.busy_workers, |n| *n)
                .write(o.key("busy_workers"), |j, n| usize_str(j, *n));
            WireField::of(&group.scheduling_delay, |d| millis(*d))
                .write(o.key("scheduling_delay_ms"), |j, ms| j.u64(*ms));
            usize_str(o.key("helpers_draining"), group.helpers_draining);
            o.key("actors")
                .array(&group.actors, |j, info| WireActor::of(info).write(j));
        });
    }

    fn strategy(s: SupervisorStrategy) -> &'static str {
        match s {
            SupervisorStrategy::OneForOne => "one_for_one",
            SupervisorStrategy::OneForAll => "one_for_all",
            SupervisorStrategy::RestForOne => "rest_for_one",
        }
    }

    fn phase(p: SupervisorPhase) -> &'static str {
        match p {
            SupervisorPhase::Starting => "starting",
            SupervisorPhase::Running => "running",
            SupervisorPhase::Stopping => "stopping",
            SupervisorPhase::Stopped => "stopped",
            SupervisorPhase::Failed => "failed",
        }
    }

    fn state(s: ChildState) -> &'static str {
        match s {
            ChildState::Waiting => "waiting",
            ChildState::Starting => "starting",
            ChildState::Running => "running",
            ChildState::Backoff => "backoff",
            ChildState::Stopping => "stopping",
            ChildState::Stopped => "stopped",
            ChildState::Failed => "failed",
        }
    }

    fn restart(r: RestartMode) -> &'static str {
        match r {
            RestartMode::Permanent => "permanent",
            RestartMode::Transient => "transient",
            RestartMode::Temporary => "temporary",
        }
    }

    pub(crate) fn tree(json: &mut Json, tree_: &SupervisorTree) {
        json.object(|o| {
            o.key("system").opt(tree_.system, |j, s| j.u64(s.0));
            o.key("name").short_str(&tree_.name);
            o.key("strategy").str(strategy(tree_.strategy));
            o.key("phase").str(phase(tree_.phase));
            o.key("budget").object(|b| {
                usize_str(b.key("max_restarts"), tree_.budget.max_restarts);
                b.key("window_ms").u64(millis(tree_.budget.window));
                usize_str(b.key("used"), tree_.budget.used);
            });
            o.key("counts").object(|c| {
                usize_str(c.key("specs"), tree_.counts.specs);
                usize_str(c.key("active"), tree_.counts.active);
                usize_str(c.key("supervisors"), tree_.counts.supervisors);
                usize_str(c.key("workers"), tree_.counts.workers);
            });
            o.key("children").array(&tree_.children, |j, child| {
                j.object(|c| {
                    row(c, &child.info);
                    c.key("node").object(|n| match &child.node {
                        TreeNode::Worker(info) => {
                            n.key("kind").str("worker");
                            WireField::of(info, WireActor::of)
                                .write(n.key("info"), |j, a| a.write(j));
                        }
                        TreeNode::Supervisor(sub) => {
                            n.key("kind").str("supervisor");
                            WireField::of(sub, |t| t).write(n.key("tree"), |j, t| tree(j, t));
                        }
                    });
                })
            });
        });
    }

    fn row(c: &mut Obj<'_>, info: &ChildInfo) {
        c.key("key").object(|k| match &info.key {
            ChildKey::Named(name) => {
                k.key("kind").str("named");
                k.key("name").short_str(name);
            }
        });
        c.key("actor").opt(info.actor, |j, id| j.u64(id.raw()));
        c.key("ready").bool(info.ready);
        c.key("state").str(state(info.state));
        c.key("kind").str(match info.kind {
            ChildKind::Worker => "worker",
            ChildKind::Supervisor => "supervisor",
        });
        c.key("restart").str(restart(info.restart));
        c.key("generation").u64(info.generation);
        c.key("restarts").u64(info.restarts);
        c.key("last_exit")
            .opt(info.last_exit.as_ref(), |j, r| WireReason::of(r).write(j));
        c.key("last_error")
            .opt(info.last_error.as_deref(), Json::short_str);
    }
}

// ---------------------------------------------------------------------------
// Public JSON forms
// ---------------------------------------------------------------------------

impl ActorInfo {
    /// This actor in the console's JSON form (schema 1, one line): every
    /// `u64` is a string, missing fields are `{"unavailable":..}`. See
    /// docs/design/introspection.md, "Wire and data contract".
    pub fn to_json(&self) -> String {
        let mut json = Json::new();
        WireActor::of(self).write(&mut json);
        json.finish()
    }
}

impl super::ActorPage {
    /// `{"actors":[..],"next":"<id>"|null}` (schema 1, one line).
    pub fn to_json(&self) -> String {
        let mut json = Json::new();
        json.object(|o| {
            o.key("actors")
                .array(&self.actors, |j, info| WireActor::of(info).write(j));
            o.key("next").opt(self.next, |j, id| j.u64(id.raw()));
        });
        json.finish()
    }
}

impl super::Snapshot {
    /// The whole snapshot as one line of JSON (schema 1):
    /// `{"schema":1,"taken_at_ms":..,"wall_unix_ms":..,"systems":[..],"unmanaged":[..]}`.
    /// `taken_at_ms` counts from the process's clock epoch (its first clock
    /// read), virtual time under the simulator.
    pub fn to_json(&self) -> String {
        let mut json = Json::new();
        json.object(|o| {
            o.key("schema").num(self.schema);
            o.key("taken_at_ms")
                .u64(spawned_rt::time::to_ticks(self.taken_at) / 1_000_000);
            o.key("wall_unix_ms").u64(
                self.wall_clock
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, millis),
            );
            #[cfg(feature = "async")]
            o.key("systems").array(&self.systems, managed::system);
            #[cfg(not(feature = "async"))]
            o.key("systems").array(std::iter::empty::<()>(), |_, ()| {});
            o.key("unmanaged")
                .array(&self.unmanaged, |j, info| WireActor::of(info).write(j));
        });
        json.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::child_handle::ActorId;
    use crate::introspection::{SystemId, TurnId};
    use std::sync::Arc;

    fn info() -> ActorInfo {
        ActorInfo {
            id: ActorId::from_raw_for_test(7),
            system: None,
            actor_type: "app::Peer",
            label: None,
            name: None,
            lifecycle: Lifecycle::Idle,
            trap_exit: false,
            links: 0,
            monitors: 0,
            parent: Parent::None,
            supervisor: None,
            placement: Placement::Async,
            deferred_outstanding: 0,
            jobs_running: 0,
            activity: Field::Unavailable(Unavailable::Feature),
            mailbox: Field::Unavailable(Unavailable::Feature),
        }
    }

    fn string(s: &str) -> String {
        let mut json = Json::new();
        json.str(s);
        json.finish()
    }

    #[test]
    fn strings_are_escaped_onto_one_line() {
        assert_eq!(string(r#"a"b\c"#), r#""a\"b\\c""#);
        assert_eq!(string("l1\nl2\r\t"), r#""l1\nl2\r\t""#);
        assert_eq!(
            string("\u{0}\u{1f}\u{7f}\u{8}\u{c}"),
            r#""\u0000\u001f\u007f\b\f""#
        );
        assert_eq!(string("\u{2028}\u{2029}"), "\"\\u2028\\u2029\"");
        // Other non-ASCII text passes through as UTF-8.
        assert_eq!(string("pär €"), "\"pär €\"");
    }

    #[test]
    fn short_strings_are_cut_at_a_char_boundary() {
        // '€' is three bytes: 256 falls inside the 86th.
        let mut json = Json::new();
        json.short_str(&"€".repeat(100));
        let out = json.finish();
        assert_eq!(out, format!("\"{}\"", "€".repeat(85)));
    }

    #[test]
    fn a_bare_idle_actor() {
        assert_eq!(
            info().to_json(),
            concat!(
                r#"{"id":"7","system":null,"type":"app::Peer","label":null,"name":null,"#,
                r#""lifecycle":"idle","dead":null,"trap_exit":false,"links":0,"monitors":0,"#,
                r#""parent":{"kind":"none"},"supervisor":null,"placement":{"kind":"async"},"#,
                r#""deferred_outstanding":"0","jobs_running":"0","#,
                r#""activity":{"unavailable":"feature","feature":"introspection"},"#,
                r#""mailbox":{"unavailable":"feature","feature":"introspection"}}"#
            )
        );
    }

    #[test]
    fn every_u64_is_a_string_whatever_its_value() {
        let mut info = info();
        info.id = ActorId::from_raw_for_test(u64::MAX);
        info.system = Some(SystemId(u64::MAX));
        info.parent = Parent::Actor(ActorId::from_raw_for_test(3));
        info.supervisor = Some(SupervisorPosition {
            child: "w".into(),
            generation: u64::MAX,
            restarts: 1 << 53,
        });
        info.deferred_outstanding = 5;
        info.links = 2;
        let json = info.to_json();
        assert!(
            json.starts_with(r#"{"id":"18446744073709551615","system":"18446744073709551615""#),
            "{json}"
        );
        assert!(
            json.contains(r#""parent":{"kind":"actor","id":"3"}"#),
            "{json}"
        );
        assert!(
            json.contains(r#""supervisor":{"child":"w","generation":"18446744073709551615","restarts":"9007199254740992"}"#),
            "{json}"
        );
        assert!(json.contains(r#""links":2,"#), "{json}");
        assert!(json.contains(r#""deferred_outstanding":"5""#), "{json}");
    }

    #[test]
    fn dead_labelled_grouped_actor() {
        let mut info = info();
        info.label = Some(Arc::from("peer \"x\"\n"));
        info.name = Some("n".into());
        info.placement = Placement::Group { name: "p2p".into() };
        info.parent = Parent::System;
        info.lifecycle = Lifecycle::Dead {
            reason: ExitReason::Panic("boom\nline".into()),
            published: false,
        };
        let json = info.to_json();
        assert!(
            json.contains(r#""label":"peer \"x\"\n","name":"n""#),
            "{json}"
        );
        assert!(
            json.contains(r#""lifecycle":"dead","dead":{"reason":"panic","detail":"boom\nline","published":false}"#),
            "{json}"
        );
        assert!(json.contains(r#""parent":{"kind":"system"}"#), "{json}");
        assert!(
            json.contains(r#""placement":{"kind":"group","group":"p2p"}"#),
            "{json}"
        );
        assert!(!json.contains('\n'));
    }

    #[test]
    fn unavailable_reasons_and_tier_one_values() {
        let mut info = info();
        info.activity = Field::Value(Activity {
            kind: ActivityKind::MessageTurn,
            turn: TurnId(1 << 60),
            message: Some("app::Ping"),
            age: Field::Unavailable(Unavailable::ClockNotStarted),
            poll: Some(PollState::InPoll {
                age: Duration::from_millis(12),
            }),
            kill_pending: true,
        });
        info.mailbox = Field::Value(MailboxCounts {
            len: 1,
            accepted: 2,
            dequeued: 1,
            discarded: 0,
        });
        let json = info.to_json();
        assert!(
            json.contains(concat!(
                r#""activity":{"kind":"message_turn","turn":"1152921504606846976","message":"app::Ping","#,
                r#""age_ms":{"unavailable":"clock_not_started"},"poll":{"state":"in_poll","age_ms":"12"},"#,
                r#""kill_pending":true}"#
            )),
            "{json}"
        );
        assert!(
            json.contains(r#""mailbox":{"len":"1","accepted":"2","dequeued":"1","discarded":"0"}"#),
            "{json}"
        );
        for (reason, wire) in [
            (
                Unavailable::ReadContended,
                r#"{"unavailable":"read_contended"}"#,
            ),
            (Unavailable::Redacted, r#"{"unavailable":"redacted"}"#),
            (Unavailable::Dead, r#"{"unavailable":"dead"}"#),
            (Unavailable::Starting, r#"{"unavailable":"starting"}"#),
        ] {
            info.mailbox = Field::Unavailable(reason);
            assert!(info.to_json().ends_with(&format!(r#""mailbox":{wire}}}"#)));
        }
    }

    #[test]
    fn exit_reasons() {
        let cases = [
            (ExitReason::Normal, r#"{"reason":"normal","detail":null}"#),
            (
                ExitReason::Shutdown(None),
                r#"{"reason":"shutdown","detail":null}"#,
            ),
            (
                ExitReason::shutdown_with("peer gone"),
                r#"{"reason":"shutdown","detail":"peer gone"}"#,
            ),
            (ExitReason::Kill, r#"{"reason":"kill","detail":null}"#),
            (ExitReason::Killed, r#"{"reason":"killed","detail":null}"#),
            (ExitReason::NoProc, r#"{"reason":"noproc","detail":null}"#),
        ];
        for (reason, wire) in cases {
            let mut json = Json::new();
            WireReason::of(&reason).write(&mut json);
            assert_eq!(json.finish(), wire);
        }
    }

    #[test]
    fn pages_end_with_null() {
        let page = crate::introspection::ActorPage {
            actors: vec![],
            next: None,
        };
        assert_eq!(page.to_json(), r#"{"actors":[],"next":null}"#);
        let page = crate::introspection::ActorPage {
            actors: vec![info()],
            next: Some(ActorId::from_raw_for_test(7)),
        };
        assert!(page.to_json().ends_with(r#"}],"next":"7"}"#));
    }
}
