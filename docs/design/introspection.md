# Introspection v1 (0.9 design, revision 3, in review)

Status: design only, not implemented. Revisions 2 and 3 answer two rounds of
independent reviews (Codex and agy, each round "approve after the listed
changes"; no P0 remained after round one); see
[Review resolutions](#review-resolutions). Priority tier 2 of the
[roadmap](../../ROADMAP.md#priorities): ranks 8 (stall watchdog), 30
(read-only inspection, `spawned-top`), 29 (metrics) and gap G23 (remote
console), with 9 (flight recorder) and 28 (causal tracing) placed explicitly.
Builds on 0.8 (PR #239 deferred replies, PR #240 OTP exit signals): it reads
the reply registry's turn phase, the exit request, the death point D and
`ExitReason::Killed`, and changes none of them. Model:
[`Introspection.tla`](../../verification/tla/Introspection.tla).

The question this design must answer in minutes on a live node:

> Which actor is stuck, on what message, for how long, with how much queued,
> and what is it linked to?

Prior art: [observability](../research/observability.md),
[deadlock and stall detection](../research/deadlock-and-stall-detection.md),
and the OTP/Elixir/Gleam comparison done for 0.9 (gaps G3, G4, G5, G9, G23 and
its section 3.3).

## Contents

1. [Decisions at a glance](#decisions-at-a-glance)
2. [Tiers and the opt-in mechanism](#tiers-and-the-opt-in-mechanism)
3. [API](#api)
4. [How the fields are kept](#how-the-fields-are-kept)
5. [The activity word and the kill protocol](#the-activity-word-and-the-kill-protocol)
6. [Cost table](#cost-table)
7. [Consistency guarantees and non-guarantees](#consistency-guarantees-and-non-guarantees)
8. [The stall watchdog](#the-stall-watchdog)
9. [Determinism and the simulator](#determinism-and-the-simulator)
10. [Access paths](#access-paths)
11. [Remote console (`spawned-ctl`)](#remote-console-spawned-ctl)
12. [Wire and data contract](#wire-and-data-contract)
13. [OTP alignment](#otp-alignment)
14. [Alternatives considered](#alternatives-considered)
15. [Semantics contract: guarantees added](#semantics-contract-guarantees-added)
16. [TLA+ model and trace validation](#tla-model-and-trace-validation)
17. [Test plan](#test-plan)
18. [Client mapping](#client-mapping)
19. [Coordination with other 0.9 work](#coordination-with-other-09-work)
20. [Implementation order](#implementation-order)
21. [Review resolutions](#review-resolutions)
22. [Open questions for reviewers](#open-questions-for-reviewers)

## Decisions at a glance

| # | Decision | Why |
|---|---|---|
| 1 | **Two kinds of access.** `info()` (OTP `process_info`) is out of band: it reads shared atomics and tables and never waits for a turn, so it answers while a handler is wedged. `get_state`/`get_status` (OTP `sys`) are in band: a turn of their own, between handler turns, in mailbox order, which times out on a stuck actor, as in OTP. | The handler future holds `&mut A` across awaits; the stuck case cannot be answered from inside the stuck turn. |
| 2 | **Tier 0 is strictly free**: identity, type, label, lifecycle, links, monitors, `trap_exit`, parent, supervisor position, placement, deferred replies and jobs, supervision trees, structured crash and supervisor reports. It reads state that exists (0.8's turn phase, exit request, final reason) or is written at spawn, admission or by explicit calls. **No turn ids in tier 0**: 0.8's Loom-verified turn-phase word is not widened. | The strict no-cost rule: nothing added on send, dispatch or turn, not even arithmetic. |
| 3 | **Tier 1 is the `introspection` feature**: the activity word (turn id, kind, kill bit), message type, turn and poll ages, mailbox counters, the watchdog and console `trace`. Compiled out without it. | The fields that diagnose a stall need stores per turn and an RMW per send. |
| 4 | **The coarse clock is for display only and can say "unknown".** Turns stamp a process-wide millisecond clock (one relaxed load); stamp 0 means "before the clock started" and is reported as unknown, never as a large age. **Kill eligibility never uses the stamp**: the watchdog measures how long it has itself seen the same open turn id, with its own monotonic clock. | A delayed or late-started clock thread must not produce false ages, and a false age must never kill. |
| 5 | **The watchdog is an OS thread**, started at run time, never a Tokio task, with sampling separated from report delivery. Four detectors: a poll blocking a shared async worker (OTP `long_schedule`), a long turn, a starved execution group (Akka probe), a long mailbox (OTP 26 `long_message_queue`). | It must keep working when every runtime worker is occupied (ethrex `d958dcba4`, `535647e45`) and when the log sink blocks. |
| 6 | **Policy: report by default; kill opt-in and fenced.** A watchdog kill is one CAS on the actor's activity word naming the observed open turn; a failed CAS does nothing. Every way an activity ends (return, handler panic, callback panic, hook end, runtime drop) consumes the kill bit, so a committed kill always yields `Killed`; D closes the word for good. L3 is unchanged: the kill takes effect when the activity returns, and a `stopped()` already running finishes. | OTP-aligned reason; no false kills; checked by 20 TLA+ negative controls. |
| 7 | Turn abort at an await is not in v1 (0.10 at the earliest, its own model). | Per-suspended-turn cost, a change to L3, and the comparison's "no `brutal_kill` emulation" rule. |
| 8 | **Flight recorder (rank 9) deferred to 0.10.** v1 ships the bounded multi-session event sink and a minimal, bounded console turn trace (tier 1). | The current turn, the crash breadcrumb and the watchdog cover the incidents; the recorder and full `sys:trace` share one ring, designed once with rank 5c. |
| 9 | **Typed `get_state(f)` on every actor; type-erased state only through `Inspect`, redacted by default.** | A typed closure is ordinary code bound by Rust privacy; dumps reach operators and logs. |
| 10 | **`which_children`/`count_children`/`tree()` on every supervisor**, one `ChildInfo`/`ChildCounts` shared with the dynamic supervisor (PR #245). Rows are sampled independently; a tree expands only the captured, instance-bound handles. | Coherent-looking snapshots that the locks cannot deliver would be a false promise. |
| 11 | Labels (`ctx.set_label`, OTP 27 `proc_lib:set_label`). | 10,000 `RLPxConnection`s are readable only by peer. |
| 12 | Structured `tracing` events with stable names and field sets. | Exporters can rely on them (1.0 stability promise). |
| 13 | Out of scope: `sys:replace_state`, `suspend`/`resume`, `change_code`. | They mutate state outside a turn or do not apply to Rust. |
| 14 | `diagnostics` folds into `introspection` (one release as a deprecated alias); `WarnOnBlocking` is removed when detector 1 lands. | One instrumentation point. |
| 15 | **Every `ActorSystem` process is attachable**: a local Unix-socket console, Cargo feature `console` in `default`, read-only by default, separately allowed `kill`, `terminate` and `trace`, no TCP, no eval. It starts automatically only with `ActorSystem::new`; bare-actor programs call `console::start(cfg)`. A refcount binds on 0→1 and unlinks on 1→0. It runs on its own `spawned-console` thread. | `erl -remsh` on every node is what makes BEAM operable; the cost stays off send, dispatch and turn. |

## Tiers and the opt-in mechanism

| Level | How it is enabled | What it adds | Cost when not enabled |
|---|---|---|---|
| Tier 0 | Always | `info()` minus tier 1 fields, labels, supervision trees, `get_state`/`get_status`, crash and supervisor events | Nothing on send, dispatch or turn |
| Console | `console` feature, **in `default`**; `SPAWNED_CONSOLE=off` at run time | The process-wide actor and system tables (so listings cover bare actors), the wire encoder, the listener thread | Feature off: compiled out. Run-time off: tables kept (spawn/exit cost), no thread, no socket |
| Tier 1 | `features = ["introspection"]` | Activity word and slot, interner, coarse clock, mailbox counters, the tables (also without `console`), the watchdog, console `trace` | Compiled out entirely |
| Watchdog | `Watchdog::start(config)` (tier 1) | Sampling thread, delivery thread, probe tasks | Not started: nothing runs |
| Per actor type | Associated consts set by `#[actor(...)]` | `FORMAT_STATUS` (state formatter), `STALL` (limits and kill policy) | Constants |
| Per ChildSpec / group | `ChildSpec::stall(..)`, `async_group(..).stall(..)` | Overrides of `STALL` | Read only by the watchdog |

Precedence for stall limits: ChildSpec, execution group, actor type, watchdog
defaults.

**Which console answers need which feature:**

| Command | Default build (`console`) | Needs `introspection` |
|---|---|---|
| `ps`, `info`, `links`, `tree`, `groups`, `snapshot` | id, type, label, name, lifecycle (`Starting`/`Idle`/`InTurn`/`Exiting`/`Dead`), links, monitors, trap_exit, parent, supervisor position, placement, deferred, jobs | message type, turn id, turn age, poll age, mailbox length, `--sort mailbox\|turn_age\|poll_age` |
| `state` | yes (in band) | — |
| `stuck` | answers `{"unavailable":"feature","feature":"introspection"}` with the list of actors currently `InTurn` as a hint | turn ages, watchdog reports |
| `trace` | `unavailable: feature` | yes, if allowed |
| `kill`, `terminate` | yes, if allowed | — |

The console states the missing feature in every reply that omits a field
(`"mailbox_len":{"unavailable":"feature"}`), and `hello` lists the compiled
features.

## API

Tasks and threads expose the same names. Reads return immediately. In-band
calls return `Response<T>` (tasks: `.await`; threads: blocking). Types live in
`spawned_concurrency::introspection`, shared by both modes. Every public
struct and enum in this section is `#[non_exhaustive]`.

### Per-actor info (`process_info`)

```rust
impl<A: Actor> ActorRef<A> { pub fn info(&self) -> ActorInfo; }
impl<A: Actor> Context<A>  { pub fn info(&self) -> ActorInfo; pub fn set_label(&self, label: impl Into<Arc<str>>); }
impl ChildHandle {
    pub fn info(&self) -> ActorInfo;
    pub fn links(&self) -> Vec<ActorId>;
    pub fn monitors(&self) -> Vec<ActorId>;   // actors this actor monitors (OTP `monitors`)
}
#[cfg(any(feature = "console", feature = "introspection"))]
pub fn introspection::info(id: ActorId) -> Option<ActorInfo>;   // None once published (OTP: undefined)

pub struct ActorInfo {
    pub id: ActorId,
    pub system: Option<SystemId>,
    pub actor_type: &'static str,          // type_name::<A>()   (OTP initial_call)
    pub label: Option<Arc<str>>,           // OTP 27 set_label
    pub name: Option<String>,              // registry name only (OTP registered_name)
    pub lifecycle: Lifecycle,
    pub trap_exit: bool,
    pub links: usize,
    pub monitors: usize,
    pub parent: Parent,
    pub supervisor: Option<SupervisorPosition>,   // ChildSpec key, generation, restarts
    pub placement: Placement,
    pub deferred_outstanding: usize,
    pub jobs_running: usize,
    // Tier 1
    pub activity: Field<Activity>,
    pub mailbox: Field<MailboxCounts>,
}

pub enum Field<T> { Value(T), Unavailable(Unavailable) }
pub enum Unavailable { Feature, ReadContended, ClockNotStarted, Redacted, Dead }

pub enum Lifecycle {
    Starting,                              // exit request phase 0: started() running
    Idle,                                  // 0.8 turn phase Running
    InTurn,                                // 0.8 turn phase InTurn / InTurnPending (handler turns)
    Exiting,                               // exit began; stopped() may be running
    Dead { reason: ExitReason, published: bool },  // past D; `published` once wait_exit() is ready
}
pub struct Activity {                      // tier 1
    pub kind: ActivityKind,                // Started | Message | ExitReceived | Status | Stopped | Idle
    pub turn: TurnId,                      // per-actor id of the current or last activity
    pub message: Option<&'static str>,     // the last message type handled or being handled
    pub age: Field<Duration>,              // stamp age; Unavailable(ClockNotStarted) for stamp 0
    pub poll: Option<PollState>,           // async only: InPoll { age } or Suspended
    pub kill_pending: bool,                // a committed kill waits for this activity to return
}
pub struct MailboxCounts { pub len: u64, pub accepted: u64, pub dequeued: u64, pub discarded: u64 }
```

Tier 0 `Lifecycle` reads 0.8 state only, so it is exact for handler turns and
does not distinguish a turn of `exit_received` from a message turn; `started()`
is `Starting` and `stopped()` is `Exiting`. Tier 1 `Activity.kind` refines
both.

`ActorInfo.name` is the first `registry` name only (bare `registry::register`
gains the `ActorId` so bare actors are covered); a ChildSpec's name is
`supervisor.child`. `monitors` counts outgoing monitors; the monitor table
stores the target `ActorId` at `ctx.monitor` (a monitor-call cost, not a
message cost). `monitored_by` is deferred.

### State and status (`sys:get_state`, `sys:get_status`)

```rust
impl<A: Actor> ActorRef<A> {
    pub fn get_state<R, F>(&self, f: F) -> Response<R>      // default timeout 5 s
    where F: FnOnce(&A) -> R + Send + 'static, R: Send + 'static;
    pub fn get_status(&self) -> Response<StatusReport>;
}
// with_request_timeout(d).get_state(f) / .get_status() for other timeouts.
impl ChildHandle {
    pub fn get_status(&self) -> Response<StatusReport>;                       // 5 s
    pub fn get_status_with_timeout(&self, timeout: Duration) -> Response<StatusReport>;
}
pub struct StatusReport { pub info: ActorInfo, pub state: StateView }   // Redacted | Formatted(String) | Truncated(String)

pub trait Inspect: Actor { fn format_status(&self) -> String; }      // OTP 25 format_status/1
// #[actor(inspect)] or #[actor(inspect = debug)] sets
// `const FORMAT_STATUS: Option<fn(&Self) -> String>` on Actor (default None).
```

The status turn is a dedicated `StatusEnvelope`, not 0.8's
`InTurnEnvelope` (which deliberately treats a closure panic as a handler
panic). Rules:

1. **A turn of its own.** It opens activity kind `Status` with a new turn id
   (tier 1), so a blocking callback is visible to `info()` and the watchdog.
   It queues behind the backlog (M2) and runs between turns (M1).
2. **`StatusReport.info`** is sampled at the start of the status turn and
   reports the actor as it was between turns: lifecycle `Idle`, the activity
   of the previous turn.
3. **Reply slot.** It uses a 0.8 reply slot with the caller's send-time
   deadline (R3), answered immediately in the status turn; it never defers and
   takes no `MAX_OUTSTANDING` credit. Outcomes: `Ok`, `RequestTimeout`,
   `ActorStopped(final reason)` (queued at D), `NoProc` (sent after D), or the
   new `ActorError::CallbackFailed(String)`. `metrics` match arms gain the
   variant.
4. **Callback panic.** The envelope runs the callback under its own
   `catch_unwind`, answers `CallbackFailed`, disposes of the payload with
   `dispose_panic`, and returns normally, so the turn ends as a returning turn
   (`end_turn(false)`). A caught callback panic alone does not end the actor.
   There is no rollback: effects the callback made through interior
   mutability (a mutex it poisoned, a message it sent, `ctx.stop()` through a
   captured handle, a `ReplyTo` it took out of a `RefCell` and dropped, which
   is answered `ReplyDropped` as in any returning turn) remain.
5. **Skipped when abandoned.** Inside the status turn, before running the
   callback, the envelope checks its reply slot. This admission check is its
   own linearization point, after the turn is published and separate from it.
   If the slot is closed (the deadline passed or the receiver was dropped; the
   0.8 slot's own arbitration decides), the callback is not run and the turn
   ends; a slot that closes after admission lets the callback finish, and its
   answer is dropped (`Delivery::Expired`). This is an explicit,
   documented exception to R4 for status envelopes only: it changes side
   effects of a callback that should have none. A callback already running
   is not interrupted; one that blocks blocks the actor (as any turn) and the
   watchdog reports it as kind `Status`.
6. **Accounting.** Status envelopes bypass the `metrics` mailbox and request
   accounting on both enqueue and dequeue (no `mailbox_depth` leak, not a
   handled message), and are counted in introspection's `MailboxCounts`
   (every envelope). A status request admitted during `stopped()` queues and
   is answered `ActorStopped` at D; one sent during destruction is after D and
   gets `NoProc`.
7. **Output bound.** `format_status` output over 64 KiB is cut to
   `Truncated`.

### Listing and snapshots

```rust
// Tier 0: managed actors, from the groups' scopes.
impl ActorSystem    { pub fn id(&self) -> SystemId; pub fn actors(&self) -> Vec<ActorInfo>; pub fn snapshot(&self) -> SystemSnapshot; }
impl ExecutionGroup { pub fn actors(&self) -> Vec<ActorInfo>; pub fn snapshot(&self) -> Option<GroupSnapshot>; }
// With `console` (default) or `introspection`: every actor, managed or bare.
pub fn introspection::processes() -> Vec<ActorId>;           // sorted by id
pub fn introspection::snapshot() -> Snapshot;

pub struct Snapshot { pub schema: u32, pub taken_at: Instant, pub wall_clock: SystemTime,
                      pub systems: Vec<SystemSnapshot>, pub unmanaged: Vec<ActorInfo>,
                      pub watchdog: Option<WatchdogState> }
pub struct SystemSnapshot { pub id: SystemId, pub groups: Vec<GroupSnapshot>, pub supervisors: Vec<SupervisorTree> }
pub struct GroupSnapshot { pub name: String, pub profile: ExecutionProfile, pub worker_budget: usize,
                           pub closing: bool, pub actors: Vec<ActorInfo>,
                           pub busy_workers: Field<usize>, pub scheduling_delay: Field<Duration>,
                           pub helpers_draining: usize }
impl Snapshot {
    pub fn top(&self, by: Rank, n: usize) -> Vec<&ActorInfo>;                          // recon proc_count
    pub fn window(&self, earlier: &Snapshot, by: Rank, n: usize) -> Vec<(ActorId, u64)>; // recon proc_window
}
```

Listings and the actor table hold an actor from spawn until **publication**
(not D): between D and publication it appears as
`Dead { published: false }`, which is how a destructor hanging after D is
seen. `ExecutionGroup::actors()` skips scopes not yet attached and scopes
whose actor is already published, even while owned helpers keep the scope
alive (revision 3; those helpers appear in `GroupSnapshot.helpers_draining`, a
count). Publication means `wait_exit()` has become ready (not that a caller
has returned from it). `is_alive()` is false from D. Every listing is sorted
by `ActorId`.

### Supervision trees

```rust
impl Supervisor {   // DynamicSupervisor (PR #245) gets the same three
    pub fn which_children(&self) -> Vec<ChildInfo>;
    pub fn count_children(&self) -> ChildCounts;
    pub fn tree(&self) -> SupervisorTree;
}
pub struct ChildInfo {     // replaces ChildSnapshot and #245's DynamicChildInfo
    pub key: ChildKey,     // Named(String) | Dynamic(DynamicChildId)
    pub actor: Option<ActorId>, pub ready: bool, pub state: ChildState,
    pub kind: ChildKind, pub restart: RestartMode,
    pub generation: u64, pub restarts: u64,
    pub last_exit: Option<ExitReason>, pub last_error: Option<String>,
}
pub struct ChildCounts { pub specs: usize, pub active: usize, pub supervisors: usize, pub workers: usize }
pub struct SupervisorTree {
    pub system: SystemId, pub name: String, pub strategy: SupervisorStrategy,
    pub phase: SupervisorPhase, pub budget: RestartBudget, pub busy: Option<SupervisorBusy>,
    pub counts: ChildCounts,   // derived from `children` below
    pub children: Vec<TreeChild>,
}
pub enum TreeNode { Worker(Field<ActorInfo>), Supervisor(Field<SupervisorTree>) }
```

`tree()` takes the supervisor's locks only to copy rows and instance-bound
handles (`ChildHandle` with its `ActorId`, or the nested `Supervisor` of that
generation), releases every lock, then expands each captured handle. Because
handles are instance-bound, a node never mixes generation N's row with
generation N+1's actor: if the captured instance has exited, its node shows
`Dead` or `Unavailable(Dead)`. No lock is held across a child's `info()` or a
nested `tree()`, so no lock order exists between supervisors and actors.
`count_children()` and `tree().counts` are derived from the same captured
rows. `SupervisedChild::snapshot()` becomes `info()`; `Supervisor::snapshots()`
stays one release as a deprecated alias.

### Structured reports

Emitted only on exit, supervisor and watchdog paths. Target and field names
are a contract (stable from 1.0).

| Target, level | When | Fields |
|---|---|---|
| `spawned::crash`, ERROR | abnormal final reason, emitted at D | `actor_id`, `system`, `actor_type`, `label`, `name`, `reason`, `ancestors`, `generation`, `group`, `links`; tier 1: `last_message`, `turn`, `mailbox_len` |
| `spawned::supervisor`, INFO/WARN/ERROR | `child_terminated`, `child_restarted`, `start_failed`, `restart_intensity_reached`, `shutdown_error`, `killed_by_console` | `supervisor`, `system`, `child`, `generation`, `reason`, `restarts_in_window`, `max_restarts`, `window_ms`, `action` |
| `spawned::stall`, WARN | a detector fires, or a reported activity ends | `detector`, `actor_id`, `actor_type`, `label`, `kind`, `message`, `turn`, `observed_ms` (watchdog), `stamp_age_ms`, `poll_ms`, `group`, `mailbox_len`, `action` (`reported`, `kill_committed`, `kill_skipped`, `ended`) |
| `spawned::starvation`, WARN | a group's probe is late | `group`, `delay_ms`, `workers`, `busy`, `occupants` |
| `spawned::console`, INFO/WARN | every console command | `peer_pid`, `peer_uid`, `cmd`, `args`, `outcome`, `bytes` |

`last_message` is the activity slot's `tag`, which only message turns write,
so entering `stopped()` does not erase it (no extra store). A stall's end
report gives the observed range (`observed_ms` from the watchdog's first and
last sight, and the stamp age), not an exact total.

### Event sink and sessions

```rust
#[cfg(feature = "introspection")]
pub fn introspection::session(filter: EventFilter, capacity: usize) -> Session;
impl Session { pub fn drain(&mut self) -> (Vec<Event>, /* lost */ u64); }
```

Bounded memory; per-actor order preserved; each session has its own cursor
and filter; a slow session loses its own events, counted, and never blocks a
producer or another session; no allocation on a turn path (producers write
fixed-size records). The 0.10 work (rank 5c monitors, rank 9 recorder, full
`sys:trace`) extends this sink.

## How the fields are kept

### Tier 0

| Field | Kept where | Written |
|---|---|---|
| `actor_type`, backend, system id | `ActorShared` | at spawn |
| `label` | `Mutex<Option<Arc<str>>>` in `ActorShared` | by `set_label` |
| type-erased reads | `SignalTarget` gains `Weak<dyn Probe>` | at spawn |
| `Lifecycle` | 0.8 turn phase (read only), exit request, final reason, publication flag | already |
| links, monitors, `trap_exit`, parent | existing tables; monitor entries gain the target id | at link/monitor calls |
| supervisor position | `Scope` gains child key, generation, restarts | at admission |
| deferred, jobs | 0.8 credit pool (load), job set (its lock) | already |

**Threads-mode parity.** In the reviewed tree, `threads::ActorRef` and
`threads::Context` inline their fields (several `Arc`s) instead of sharing one
`Arc<ActorShared<A>>` as tasks mode does since 0.7.2. Step 0 of the
implementation consolidates threads mode onto `Arc<ActorShared<A>>` (one word
per handle, one reference-count increment per clone), so both modes get the
same fields at the same cost. That step stands on its own and is measured
before any introspection field is added.

### Actor and system tables (`console` or `introspection`)

64 shards keyed by `ActorId & 63`, each a `RwLock<BTreeMap<u64, Arc<dyn
Probe>>>` (ordered, no random hasher). Insert at spawn; remove at
publication. D needs no table write: the probe reads D from the actor's own
state (the 0.8 final reason). Readers take one shard's read lock at a time and copy what they
need; they never hold a lock across a probe call that locks the actor's own
metadata. Systems register in a small ordered list with their `SystemId`.

### Tier 1: the activity slot

One cache-aligned block per actor, written only by its run loop:

```text
word  AtomicU64  turn_id:56 | kill:1 | open:1 | kind:3 | spare:3   (DEAD = kind 7, turn id kept)
tag   AtomicU64  turn_id_low:32 | interned message type:32, written by message and exit_received turns only
since AtomicU64  coarse clock (ms, 0 = clock not started) at activity begin
poll  AtomicU64  poll_count:23 | in_poll:1 | poll_stamp_ms:40     one store before and after each poll
trace AtomicU32  0 = off, else the console session id tracing this actor
```

**Writers.** The run loop is the only writer of `tag`, `since`, `poll` and of
`word` except for the kill bit; the watchdog's CAS is the only other writer of
`word`, and it only sets the kill bit on an open word. `trace` is not in
`word` (revision 3): the console owns it. Enabling is
`trace.compare_exchange(0, session)`, disabling and disconnect are
`compare_exchange(session, 0)`, so a session can only clear its own trace and
two sessions cannot overwrite each other (the second gets `busy`). The run
loop reads `trace` with one relaxed load at each activity end (same cache
line); a change takes effect at the next activity end. The Loom model covers
enable, disable and disconnect racing activity ends.

**Finite identifiers.** Turn ids are 56 bits: at one activity per nanosecond
that is over two years. A run loop that would exceed the maximum stops
publishing new ids (the word stays at the maximum, the watchdog treats the
actor as unobservable and reports `turn_id_exhausted` once); an old id is never
reused. The 23-bit poll count wraps, so detector D1 identifies a poll by
`(turn, poll_count, poll_stamp_ms)`: a wrap that also repeats the millisecond
stamp would need 8 million polls in one millisecond. Loom and unit tests run
the protocol with reduced widths (8-bit turn ids, 4-bit poll counts) to check
both rules.

Senders increment `accepted: AtomicU64` (relaxed `fetch_add`) on their own
cache line after a successful push. The run loop stores `dequeued` at each
dispatch and adds the drained count to `discarded` when it discards the queue
(stop, death). `len = accepted - dequeued - discarded` (saturating); a push
in flight is counted when it succeeds; a failed enqueue (`NoProc`) is not
counted. From D, `len` reads 0. The counters are not frozen at D: a sender that
pushed before D can be descheduled before its `fetch_add` and resume after D
(revision 3). They become final once every send call that began before D has
returned; sends never block, so that is bounded by scheduling, not by the
actor. A failpoint between the push and the increment tests exactly this
interleaving. A fenced admission reservation would give a sharp freeze point
at the cost of a second RMW per send; v1 does not take it.

**Message type without `unsafe`.** Each run loop caches the `type_name::<M>()`
pointer (compared with `ptr::eq` and length) to a `u32` tag from a
process-wide interner. A cache miss (once per message type per actor instance)
takes the interner's mutex and may allocate, on the opted-in dispatch path.

## The activity word and the kill protocol

### Writer (the run loop, single writer)

| Step | Operation | Ordering |
|---|---|---|
| begin activity N | `fence(Release)`; `tag = (N low 32 bits, type)` (message kinds only), `since`, `poll` stores | Relaxed after the fence |
| | `word.store(N, open, kind)` | Release |
| each poll (async) | `poll.store(count, in_poll=1, stamp)`; poll; `poll.store(count, 0, stamp)` | Relaxed (one word, never torn) |
| end activity N (return or panic in `started()`, a handler or `stopped()`; status end) | `old = word.swap(N, closed, kind=Idle)`; `trace.load` | AcqRel; Relaxed |
| | if `old.kill`: record `Killed` in the exit request before anything else (before `stopped()` is decided, before the next receive) | — |
| runtime drop (cleanup reaches D without the loop) | `old = word.swap(DEAD)`; if `old.kill`: `Killed` | AcqRel, before the reason is finalized |
| D | `word.store(turn_id, DEAD)`, keeping the last turn id (already DEAD after a drop) | Release |

The `fence(Release)` at begin orders the new activity's payload stores after
the previous end's swap, so a reader that sees a payload store of activity N+1
also sees the word move past N (the C++ seqlock construction of Boehm, "Can
seqlocks get along with programming language memory models?", 2012).

### Reader (`info()`, watchdog, console)

```text
w1 = word.load(Acquire)
t = tag.load(Relaxed); s = since.load(Relaxed); p = poll.load(Relaxed)
fence(Acquire)
w2 = word.load(Relaxed)
accept iff w1 and w2 are equal ignoring the kill bit
     and t.turn is not ahead of w1.turn (wrapping 32-bit distance, t.turn - w1.turn <= 0)
  open  -> the activity of w1 with t, s, p
  idle  -> Idle; t names the last message turn, t.turn <= w1.turn
  dead  -> Dead (with the last turn id and message)
retry at most 3 times, then Field::Unavailable(ReadContended)
```

**Why the tag carries a turn id (revision 3, round three).** An idle read is
not protected by the word alone. Activity N+1 writes its `tag` before it
publishes `(N+1, open)`, and between the two the word still reads
`(N, closed)`, so a reader can see `w1 == w2 == (N, closed)` together with
N+1's tag (agy, round three). There is no "being written" state to catch it,
and adding one would cost a second word store per activity. Instead, the tag
store carries the low 32 bits of its own turn id, in the same relaxed 64-bit
store, so the cost is unchanged. A tag stamped with a turn ahead of `w1` comes
from an activity that has not been published yet. The reader then retries,
and the retry sees the new word. Because only message turns write `tag`, a
tag behind `w1` is normal (the last message before a status turn or a hook)
and is accepted. Thirty-two bits are enough: a reader is never 2^31
activities behind the writer within one read. The open case needed no change,
because activity N+1's payload stores follow its begin fence, which orders
them after N's end swap, so `w2` has moved. The dead case needs no change
either, because no activity begins after DEAD.

`poll` is one atomic word, so a poll's count, flag and stamp are never mixed;
within one accepted read it belongs to activity N because activity N+1's poll
stores follow the begin fence. The protocol, with every field stored and
loaded separately, is checked by Loom (`activity_slot_loom`, test plan).

### The fenced kill

1. **Eligibility (watchdog only).** The watchdog keeps, per actor, the first
   instant it saw open turn id N (its own monotonic clock). N is eligible when
   it has been seen open for `kill_after`. The stamp never decides.
2. **Commit.** `word.compare_exchange((N, open, kill=0), (N, open, kill=1),
   AcqRel, Acquire)`. On failure (the activity ended, a new one began, the
   word is DEAD or another kill bit is set) nothing happens: the watchdog
   does not call any kill path and reports `kill_skipped`.
3. **Record.** After a successful commit the watchdog records `Killed`
   through the ordinary exit-request path (idempotent; inert after D).
   Whichever of this and the run loop's end swap comes first records it.
4. **Every end consumes the bit**: return, handler panic (a committed kill
   dominates `Panic`, as 0.8's precedence already says), callback panic,
   `started()` and `stopped()` ending, runtime drop. Nothing dispatches after
   the end of a committed activity.
5. **Hooks.** A kill committed during `started()`: when it returns, exit
   begins and `stopped()` is skipped (L5). During `stopped()`: the hook
   already running finishes cooperatively (L3) and the final reason is
   `Killed` (0.8: a kill during `stopped()` supersedes).
6. **D closes the word.** After D the word is DEAD, so a retained probe can
   never commit against a dead actor.

Trace instrumentation (below) records the CAS and the kill record as separate
events, never in one critical section, so the race stays observable.

## Cost table

"Turn" is one dispatched envelope (status included); "activity" adds
`started()`/`stopped()`.

| Mechanism | Tier | Send | Turn | Poll | Spawn / exit | Reader |
|---|---|---|---|---|---|---|
| id, type, backend, system, parent | 0 | – | – | – | a few words stored | loads |
| label | 0 | – | – | – | – | short lock |
| lifecycle | 0 | – | – (reads 0.8's phase, unchanged) | – | – | load + exit-request lock |
| links, monitors, trap_exit | 0 | – | – | – | monitor calls store the target id | table locks, bounded copies |
| supervisor position | 0 | – | – | – | 3 words at admission | scope lock |
| deferred, jobs | 0 | – | – | – | – | a load; the job set's lock |
| `get_state`/`get_status` | 0 | – | one status turn when called | – | – | a request |
| trees, crash/supervisor events | 0 | – | – | – | on exit and supervisor actions | supervisor locks, briefly |
| `Weak<dyn Probe>` | 0 | – | – | – | weak count +1 | upgrade |
| actor and system tables | console (default) | – | – | – | one shard write lock at spawn and one at publication | shard read locks |
| console thread | console (default) | – | – | – | one idle OS thread per process while a system or guard exists | per command |
| activity word | 1 | – | `fence(Release)` + 3 relaxed stores + 1 Release store at begin; 1 swap at end | – | – | seqlock read |
| message type | 1 | – | virtual call, cache probe, 1 store (64-bit, turn-stamped); a miss locks and may allocate | – | – | interner lookup |
| coarse clock | 1 | – | 1 relaxed load (in the begin stores above) | 1 load | – | – |
| poll stamps | 1 | – | – | 2 relaxed stores | – | – |
| mailbox counters | 1 | 1 relaxed `fetch_add` (per-actor line) | 1 store; 1 store at discard | – | – | 3 loads |
| trace flag | 1 | – | 1 relaxed load at activity end (same cache line) | – | – | – |
| clock thread | 1 | – | – | – | – | wakes every 5 ms once started |
| watchdog | 1 + started | – | – | – | – | O(actors) per sample on its own thread |
| fenced kill | 1 + policy | – | the end swap above | – | – | 1 CAS when firing |
| **default build** (`console` on) | – | **nothing** | **nothing** | **nothing** | tables + thread | – |
| `console` off | – | **nothing** | **nothing** | **nothing** | tier 0 only | – |

The default turn path is the 0.8 turn path unchanged (same code), which the
benchmark gate compares against the 0.8 release, not against today's main.

## Consistency guarantees and non-guarantees

What holds:

- **Per field.** Each scalar is one atomic load or one locked read. The
  activity fields come from one activity (seqlock read) or are reported
  `ReadContended`. Fields of one `ActorInfo` are not from one instant.
- **Monotonic.** Turn ids, `accepted`, `dequeued`, `discarded`, generations,
  restarts never decrease; `Dead` is final; ids are never reused (M4).
- **No ABA.** The same `TurnId` open in two reads means the same activity was
  open in between.
- **Ages.** A stamp age is an estimate: the clock may lag its 5 ms cadence by
  the thread's scheduling delay (reported as `clock_lag` in the snapshot), and
  a stamp of 0 is `ClockNotStarted`. The watchdog's `observed` duration is a
  lower bound measured with its own monotonic clock.
- **Listings.** An actor not past D for the whole listing appears exactly
  once, in `ActorId` order; actors starting or ending during it may or may not
  appear; one between D and publication appears as `Dead { published: false
  }`; no id appears twice.
- **Supervision.** Each `ChildInfo` row is consistent with itself (one
  child's lock); rows of one call may be sampled at different instants, so a
  level as a whole may never have existed. Counts agree with the rows of the
  same call. Expanded nodes belong to the captured instance.
- **No waiting on turns.** Out-of-band reads never enqueue, wake, run user
  code on the observed actor, or wait for a turn. They take short metadata
  locks the actor also takes (labels, links, the exit request), so they can
  contend briefly; never for the length of a turn.

Not guaranteed: an instant view across actors, groups or tree levels; exact
`len` under concurrent sends; that `get_status` answers a busy actor in time;
detection while the watchdog thread is itself starved; that a kill ends an
activity that never returns (L3); memory per actor, payloads, mailbox
contents, or history.

## The stall watchdog

```rust
#[cfg(feature = "introspection")]
let watchdog = introspection::Watchdog::start(WatchdogConfig {
    sample_every: Duration::from_millis(50),
    long_poll: Some(Duration::from_millis(100)),     // shared async workers only
    long_turn: Some(Duration::from_secs(10)),        // every activity kind
    long_mailbox: Some((10_000, 1_000)),             // enable, disable
    starvation: Some(Duration::from_millis(250)),    // one probe per target at a time
    kill_after: None,                                // Some(d): fenced kill on long turns only
    dump_dir: None,
    ..Default::default()
})?;
watchdog.probe_runtime(tokio::runtime::Handle::current(), "main");
let reports = watchdog.check_now();                  // one synchronous pass; report-only
```

| Detector | Signal (watchdog-observed) | Applies to |
|---|---|---|
| D1 long poll | the same `(turn, poll_count, poll_stamp)` seen `in_poll` for `long_poll` | `Backend::Async` and async groups; not `Thread`, `Blocking`, native groups, threads mode |
| D2 long turn | the same open turn id for `long_turn` | every actor and activity kind, both modes |
| D3 starvation | the probe task has not run `starvation` after spawn (reported before it runs) | async groups, probed runtimes |
| D4 long mailbox | `len >= enable`, cleared at `<= disable` | every actor |

**Structure.** A sampling thread reads the tables and the activity words and
pushes fixed-size reports into a bounded channel; a separate delivery thread
emits `tracing` events, updates metrics and writes dumps (at most one file per
second, 16 kept). If delivery blocks (a stuck subscriber or disk), sampling
continues and excess reports are dropped and counted (`reports_dropped`).
At most one starvation probe is outstanding per target; a late probe is
reported once and not re-spawned until it runs. Reports are rate limited per
actor and activity: at the threshold, 2x, 4x, and an `ended` report.

**Policy.** Report for D1, D3, D4. `kill_after` adds the fenced kill for D2
only (a long poll can be legitimate CPU work).

**Shutdown (revision 3).** Dropping `Watchdog` (or the end of the last
`ActorSystem` that started it) signals both threads and joins only the
sampling thread, which never runs user code and finishes within one sample.
It never joins the delivery thread, which may be blocked inside a `tracing`
subscriber or a filesystem write: that thread is detached and exits when its
current delivery returns. At most one detached delivery thread may exist per
process: `Watchdog::start` while a previous one is still blocked reuses no
thread and returns `Err(DeliveryStuck)` (reported, with the time it has been
blocked), so blocked threads cannot accumulate. Nothing in `ActorSystem`
destruction waits on subscriber code.

**Systems.** The watchdog's observation state is keyed by `ActorId` (unique
in the process); its limits are resolved per system, group and ChildSpec; its
reports carry the `SystemId`. `check_now()` covers every system;
`check_now_for(system: SystemId)` covers one, which is what multi-node
simulations use.

## Determinism and the simulator

- **Reads and report-only checks are neutral.** `info`, listings,
  `snapshot`, `tree`, the console's read commands (`hello`, `ps`, `info`,
  `links`, `tree`, `groups`, `snapshot`, `stuck`) and `check_now()` without a
  kill policy take no scheduling decision, register no waker, draw no
  randomness and iterate in `ActorId` order (ordered shards, sorted outputs),
  so a seed's decision tape is identical with or without them. The console's
  `state` is in band (an ordinary request), and `kill`, `terminate` and
  `trace` change execution: they are deterministic for the same script, not
  neutral.
- **Kill-capable checks are deterministic, not neutral.** A
  `check_now_with(cfg)` that commits a kill changes the run; it replays
  identically for the same seed and script.
- Under `spawned_sim` the stamp is virtual time and there is no clock,
  watchdog or console thread; `Watchdog::start` and the console listener are
  not available. Tests call `check_now()` and `console::handle_line()` (the
  same function the listener uses).

## Access paths

1. The Rust API above, identical in both modes.
2. `tracing`: the structured events, delivered by the watchdog's delivery
   thread or the emitting path.
3. Metrics (with `metrics`), each family checked by
   `scripts/check-metrics-exposition.py` with a negative control:
   `spawned_watchdog_{long_polls,long_turns,long_mailboxes,kills,reports_dropped}_total`,
   gauges `spawned_oldest_turn_observed_seconds`, `spawned_max_mailbox_len`,
   `spawned_actors_in_turn`, and per group (labels bounded by the groups an
   application creates) `spawned_group_scheduling_delay_seconds{group}` and
   `spawned_group_starvation_total{group}`.
4. The remote console.
5. `spawned-top` (rank 30, later): a TUI over the console protocol.

## Remote console (`spawned-ctl`)

A closed command set over the introspection data, no code evaluation (gap
G23).

### Lifecycle

| | Default | Override |
|---|---|---|
| Compiled | `console` feature, in `default` | omit it: no thread, socket, table or symbol |
| Starts | on `ActorSystem::new` (automatic) or `console::start(cfg) -> ConsoleGuard` (explicit, for bare-actor programs); never on a bare `.start()` | `SPAWNED_CONSOLE=off` or `ConsoleConfig { enabled: false }` |
| Mode | read-only | `SPAWNED_CONSOLE_ALLOW=kill,terminate,trace` (each separately) or `ConsoleConfig::allow(..)` |
| Directory | `$XDG_RUNTIME_DIR/spawned`, else `/tmp/spawned-<euid>` | `SPAWNED_CONSOLE_DIR=/dir` or `ConsoleConfig::dir(..)` |
| Failure | one `WARN spawned::console` (`ENOENT`, `EROFS`, `EACCES`, `EPERM`/seccomp, untrusted directory) and the program continues | — |

**Refcount.** A process-wide `Mutex<ConsoleState>` counts live
`ActorSystem`s and `ConsoleGuard`s. On 0→1 it publishes a new instance (the
protocol below) and starts the `spawned-console` thread; on 1→0 it stops the
thread (joined with a 1 s deadline; the thread runs no user code and never
calls a `tracing` subscriber, see audit below) and withdraws the instance; a
later 0→1 publishes a new instance with a new nonce. There is no `atexit`
and no `unsafe`: a process that exits without dropping its systems leaves
files that the lock protocol identifies as stale.

**`fork()` (revision 3).** v1 supports `fork()` followed by `exec` or
`_exit`, which is what process spawning does, and nothing else. All console
descriptors (the lock file, the listening socket, accepted connections) are
opened close-on-exec (Rust's standard library does so by default), so an
`exec`ed child holds none of them. A child that continues to run Rust code
after `fork()` must not use, start or drop any inherited `ActorSystem` or
`ConsoleGuard` (inherited mutexes may be held by threads that do not exist in
the child). As a safety net only, guard and system destruction never removes
or unlocks files when `std::process::id() != bound_pid`, and the lock is
never released with an explicit `unlock()` (which on a shared `flock` would
release it for the parent too), only by closing the descriptor, which leaves
the parent's lock in place. Continued execution after `fork()` with a working
console in the child would need its own reviewed protocol and is not offered.
Consequence documented for operators: a forked child that neither `exec`s
nor exits keeps an inherited lock descriptor open, so after the parent dies
`spawned-ctl list` sees the lock held and shows the entry as
`held, not answering` rather than deleting it.

**Where it runs.** On its own `spawned-console` OS thread, never on
application workers and not on a system's control runtime, so console work
cannot delay supervision or shutdown, and the console outlives any one system.
In tasks builds the thread runs a current-thread Tokio runtime with one task
per connection; in threads-only builds it accepts and hands each connection
to a short-lived thread (at most 8).

**Coverage.** Every actor, managed or bare, is in the actor table while
`console` or `introspection` is compiled. The 0.9 unified start funnels every
start function through one spawn path, so registration lives in one place; a
test compares the spawned counter with the table for every start function.

### Files, permissions and stale sockets

Per instance: `<pid>-<nonce>.sock` and `<pid>-<nonce>.lock`, with a random
64-bit nonce. The process umask is never touched.

**Publication (revision 3).** The lock is taken and verified before the
socket exists, and removed after it:

1. Check the directory (step 1 below), and keep it open by handle.
2. Create `<pid>-<nonce>.lock` with `create_new` (O_EXCL), mode `0600`,
   close-on-exec. `AlreadyExists` → new nonce (at most 3 tries).
3. `try_lock()` (non-blocking, exclusive). `WouldBlock` (a cleaner holds it)
   → close, new nonce. `Unsupported`/`ENOLCK` (for example on NFS) → no
   console (one warning).
4. Verify identity: `fstat` of the open descriptor equals `lstat` of the path
   (same `dev` and `ino`), the path is a regular file (not a symlink) owned
   by the effective uid. A mismatch or a missing path (a cleaner removed it
   between steps 2 and 3) → close, new nonce.
5. Bind `<pid>-<nonce>.sock`; `AddrInUse` → withdraw (below) and new nonce.
   Set `0600`; record the socket's `(dev, ino)` from `lstat`.
6. Serve.

**Withdrawal**, with the lock still held: stop accepting; remove the socket
if its `lstat` still shows the recorded `(dev, ino)`; remove the lock file if
it still matches; then close the lock descriptor (releasing the lock).

**Cleaning (`spawned-ctl list`).** For each `.lock`: open it read-only with
`O_NOFOLLOW` and close-on-exec; `try_lock()`. `WouldBlock` → live (or held
by a forked child, see above): never touched. Acquired → re-verify
`fstat == lstat`; if it still matches, remove the `.sock` (only if it is a
socket owned by the caller), then the `.lock`, then close. If it no longer
matches, close and skip. A `.sock` with no `.lock` cannot belong to a running
instance (the lock exists from step 2 until after the socket is removed), so
it is removed if owned by the caller. Two cleaners: only one acquires a given
lock; the other sees `WouldBlock` and leaves the entry for a later run.
Startup against a cleaner: a cleaner can only win step 3's lock on a file
with no socket, removes it, and the server's step 4 sees the missing path and
retries; a server past step 4 holds the lock, so no cleaner touches it.

1. **Directory.** `$XDG_RUNTIME_DIR` must be a directory owned by the
   effective uid with no group or other access (as the XDG spec requires);
   `spawned` inside it is created with `DirBuilder::mode(0o700)`. The `/tmp`
   fallback creates `/tmp/spawned-<euid>` with `mkdir` (mode `0700`). An
   existing directory is accepted only if `symlink_metadata` shows a real
   directory, owned by the effective uid, with `mode & 0o077 == 0`; otherwise
   the console does not start (one warning; no fallback to another
   location, which would invite squatting). A configured directory gets the
   same check, and every ancestor must be owned by root or the effective uid
   and not writable by group or others unless sticky (OpenSSH StrictModes).
   The directory is reopened and rechecked by handle before binding.
2. **Bind** inside it, then set the socket file to `0600` (defence in
   depth; the `0700` directory is the access control).
3. **Peer check.** Each connection's peer credentials (`SO_PEERCRED` on
   Linux, `getpeereid` on macOS, through Tokio's `peer_cred` or `rustix`) must
   match the process's **effective** uid. Root peers are refused unless the
   process runs as root.
4. **Liveness lock.** The instance holds an exclusive `File::lock` (std,
   stable since Rust 1.89) on its `.lock` file for its lifetime, taken before
   the socket exists (publication above).
5. **Cleanup** is the withdrawal above, only when `pid == bound_pid`; the
   server never removes anything but its own files.
6. **Stale entries** are removed only by a cleaner holding their lock, so a
   live instance, including one in another pid namespace with the same pid
   (the nonce keeps their names apart), is never deleted.

### Wire protocol

Line-delimited JSON; see the [wire and data contract](#wire-and-data-contract).

```text
<- {"v":1,"hello":{"pid":41237,"nonce":"9f2c…","pidns":"4026531836","binary":"ethrex",
     "spawned":"0.9.0","schema":1,"features":["console","introspection"],"allow":[],"watchdog":true}}
-> {"v":1,"id":1,"cmd":"ps","args":{"sort":"turn_age","limit":20}}
<- {"v":1,"id":1,"ok":{"taken_at_ms":91233411,"actors":[…],"next":"c:7"}}
-> {"v":1,"id":2,"cmd":"kill","args":{"target":"sys1/p2p/peer_table","expect":"12@3"}}
<- {"v":1,"id":2,"err":{"code":"not_allowed","msg":"kill is disabled (SPAWNED_CONSOLE_ALLOW)"}}
```

### Commands

| Command | Allow | Answers from | While an actor is wedged |
|---|---|---|---|
| `hello`, `ps [--sort ..] [--group g] [--type t] [--limit n]`, `info <a>`, `links <a>`, `tree [sup]`, `groups`, `snapshot` | read | tables, tier 0, tier 1 if compiled | answers |
| `stuck [--window 2s]` | read | watchdog reports, or two samples of turn ids (needs `introspection`) | answers |
| `state <a> [--timeout 5s]` | read (state is opt-in per actor through `Inspect`) | `get_status` under a status permit (limits below); awaited on the console runtime, never blocking the thread | times out; the error points to `info` |
| `trace <a> <rate> [--max n] [--for t]` | `trace` | bounded turn events (needs `introspection`) | events stop; `info` shows the open turn |
| `kill <target> --expect <id>[@gen]` | `kill` | resolves the target once to an instance-bound handle; `stale_target` if the id or generation differs; then `ChildHandle::kill()` | `accepted`: effective when the activity returns (L3) |
| `terminate <sup> <child> --expect <id>[@gen]` | `terminate` | the supervisor's terminate operation with the expectation passed in: the supervisor compares the child's current `ActorId` and generation with it inside the operation's own admission (its control loop), so a restart between the console's lookup and the operation yields `stale_target`, never the termination of the newer generation (dynamic supervisors: `terminate_slot` gains an `expect` argument, coordinated with PR #245; static supervisors once G6 adds the operation) | as the supervisor's call |

`kill` is OTP's `exit(Pid, kill)`, not `restart_child`: the supervisor then
applies its policy. A Temporary child is not restarted; under OneForAll or
RestForOne siblings are rebuilt; the restart counts against the budget and may
fail the tree. The reply says `accepted`; completion is visible through
`info`/`tree`. Targets carry the system id (`sys<id>/…`) and an expected
instance, so a stale operator view cannot hit a newer generation. There is no
`eval`, `replace_state`, `suspend` or arbitrary send.

**Audit (revision 3).** Every command produces a fixed-size audit record
(`spawned::console`; WARN for `kill`, `terminate`, `trace`). The console
thread never calls a `tracing` subscriber itself: records go through a
bounded channel (256) to the same kind of detached delivery thread the
watchdog uses. If delivery is blocked, records beyond the channel are dropped
and counted, the count is reported in the next `hello` and on recovery, and
a mutating command is refused with `audit_unavailable` while its record
cannot be queued (reads continue). So a blocked subscriber can stop audit
output but cannot stop the console, and no mutation goes unrecorded.

**Console turn trace (v1, tier 1).** One fixed-size event per activity of a
traced actor (turn id, kind, message, observed duration, outcome) into the
connection's own session; at most one trace per connection and 4 traced
actors per process; `max` default 100 (cap 10,000), `for` default 10 s (cap 5
min); stops at either, or on disconnect; events beyond the session's
capacity are dropped and counted. The run loop reads the `trace` flag (its own atomic, owned by the console) at each activity end.

### Limits

| Resource | Limit |
|---|---|
| connections | 8; the ninth is refused with `busy` |
| request line / JSON depth | 64 KiB / 32 |
| requests in flight per connection | 1 executing, 4 queued, then `busy` |
| commands per connection | 20 per second |
| response size | 4 MiB; larger results are cut with `"truncated":true` and a cursor |
| `ps` sorted by a metric | top-k only (recon `proc_count`): one pass over the shards keeping a heap of at most `limit` ≤ 1,000 entries, so memory is O(k), not O(actors); no pagination of sorted views |
| `ps`/`snapshot` in id order | pages of at most 1,000 actors by keyset (`after = last id`): exact for actors that live across pages, no captured vectors, no cursor state on the server |
| work per slice | a command processes at most 1,000 actors or 5 ms, then yields to the console runtime, so other connections and every write deadline keep running; threads-only builds serve each connection on its own thread |
| memory | per connection 8 MiB (request, response under construction, write buffer); process-wide 32 MiB; a command that would exceed either fails with `too_large` before allocating more |
| `state` timeout | at most 30 s (default 5 s) |
| queued `state` requests | a console `state` takes a status permit before it enqueues: at most 2 per target actor and 32 per process, and 1 per connection. The permit lives in the `StatusEnvelope` and is released only when the envelope is dequeued (run or skipped) or discarded at D, never on timeout or disconnect. With the permits taken, `state` answers `busy` ("2 status requests already queued for this actor; it may be wedged, use `info`"). So a client that repeatedly requests and disconnects against a wedged actor retains at most 2 envelopes for it and 32 in total |
| `tree` | depth 64, 10,000 nodes |
| strings in output | labels, names, errors: 256 bytes; `format_status` output cut at 64 KiB. The formatter's own allocation while it runs (on the actor) is user code and outside the guarantee |
| write deadline | 2 s for a whole frame (partial writes included), then the connection is closed (a paused client cannot block others) |
| idle connection | 10 minutes |
| disconnect | cancels the connection's pending `state` and `trace` |

### Security

- **Threat model: the same effective uid.** Connecting requires the
  process's effective uid. Such a principal can already signal the process,
  read its files and usually trace it (subject to Yama's `ptrace_scope`, so
  this is an authorization choice, not proof of equivalent access). Read-only
  by default; three narrow mutations each enabled explicitly. Other local
  users are stopped by the `0700` directory and the peer-uid check.
- **What it reveals.** Types, labels, names, message type names, counters,
  exit reasons, structure. Never payloads or mailbox contents; state only
  through `Inspect`. Labels may hold peer addresses: same exposure as logs.
- **Why no TCP.** A network listener needs authentication, encryption and
  secrets, and fails as remote access. Remote use goes through SSH
  (`ssh host spawned-ctl <pid> ps`).
- **Why no eval.** An evaluator turns any access into arbitrary code
  execution; Rust has no safe interpreter to offer.
- **Denial of service.** The limits above; console work runs on its own
  thread with per-frame write deadlines, so a slow or hostile client cannot
  stall supervision, shutdown, the watchdog or other clients.
- **Compared with Erlang.** `erl -remsh` joins distribution: epmd, a TCP
  port, a shared cookie, unencrypted unless TLS distribution is configured,
  and full `eval`. Anyone with the cookie and network reach can run code. The
  console has no eval, no network surface and no secret.

## Wire and data contract

Frozen before release:

- **Framing.** One JSON object per line, UTF-8, at most 64 KiB per request,
  nesting depth 32. Encoded with `serde_json` under `console` through
  dedicated wire structs, so the schema does not follow Rust's derived
  representation.
- **Envelope.** Server first: `{"v":1,"hello":{…}}`. Requests
  `{"v":1,"id":<u32>,"cmd":<string>,"args":{…}}`; `id` is client-chosen and
  unique among the connection's in-flight requests. Responses
  `{"v":1,"id":…,"ok":{…}}` or `{"v":1,"id":…,"err":{"code":…,"msg":…}}`.
  Streams send `{"id":…,"event":{…}}` lines then exactly one
  `{"id":…,"end":{"reason":…,"lost":<n>}}`.
- **Error codes.** `version`, `parse`, `too_large`, `busy`, `unknown_cmd`,
  `bad_args`, `not_found`, `stale_target`, `not_allowed`, `unavailable`,
  `timeout`, `actor_stopped`, `noproc`, `callback_failed`, `internal`.
- **Numbers and time.** Every `u64` (ids, turn ids, generations, counters,
  `accepted`/`dequeued`/`discarded`, `turns`) is a decimal string, whatever its
  value. Only values bounded below 2^32 by construction (limits, link and
  monitor counts, page sizes) are JSON numbers. Durations are integer
  milliseconds in `*_ms` fields; `taken_at_ms` is monotonic milliseconds
  since process start; `wall_unix_ms` is wall time.
- **Unavailable values.** A field that cannot be given is
  `{"unavailable":"feature"|"read_contended"|"clock_not_started"|"redacted"|"dead"}`,
  never `null` or omission, so "not compiled" and "could not read" differ.
- **Enums** are strings; a client shows an unknown value verbatim.
- **Evolution.** New fields and enum values may appear within `v:1`; removing
  or renaming one, or changing a type, is `v:2`. A client ignores unknown
  fields.
- **Pagination.** Id-order listings page by keyset: `next` is the last
  `ActorId` returned, and the next page starts after it, with no state kept
  on the server; an actor alive across both pages appears exactly once.
  Views sorted by a changing metric are top-k only and not paginated
  (revision 3: the captured-vector cursor of revision 2 did not bound
  memory).
- **Rust types.** Every public type in the API sections is
  `#[non_exhaustive]`; `Field<T>` carries the same `Unavailable` reasons.

## OTP alignment

| OTP | Spawned v1 | Status |
|---|---|---|
| `process_info(status)` | `Lifecycle` | adapted: different vocabulary (`Idle`/`InTurn`, not `waiting`/`running`/`runnable`) |
| `message_queue_len` | `MailboxCounts.len` | aligned (tier 1), approximate under concurrency |
| `current_function` | `Activity.kind`, `message`, ages | adapted: message type instead of a stack |
| `links`, `monitors`, `trap_exit`, `registered_name`, `initial_call` | same names | aligned; `monitored_by` deferred |
| `reductions` | turn ids, `dequeued` | adapted |
| `memory`, `dictionary`, `group_leader` | none | Rust-forced (rank 18a for memory) |
| `proc_lib:set_label` (OTP 27) | `set_label` | aligned |
| `erlang:processes/0` | `processes()` | aligned (with `console` or `introspection`) |
| `sys:get_state/1,2` | `get_state(f)` | aligned in placement (between callbacks, in order, 5 s); adapted in form (a closure) |
| `sys:get_status`, `format_status/1` | `get_status`, `Inspect` | **stronger**: OTP's default `format_status` returns the state; Spawned redacts unless `Inspect` |
| formatter or callback crash | `CallbackFailed`, actor continues | adapted: OTP returns `{callback_failed, …}` from `sys:replace_state`/`get_state` callbacks and substitutes a failure term for a crashing `format_status`; Spawned uses one error for both |
| `sys:statistics` | turn ids, counters | adapted: always on with the feature |
| `sys:trace`, OTP 27 trace sessions, `recon_trace` | sink with sessions; bounded console turn trace | partial; full trace 0.10 |
| `sys:replace_state`, `suspend`, `change_code` | none | rejected |
| `which_children`, `count_children` | same, plus `tree` | aligned rows; counts as OTP |
| SASL supervisor reports, proc_lib crash reports | `spawned::supervisor`, `spawned::crash` | aligned fields where they exist |
| `system_monitor` `long_schedule`, `long_message_queue` | D1, D4 | aligned in spirit (Spawned reports; OTP messages one monitor process) |
| observer, recon `proc_count`/`proc_window`, `recon:info` | `spawned-top`, `top`/`window`; no mailbox copy | aligned |
| `exit(Pid, kill)` | fenced watchdog kill; console `kill` | aligned reason; effective at return (L3, Rust-forced) |
| `erl -remsh` | `spawned-ctl attach` | adapted: closed command set, local only |

## Alternatives considered

| Alternative | Decision | Reason |
|---|---|---|
| Control lane for `get_status` | Rejected for v1 | OTP handles system messages in order; `info()` is the stuck path. G11 (0.10) may add a receiver opt-in. |
| Reading state mid-turn | Impossible | `&mut A` is held across awaits. |
| Turn ids in tier 0 by widening 0.8's phase word | Rejected (revision 2) | Packing and a persistent counter are added work on the turn path and touch a Loom-verified protocol; the strict rule wins. |
| `Instant::now()` per turn and poll | Rejected | ~20 ns per read; the stamp is display-only. |
| The stamp deciding kill eligibility | Rejected (revision 2) | A late clock would make a young turn look old; the watchdog's own observation is a defensible lower bound. |
| Tokio task hooks, tokio-console | Not the mechanism; complement later | `tokio_unstable`, no actor or message identity, blind to `Thread` and threads mode. |
| Mailbox length from the channel | Not possible | Tokio's `len()` is receiver-side; `std::sync::mpsc` has none. Rank 37's own mailbox can provide it later. |
| Per-actor counters always on | Rejected | An RMW per send. |
| Process-wide table in the default build | **Accepted** (console decision) | Spawn/exit cost only, required for attaching to bare-actor programs; compiled out with `console` off. |
| `&'static str` or `TypeId` in an atomic | Rejected | Needs `unsafe` or 128-bit atomics. |
| `ExitReason::Stalled` | Rejected | OTP uses `killed`. |
| Turn abort at an await | Deferred (0.10 at the earliest) | Cost per suspended turn, L3 change, cancellation safety, deferred obligations; own model. |
| Flight recorder in v1 | Deferred to 0.10 | Same ring as full `sys:trace`; design once. |
| Per-turn `tracing` span | Deferred to rank 28 | Not free even when filtered. |
| Console on a system's control runtime | Rejected (revision 2) | A system may end while others live; console work must not delay supervision. |
| Console started by bare `.start()` | Rejected (revision 2) | Hidden threads and sockets in library calls, seccomp failures; bare programs opt in with `console::start`. |
| Changing the umask to create the socket | Rejected (revision 2) | Process-global, races other threads, needs `unsafe`. |
| Pid-only socket names, pid-based stale cleanup | Rejected (revision 2) | Pid namespaces and reuse; nonce plus lock instead. |
| `atexit` cleanup | Rejected | Needs `unsafe`; the lock makes leftovers identifiable. |
| Console over TCP/HTTP, console `eval`, abstract-namespace socket | Rejected | See security. |
| `restart` as a console command | Renamed `kill` (revision 2) | It is `exit(Pid, kill)`, not OTP `restart_child`. |
| Watchdog as a Tokio task | Rejected | It would starve with what it watches. |
| A coherent published supervisor view | Deferred | Would make each tree level an instant; costs work on supervisor paths; v1 promises per-row consistency only. |

## Semantics contract: guarantees added

Proposed section 13 of [semantics.md](semantics.md). L3, L5, M1, M2, R1 and
R3 are unchanged; R4 gains one exception (I4).

| Id | Guarantee | Checked by (planned) |
|---|---|---|
| I1 | **Out of band.** `info`, listings, snapshots, trees, the console's read commands (not `state`, `kill`, `terminate`, `trace`) and watchdog sampling never enqueue, wake, run user code on the observed actor or wait for a turn; they may contend briefly on metadata locks. | `info_returns_during_a_wedged_turn` (every placement); `reads_do_not_change_the_tape` |
| I2 | **Activity reads.** The activity fields come from one activity or are reported `ReadContended`; the same open `TurnId` in two reads is the same activity; a stamp of 0 is `ClockNotStarted`. | `activity_slot_loom`; `activity_*`; `lazy_clock_*` |
| I3 | **Monotonic and final.** Turn ids, mailbox counters, generations, restarts never decrease; `len` reads 0 from D and the counters are final once every send call that began before D has returned; `Dead { reason }` is final, its reason equals `wait_exit()`'s, and `published` becomes true exactly when `wait_exit()` becomes ready. | `monotonic_*`; `d_to_publication_*`; fuzz oracle |
| I4 | **Status is a turn.** `get_state`/`get_status` run in their own turn, between handler turns, in mailbox order, with one outcome (`Ok`, `RequestTimeout`, `ActorStopped(final)`, `NoProc`, `CallbackFailed`). A caught callback panic alone does not end the actor; no effect is rolled back. A status envelope whose caller gave up before dispatch is not run (an exception to R4). | `Introspection!StatusBetweenTurns`, `!StatusDischarged`, `!StatusReasonIsFinal`, `!StatusAtMostOnce`, `!NoCallbackAfterGiveUp`, `!CallbackPanicContained`; `status_*` |
| I5 | **Redaction.** Type-erased state appears only for `Inspect` actors. | `state_is_redacted_by_default`; trybuild |
| I6 | **Listings.** An actor not past D for a whole listing appears exactly once, in id order; between D and publication it appears `Dead { published: false }`. | `listing_under_churn`; `actor_table_loom`; fuzz |
| I7 | **Supervision rows.** Each row is self-consistent; counts agree with the rows of the same call; expanded nodes belong to the captured instance. | `tree_*`; `snapshot_during_restarts` |
| I8 | **Watchdog detection.** With the watchdog running and its threads scheduled, D1–D4 report within their threshold plus two samples; no report for an activity observed open for less than its threshold; sampling continues while delivery is blocked. | `watchdog::*`; `check_now_*` |
| I9 | **Fenced kill.** A watchdog kill commits only on the open activity it observed; a failed commit has no effect; a commit before D makes the final reason `Killed` however the activity ends (return; panic in `started()`, a handler or `stopped()`; status end; runtime drop); nothing is dispatched after it; `stopped()` is never decided after a kill or a commitment, and one already running finishes; no commit after D. | `Introspection!NoChargeFinishedTurn`, `!CommitOnOpen`, `!KillDominates`, `!NoDispatchAfterStop`, `!KillSkipsStopped`, `!NoCommitAfterD`; Loom; sim |
| I10 | **Reports and sessions.** Documented targets and fields; sessions isolated and bounded; losses counted. | `reports::*`; `sink::*` |
| I11 | **No cost on the default path.** With `introspection` off (console on or off), send, dispatch and turn execute the 0.8 code unchanged. | benchmark gate; codegen check |
| I12 | **Determinism.** Reads, the console's read commands and report-only checks leave the tape unchanged; `state`, console mutations and kill-capable checks replay identically for the same script. | `reads_do_not_change_the_tape`; `console_queries_do_not_change_the_tape`; `kill_check_replays` |
| I13 | **Console.** Local Unix socket only, in a directory owned by the effective uid with no group/other access; peers with another effective uid are refused; read-only unless each mutation is allowed; every command produces an audit record or is counted as dropped, and no mutation runs without a queued record; the lock is held before the socket exists and until after it is removed, and only a lock holder removes files, so a live instance's files are never removed; limits bound memory, per-slice work and time per client; a failure to start never fails the program. | `console::*` |

## TLA+ model and trace validation

[`Introspection.tla`](../../verification/tla/Introspection.tla), revision 3:
one actor whose every activity (`started()`, messages, the status turn,
`stopped()`) opens a new turn id; activities that may suspend forever (the
status callback, synchronous, does not); panics in `started()`, a handler and
`stopped()`, and a caught status-callback panic; the status request with a
deadline, its callback admission (`StatusAdmit`), its answer and the end of
its turn as separate steps, the slot closing before or after admission; administrative stop and
unfenced kill (also between the drop swap and D); the runtime drop's swap and
D as separate steps; publication, which may never come; and the watchdog's
observe, successful CAS, failed CAS and record. Reads are not actions. It
stays separate from `Lifecycle.tla` (being changed by PR #240) and abstracts
links and monitors, which v1 does not touch.

Revision 3 changes, from the round-two reviews: `Committed == commitPreD`
(a later `Observe` can no longer reset it); every ghost is derived in `Next`
from the state change of the step, never from the `Fault` selector
(`CallbackPanicContained` holds over every step taken while a status turn is
open, not only its end);
commitment validity is checked at the CAS itself (`CommitOnOpen`) and a
target that has ended clean is charged from the moment of commitment
(`NoChargeFinishedTurn`); `stopped()` may not be decided after a commitment
whose bit was lost (`KillSkipsStopped`, the round-two counterexample
`KillCas → Stop → MsgEnd loses bit → BeginExit`); the status answer carries
the final reason (`StatusReasonIsFinal`); and actions are split so each one
is a single implementation linearization point.

Invariants: `NoChargeFinishedTurn`, `CommitOnOpen`, `NoDispatchAfterStop`,
`KillDominates`, `NoCommitAfterD`, `KillSkipsStopped`, `StatusBetweenTurns`,
`StatusDischarged`, `StatusReasonIsFinal`, `StatusAtMostOnce`,
`NoCallbackAfterGiveUp`, `CallbackPanicContained`. They cover I4 and I9 only;
the other guarantees (memory ordering, listings, limits, redaction, cost)
are checked by Loom, tests and the benchmark gate listed in the test plan.

Results (TLC 2.19 on fede, 4 workers, `nice 19`, one configuration at a
time): `MCIntrospection` (3 messages) 47,968 distinct states, depth 27, no
error; 4 messages 80,482 states, depth 30 (round three; the controls affected
by round three were re-run: the four callback controls and the four other
status controls). Each control failed on its named
invariant, run alone (`negative_checks.py`, `INTROSPECTION_CONTROLS`):

| Control | Bug | Fails |
|---|---|---|
| `CheckThenAct` | checks the turn, kills in a later step | `NoChargeFinishedTurn` |
| `FailedCasKills` | a failed CAS still records a kill | `NoChargeFinishedTurn` |
| `LooseCas` | the CAS compares the turn id but not the open bit | `CommitOnOpen` |
| `KillBitIgnored` | normal end drops the kill bit, a turn follows | `NoDispatchAfterStop` |
| `LostBitRunsStopped` | the bit is dropped and a stop then runs `stopped()` | `KillSkipsStopped` |
| `PanicOverKill` | a handler panic ignores the kill bit | `KillDominates` |
| `StartPanicOverKill` | a panic in `started()` ignores the kill bit | `KillDominates` |
| `HookPanicOverKill` | a panic in `stopped()` ignores the kill bit | `KillDominates` |
| `DropLosesKill` | the drop swap ignores the kill bit | `KillDominates` |
| `NoInvalidateAtD` | the word stays open after a drop | `NoCommitAfterD` |
| `KillRunsStopped` | `stopped()` decided after a recorded kill | `KillSkipsStopped` |
| `DispatchAfterStop` | dispatch after a stop or kill | `NoDispatchAfterStop` |
| `StatusInTurn` | status answered inside a suspended message turn | `StatusBetweenTurns` |
| `StatusLost` | waiting status not answered at D | `StatusDischarged` |
| `StaleStatusReason` | status answered at D with a reason computed before the kill | `StatusReasonIsFinal` |
| `DoubleAnswer` | status answered twice | `StatusAtMostOnce` |
| `CallbackAfterGiveUp` | admission ignores a closed slot and runs the callback | `NoCallbackAfterGiveUp` |
| `RunAfterSkip` | the callback runs after admission decided to skip | `NoCallbackAfterGiveUp` |
| `CallbackPanicStops` | a caught callback panic begins exit at the status turn's end | `CallbackPanicContained` |
| `AnswerPanicStops` | the panic is recorded while the callback runs, before the end | `CallbackPanicContained` |

No liveness property: with L3 unchanged, an actor whose activity never
resumes legitimately never dies.

### Trace events and their mapping

Under `lifecycle-trace` the implementation records one event per model
action, at that action's linearization point; `TraceIntrospection.tla`
replays a trace as a behaviour of the model, one event per step (no merged
events, no silent steps except `Observe`, which the watchdog records only when
it acts on what it saw). The CAS and the kill record are separate events and
are never recorded inside one critical section, so their race stays
observable.

| Event | Fields | Model action | Linearization point |
|---|---|---|---|
| `IStart` | turn | `Init` | spawn |
| `IStartEnd` / `IStartPanic` | turn, old word | `StartedEnd` / `StartPanic` | the end swap after `started()` |
| `ISend` | envelope id, accepted | `Send` | admission (existing `life_send`) |
| `IStatusSent` | request id, deadline | `SendStatus` | admission |
| `IStatusTimeout` | request id | `StatusTimeout` | slot expiry |
| `IDispatch` | turn, kind, envelope or request id | `Dispatch` | `word.store(open)` |
| `IStatusAdmit` | request id, `run` or `skip`, slot state seen | `StatusAdmit` | the status envelope's slot check, inside the status turn |
| `ISuspend` / `IResume` | turn, poll count | `Suspend` / `Resume` | a poll returns `Pending` / the next poll's entry (messages and async hooks) |
| `IEnd` / `IPanic` | turn, kind, old word | `MsgEnd` / `MsgPanic` | the end swap |
| `IStatusAnswer` | request id, outcome (`ok`, `failed`) | `StatusAnswer` | the reply slot's discharge in the status turn |
| `IStatusEnd` | turn, old word | `StatusEnd` | the end swap of the status turn |
| `IExitDecide` | run or skip | `ExitDecide` | 0.8's `begin_hook` under the exit-request lock (existing `Hook` event) |
| `IHookBegin` | turn | `HookBegin` | `word.store(open)` for `stopped()` |
| `IHookEnd` / `IHookPanic` | turn, old word | `HookEnd` / `HookPanic` | the end swap after `stopped()` |
| `IDropSwap` | old word | `DropSwap` | the cleanup's swap to DEAD |
| `ID` | final reason, status answers given | `Die` | the existing death point (status discharge at D is part of it) |
| `IPublish` | — | `Publish` | completion publication |
| `IStop`, `IKill` | reason | `Stop`, `ExternalKill` | exit-request lock |
| `IObserve` | turn seen, open | `Observe` | the watchdog's word load |
| `ICas` | expected word, **returned word**, result | `KillCas` / `KillCasFail` | the CAS |
| `IRecord` | target turn | `KillRecord` | exit-request lock |

The ordinary kill between `IDropSwap` and `ID` is now a step of its own
(`ExternalKill` from phase `dropping`), which the revision-2 atomic
`RuntimeDrop` hid. An event the model cannot take (placement failure,
runtime experiments) records `Unmodelled` and rejects the trace.

Required trace suites: a wedged turn with a fenced kill; the kill racing the
end swap in both orders (failpoints); a kill during `started()` and during
`stopped()`; panics in `started()`, a handler and `stopped()` after a
commit; a runtime drop after a commit, with an ordinary kill between the drop
swap and D; status queued at D, its slot expiring before dispatch, between dispatch and
admission, and after admission, a callback panic;
console `kill` with a stale expectation. Trace coverage must reach every
production action (fault-only actions such as `StatusFromTurn` and
`KillDecide` are excluded). Negative controls on **recorded traces**
(`require_rejected`): an `IStatusAnswer` after an `IStatusAdmit` with
`skip`; an `IRecord` moved after the next `IDispatch`; an `ICas`
with result `ok` whose returned word is closed; an `IStatusAnswer` outside
its status turn; an `ID` that drops a pending `IStatusAnswer`; an
`IHookBegin` after an `ICas` whose bit no end consumed.

## Test plan

Placements: A `Backend::Async`, B `Blocking`, T `Thread`, N threads mode, G
`async_group`, TG `thread_group`, P `pinned_group` (Linux), using the
`semantics_gaps` placement macros.

### Integration tests (`concurrency/tests/introspection.rs`, both modes, every placement)

| Test | Guarantee | Fails if |
|---|---|---|
| `info_returns_during_a_wedged_turn` (await that never completes; sync block on T/N) | I1 | `info()` waits for the turn |
| `lifecycle_walks_starting_idle_in_turn_exiting_dead` | lifecycle | a state skipped or wrong |
| `d_to_publication_interval` (a `Drop` gated on a barrier) | I3, I6 | `Dead{published:false}` not shown, `is_alive()` true after D, `NoProc` not returned, `wait_exit()` returns early, entry removed at D |
| `activity_names_the_message_and_turn` (two types whose names are prefixes) | I2 | wrong tag or turn id |
| `activity_fields_are_from_one_turn` (10^6 short turns, reader loop) | I2 | a mixed read accepted |
| `lazy_clock_started_mid_turn_does_not_false_kill` | I2, I8 | stamp 0 reported as an age, or a report or kill fires early |
| `mailbox_counts_every_transition` (accepted, failed enqueue, dispatch, stop discard, death drain, in-flight sends, `Exit`/`Down`, status) | fields | `len` wrong at quiescence or nonzero after D |
| `monotonic_counters_never_decrease` | I3 | a decrease |
| `links_monitors_trap_exit_parent_supervisor_position` | fields | a field wrong |
| `status_runs_between_turns_in_mailbox_order` | I4 | answered early or during a turn |
| `status_outcomes` (idle, queued at D, after D, timed out, receiver dropped) | I4 | an unlisted or second outcome |
| `status_skipped_when_caller_gave_up` | I4 | the callback runs |
| `callback_panic_is_callback_failed_and_actor_continues` (also dropping a `ReplyTo` taken from a `RefCell`) | I4 | actor exits; the token not `ReplyDropped` |
| `status_during_stopped_and_destruction` | I4 | admitted during `stopped()` not `ActorStopped`; sent during destruction not `NoProc` |
| `get_state_runs_while_deferred_reply_is_outstanding` | I4 | credit consumed or disturbed |
| `status_bypasses_metrics_mailbox_accounting` | I4 | `mailbox_depth` leaks or a handled message counted |
| `state_is_redacted_by_default` / `inspect_formats_state` / `format_status_truncated` | I5 | state leaks; no cut |
| `listing_under_churn` | I6 | a stable actor missing or duplicated; not sorted |
| `tree_rows_and_counts_agree` / `tree_expands_captured_instance` (restart between capture and expansion) | I7 | counts disagree; generation N+1 attached to N's row |
| `tree_holds_no_lock_across_children` (lock-order check under a nested restart storm) | I7 | deadlock or lock held |
| `reports::crash_event_fields`, `reports::supervisor_event_fields`, `crash_last_message_survives_stopped` | I10 | a field missing; `last_message` lost |
| `sink::sessions_are_isolated` | I10 | interference or uncounted loss |
| `watchdog::long_poll_names_actor_and_message` (A, G only) | I8 | missing, or fires on T/B/N/TG |
| `watchdog::long_turn_across_awaits`, `..._in_started`, `..._in_stopped`, `..._in_status_callback` | I8 | missing |
| `watchdog::starvation_reported_before_probe_runs`, `one_probe_outstanding` | I8 | late, or probes pile up |
| `watchdog::no_report_under_threshold` | I8 | false positive |
| `watchdog::delivery_blocked_sampling_continues` (a subscriber that blocks; a dump dir on a full filesystem) | I8 | sampling stops; drops uncounted |
| `watchdog::drop_and_restart` | I8 | threads leak; `AlreadyRunning` missing |
| `fenced_kill_kills_the_observed_turn`, `..._skips_a_finished_turn` | I9 | kill misses or hits the next turn |
| `fenced_kill_beats_handler_panic_and_self_stop` | I9 | reason not `Killed`; `stopped()` runs |
| `fenced_kill_during_started_and_stopped` | I9 | `stopped()` runs after a kill in `started()`; a running `stopped()` interrupted |
| `fenced_kill_then_runtime_drop` (and an ordinary kill between the drop swap and D) | I9 | reason not `Killed` |
| `fenced_kill_then_panic_in_started_or_stopped` | I9 | reason `Panic` after a commitment |
| `idle_and_dead_reads_validate_the_tag` (reader racing activity ends) | I2 | an idle read pairs one turn's id with another's tag |
| `paused_sender_after_d` (failpoint between the push and `fetch_add`) | I3 | `len` nonzero after D; counters change after every pre-D send returned |
| `group_listing_excludes_published_with_draining_helper` | I6 | a published actor listed while its helper keeps the scope |
| `turn_and_poll_identity_with_reduced_widths` (cfg(test) widths) | I2, I8 | a wrapped poll count read as one continuous poll; an exhausted turn id reused |
| `kill_waits_for_a_wedged_turn` | I9, L3 | preempted; `kill_pending` not shown |
| `feature_off_api_shape` | tiers | tier 1 fields not `Unavailable(Feature)` |

### Console tests (`concurrency/tests/console.rs`, Linux and macOS; `tools/spawned-ctl`)

| Test | Fails if |
|---|---|
| `starts_with_actor_system_not_with_bare_start` | a socket appears for bare `.start()` |
| `console_start_guard_for_bare_programs` | no socket with the guard; left after its drop |
| `refcount_binds_and_unbinds_and_binds_again` (two systems, then a third later) | socket missing while a system lives; not removed at 1→0; no rebind |
| `forked_child_never_unlinks_parent_socket` (`drop_subprocess`-style fork) | the parent's files removed |
| `stale_pair_removed_only_when_lock_is_free` (SIGKILLed child) | live pair removed; dead pair kept |
| `same_pid_two_namespaces_coexist` (two instances forced to the same pid prefix) | one removes the other |
| `directory_checks` (symlink, foreign owner, group-writable, untrusted ancestor) | the console uses it, or the program fails |
| `umask_untouched` (a concurrent thread creating files during bind) | a file's mode changes |
| `peer_with_another_euid_is_refused`, `root_refused_unless_process_is_root` (CI job as root, setuid child) | a command answered |
| `bind_failure_warns_once_and_continues` (ENOENT, EROFS, EACCES dirs) | the program fails or warns twice |
| `commands_answer_while_a_handler_is_wedged` (every worker of a group blocked) | a command waits |
| `state_times_out_on_a_wedged_actor_and_says_use_info` | hangs |
| `stuck_without_introspection_says_unavailable` | a misleading answer |
| `slow_reader_does_not_wedge_control_plane` (client never reads during `snapshot` and `trace`) | other clients, the watchdog or a supervisor restart stall; the client not dropped after 2 s |
| `partial_lines_and_huge_requests` | crash, unbounded buffering |
| `eight_attaches_and_the_ninth_refused`, `pipelined_requests_bounded` | starvation, unbounded queue |
| `large_tables_are_paginated_and_truncated` (100,000 actors) | over 4 MiB; duplicated or skipped rows across pages |
| `mutations_refused_unless_each_allowed`, `kill_with_stale_expectation_is_refused`, `trace_stops_at_max_for_and_disconnect` | wrong gate, stale hit, trace keeps running |
| `every_command_is_logged` | a command without an event |
| `spawned_console_off_opens_nothing`, `feature_off_build` (`cargo tree`, `nm`) | thread, socket or console symbol present |
| `protocol_v1_contract` (golden JSON for every command; unknown version; unknown fields ignored; unavailable encodings; every `u64` a string; keyset pages; id echo; stream `end`) | contract drift |
| `publication_orders_lock_before_socket` (failpoints between each publication step, a cleaner run at each) | a cleaner removes a live socket; a server serves without its lock; the socket outlives its lock |
| `two_cleaners_and_a_starting_server` | a live entry removed; a dead one left forever |
| `fork_then_exec_child_holds_no_console_descriptor` (`/proc/<pid>/fd` or `fcntl` checks) | an inherited lock or socket descriptor |
| `fork_without_exec_child_never_unlocks_or_unlinks` (child exits after dropping nothing, and a variant that drops a guard) | the parent's lock released or files removed |
| `held_but_not_answering_is_shown_not_deleted` | the entry deleted |
| `topk_ps_and_memory_budget` (1,000,000 actors) | memory proportional to the table; budget exceeded without `too_large` |
| `large_command_yields` (a 100,000-actor `snapshot` on one connection while another sends `info` every 10 ms) | the second client's latency exceeds one slice plus scheduling |
| `wedged_actor_state_requests_are_bounded` (10,000 rounds of connect, `state`, disconnect against a permanently wedged actor, from several connections) | more than 2 envelopes queued for the actor or 32 in the process; a permit released before dequeue; permits not returned at D |
| `blocked_audit_delivery` (a subscriber that blocks forever) | reads stop; a mutation runs without a queued record; drops uncounted |
| `drop_with_blocked_delivery_does_not_hang`, `restart_with_stuck_delivery_errors`, `last_system_removal_with_blocked_delivery` | a hang; blocked delivery threads accumulate |
| `terminate_expect_is_fenced_at_the_supervisor` (a restart forced between lookup and operation) | the newer generation terminated |
| `trace_sessions_cannot_overwrite_each_other` (enable, disable, disconnect racing) | a session clears another's trace; a trace survives its session |
| `every_start_function_registers` | an actor missing from the table |

### Simulator tests (`#[sim_test]`)

- `snapshot_during_restarts` (PCT): a OneForAll cohort restarting while
  `tree()`/`actors()` run every step; I6, I7 oracles.
- `get_status_races_death` (PCT, DFS): status against stop, kill, panic and
  a timeout; one outcome; never inside a turn.
- `fenced_kill_races_turn_end` (DFS): `check_now_with(kill)` while the
  observed turn ends, panics or is dropped.
- `reads_do_not_change_the_tape` and `console_queries_do_not_change_the_tape`
  (`console::handle_line` with `ps`, `info`, `tree`, `stuck`): identical tapes
  with and without (I1, I12).
- `kill_check_replays`: a kill-capable check replays identically for the
  same seed.
- `check_now_reports_after_virtual_time`.

### Fuzzer operations

`Info { who }`, `Snapshot`, `Tree { sup }`, `GetStatus { who, ms }`,
`WatchdogCheck { kill_after_ms }`. Oracles: `Dead` iff sends return `NoProc`;
`published` iff `wait_exit()` has returned; reasons agree; `len` 0 once
dead; counters monotonic; `GetStatus` outcome in the allowed set; no
duplicate id in a snapshot; after a committed kill no later handled message.

### Loom

`activity_slot_loom`: the exact writer and reader protocol with every field a
separate atomic, two consecutive activities, polls changing within one,
idle and dead reads (including a reader that sees `(N, closed)` twice while
activity N+1 has already stored its tag but not yet its word, which must be
retried, never accepted with N+1's tag), CAS against the end swap, a callback panic end and the
drop swap; asserts no mixed accepted read (open, idle or dead), no kill bit
lost, no commit after DEAD. `trace_flag_loom`: console enable, disable and
disconnect against activity ends.
`actor_table_loom`: insert, D-mark and removal against a sharded listing.

### Benchmark gate

- `concurrency/benches/introspection_overhead.rs` (shaped like
  `metrics_overhead`): async and threads; 1, 64, 1,024 actors; 1 and 64
  producers; send-only and request round trip.
- **Default path (I11)**: alternating A/B on fede of three builds against the
  **0.8 release**: 0.8; the branch's default features (`console` on,
  `introspection` off); the branch with `console` off. 10 pairs each;
  acceptance: no configuration slower by more than 1% with a confidence
  interval including zero.
- **Codegen**: the baseline is 0.8 **plus step 0** (the threads-mode
  consolidation changes threads handles' size and code on purpose, and is
  measured on its own against 0.8). Against that baseline, the run loop's
  dispatch and `enqueue` with `introspection` off are
  instruction-for-instruction the same (`cargo asm` diff in CI), and `size_of`
  of `ActorRef`, `Context` and envelopes is unchanged; no activity or watchdog
  symbol is linked. Total performance is still compared against 0.8.
- **Spawn churn**: start/stop of 100,000 actors with and without `console`
  (budget: at most 5% of spawn-to-join time).
- **Threads consolidation (step 0)**: before/after clone and request cost.
- **On vs off**: reported; proposed budget at most 3% on request round trip,
  5% on 64-producer send.

### trybuild

`#[actor(inspect)]` without `Inspect`; `inspect = debug` without `Debug`;
bad `stall(..)` durations; `get_state` with a non-`Send` closure or result;
an exhaustive `match` on `Lifecycle` or `Unavailable` without a wildcard.

### Planted bugs

| Bug | Caught by |
|---|---|
| Check-then-act kill / a failed CAS that still kills | `fenced_kill_races_turn_end`, Loom, trace control |
| End store instead of swap (kill bit lost) | same |
| Panic path or drop path ignores the kill bit | `fenced_kill_beats_handler_panic_*`, `fenced_kill_then_runtime_drop` |
| No `fence(Release)` at begin / reader without the second load | `activity_slot_loom` |
| `poll` split back into two words | `activity_fields_are_from_one_turn`, Loom |
| Stamp 0 read as process uptime | `lazy_clock_*` |
| Discards not subtracted from `len` | `mailbox_counts_every_transition` |
| Table entry removed at D, or kept after publication | `d_to_publication_interval`, leak tests |
| Status via `InTurnEnvelope` (panic ends actor) | `callback_panic_*` |
| Abandoned status callback still runs | `status_skipped_when_caller_gave_up` |
| `tree()` holds the parent lock while expanding | `tree_holds_no_lock_across_children` |
| Console binds on bare `.start()` / unlinks without the pid check | `starts_with_actor_system_*`, `forked_child_*` |
| Stale cleanup without the lock | `stale_pair_removed_only_*` |
| Blocking write without a deadline | `slow_reader_*` |
| Unordered shard iteration | `reads_do_not_change_the_tape` |
| Idle read returned before the second word load | `idle_and_dead_reads_validate_the_tag`, Loom |
| Tag without its turn stamp (or an idle read not checking it) | `activity_slot_loom` (tag of N+1 before word of N+1) |
| Trace flag stored with the activity word | `trace_sessions_cannot_overwrite_each_other`, `trace_flag_loom` |
| Socket bound before the lock is taken | `publication_orders_lock_before_socket` |
| Cleaner removing without the `fstat == lstat` recheck | `two_cleaners_and_a_starting_server` |
| Lock released with `unlock()` in a forked child | `fork_without_exec_child_never_unlocks_or_unlinks` |
| Console thread calling the subscriber directly | `blocked_audit_delivery` |
| `terminate` checked only in the console | `terminate_expect_is_fenced_at_the_supervisor` |
| Sorted `ps` capturing every key | `topk_ps_and_memory_budget` |
| Group listing keeping published actors | `group_listing_excludes_published_with_draining_helper` |
| Status permit released on timeout or disconnect | `wedged_actor_state_requests_are_bounded` |

### Coverage-matrix rows

| Id | Strongest checks | Modes |
|---|---|---|
| I1 | `info_returns_during_a_wedged_turn`; tape neutrality | A B T N G TG P, Sim |
| I2 | `activity_slot_loom`; `activity_*`; `lazy_clock_*` | A B T N G TG, Loom |
| I3 | `monotonic_*`; `d_to_publication_interval`; fuzz | A B T N G TG, Fz |
| I4 | `status_*`; `get_status_races_death`; model | A B T N G TG P, Sim, Model |
| I5 | redaction tests; trybuild | A N |
| I6 | `listing_under_churn`; `actor_table_loom` | A G N, Loom, Fz |
| I7 | `tree_*`; `snapshot_during_restarts` | G TG, Sim |
| I8 | `watchdog::*` | A G (D1), all (D2, D4), G (D3), Sim |
| I9 | `fenced_kill_*`; model; Loom; trace controls | A G N, Sim, Loom, Model |
| I10 | `reports::*`; `sink::*` | A N |
| I11 | benchmark gate; codegen | A N |
| I12 | tape tests; `kill_check_replays` | Sim |
| I13 | `console::*` | A N G (Linux, macOS) |

## Client mapping

What an operator would have seen with v1 (`introspection` and the watchdog
on, unless stated).

### Ethrex `d958dcba4`: `GetCells` served synchronously on every runtime worker

An eth/72 `GetCells` request could name 256 hashes, each expanding all blob
cells, "all of it ran synchronously on the peer connection task, so a few
concurrent requests could occupy every runtime worker". The fix bounded the
work and moved it to `spawn_blocking`.

From the watchdog's threads, within about 300 ms:

```text
WARN spawned::starvation group="main" delay_ms=1840 workers=8 busy=8
     occupants="RLPxConnection(peer 0x3a1f…)/GetCells poll_ms=1910; …"
WARN spawned::stall detector="long_poll" actor_type="…::RLPxConnection" label="peer 0x3a1f… 203.0.113.7"
     kind="message" message="…::GetCells" observed_ms=1900 group="main"
```

Attached with `spawned-ctl` (the node uses an `ActorSystem`, so the console
is on; its thread is not a runtime worker):

```text
$ spawned-ctl list
PID     NONCE  BINARY  SPAWNED  ACTORS  LIVE
41237   9f2c   ethrex  0.9.0    10412   yes
$ spawned-ctl attach 41237
spawned 0.9.0, ethrex pid 41237, read-only, features console+introspection, watchdog on
> stuck
GROUP main: probe late 1840 ms, 8/8 workers in a poll
  ACTOR   TYPE            LABEL                     KIND     MESSAGE   OBSERVED
  88123   RLPxConnection  peer 0x3a1f… 203.0.113.7  message  GetCells  1.9 s (in poll)
  90311   RLPxConnection  peer 0x92c0… 198.51.100.4 message  GetCells  1.7 s (in poll)
  …6 more
> ps --sort mailbox --limit 3
  ACTOR   TYPE             STATE    MAILBOX  MESSAGE   TURN AGE
  12      PeerTableServer  Idle     4113     -         -
  15      RpcHandler       Idle     2380     -         -
  88123   RLPxConnection   InTurn   41       GetCells  1.9 s
```

Every worker is inside one poll of `GetCells` from a connection actor, and
idle actors' queues grow behind them: synchronous CPU in that handler. A
default build without `introspection` still shows, from tier 0, eight
`RLPxConnection` actors `InTurn` with their labels in `ps`, and `stuck`
answers that turn ages need the `introspection` feature.

### Ethrex `535647e45`: L2 imports deadlocked at 0% CPU, RPC and logging stopped

Each L2 connection ran `add_block_pipeline` on a Tokio worker, parking 17
jobs on the shared 17-thread merkle pool; concurrent imports split the pool
and none finished.

```text
WARN spawned::starvation group="main" delay_ms=30000 busy=4 occupants="L2Connection(peer …)/NewBlock poll_ms=31200; ×4"
dump: /var/lib/ethrex/spawned/2026-10-23T14:02:11.json   (written by the watchdog's delivery thread)
```

Four connection actors in one poll of the same message for 30 s at 0% CPU:
blocking imports waiting on a shared resource. If logging stops because the
subscriber writes through a runtime task, the delivery thread's dump file and
`spawned-ctl stuck` (on its own thread) still answer.

### Ethlambda: the chain actor moved to its own thread

The chain actor's long synchronous handlers starved P2P and HTTP on the
shared runtime; it now has a one-thread `thread_group`. On the shared runtime
v1 would have shown starvation naming `BlockChain/NewBlock poll_ms=790` and
the P2P actor's mailbox climbing. After the move D1 does not apply, and a
per-type turn limit catches an import that would miss the 4 s slot:

```rust
#[actor(stall(long_turn = "3s"))]
impl BlockChain { … }
```

Ethlambda's shutdown overrun (`1d7902da`, 14.3 s) shows as kind `Stopped`
observed for 9.8 s in `info()` and `ShutdownReport`.

### Two smaller cases

- Ethrex's health check by ordinary request (which times out while a busy
  actor works): `info()` answers without queueing.
- Ethrex `63167661a` (`get_best_peer` timeout under mailbox pressure): D4
  reports `PeerTableServer` crossing its threshold, with its current message.

## Coordination with other 0.9 work

- **Dynamic supervisor (PR #245):** adopt `ChildInfo` (with `ChildKey`) and
  `ChildCounts`; add `tree()`; a slow `terminate_child` shows as
  `SupervisorTree::busy`; console `terminate` maps to `terminate_slot`.
- **Unified start:** one spawn path registers every actor; labels, `Inspect`
  and stall limits may become start options.
- **0.8 (PR #240):** read only; nothing here changes its semantics.
- **0.10:** full `sys:trace`, flight recorder and rank 5c extend the sink;
  turn abort at an await, if pursued, gets its own design and model.
- **Rank 7:** wait-for edges become an `ActorInfo` field. **Rank 8a:** shares
  the poll word.

## Implementation order

0. Threads mode onto `Arc<ActorShared<A>>`, measured on its own.
1. Tier 0: metadata and labels, `info()`, trees with the shared types, crash
   and supervisor events, `StatusEnvelope`, `get_state`/`get_status`,
   `Inspect`, `ActorError::CallbackFailed`.
2. Tables (`console` or `introspection`), `processes()`, `snapshot()`, wire
   structs; tier 1 activity slot, interner, clock, mailbox counters; fold
   `diagnostics`; benchmark gate and codegen check.
3. Watchdog: sampling and delivery threads, D1–D4, reports, sessions,
   metrics, dumps, `check_now`; remove `WarnOnBlocking`.
4. Fenced kill, `activity_slot_loom`, trace events, `TraceIntrospection.tla`
   and the recorded-trace controls.
5. Console: lifecycle and files, protocol v1, commands, limits,
   `tools/spawned-ctl`, console turn trace; console tests.
6. Examples (`introspection`, `stall_watchdog`, `console`, both modes),
   guide, semantics section 13, coverage rows.

## Review resolutions

### Round one (revision 2)

Each finding was treated as a claim and checked against the design and the
0.8 code. "Accepted" means the claim held and the design changed.

| Finding | Claim | Verdict | Resolution |
|---|---|---|---|
| Codex 1, agy P1-1 | The activity reader can accept a torn read: wrong orderings, and `poll`/`poll_t` change within a turn while `word` stays fixed | Accepted | `poll` packed into one word; writer `fence(Release)` before the payload, Release store of the word, AcqRel end swap; reader Acquire load, payload, `fence(Acquire)`, second load; bounded retries then `ReadContended`; `activity_slot_loom` with separate field atomics ([protocol](#the-activity-word-and-the-kill-protocol)) |
| Codex 2, agy P0-1 | The fenced kill is incomplete: panics, hooks, runtime drop, D, failed CAS; `stopped()` contradiction | Accepted | Every activity opens a turn id and every end path consumes the kill bit; kill dominates `Panic`; runtime drop swaps the word to DEAD and consumes the bit before the reason; D closes the word; a failed CAS does nothing; a kill during `stopped()` lets it finish with `Killed`. Model revision 2 includes failed CAS, handler and callback panics, hooks, drop and D, with 13 controls |
| Codex 3, agy P1-2 | A lazy or delayed coarse clock gives false ages and could kill | Accepted | Stamp 0 = `ClockNotStarted`; ages are estimates with `clock_lag`; kill eligibility uses only the watchdog's own observed duration; `lazy_clock_*` test |
| Codex 4, agy P1-3 | Tier 0 turn ids are not free; duplicated with tier 1; hooks missing | Accepted | No turn ids in tier 0; 0.8's phase word unchanged; one tier 1 word with the turn id, covering hooks and status; the end swap is listed in the cost table; the gate compares against 0.8 |
| Codex 5, agy P1-10 | D and publication are conflated | Accepted | Tables hold entries until publication; `Lifecycle::Dead { published }`; `is_alive()` false from D; watchdog ineligible after D; crash report at D; fuzz oracle and planted bug corrected; `d_to_publication_interval` test |
| Codex 6, agy P1-9 | Per-supervisor atomic snapshots don't follow from the locks; `tree()` lock-order hazard | Accepted | Promise weakened to self-consistent rows and counts derived from them; `tree()` copies rows and instance-bound handles, releases all locks, then expands; generation-mismatch shows `Dead`/`Unavailable`; lock-order test. A coherent published view is listed as a deferred alternative |
| Codex 7 | Mailbox and report accounting miss transitions; `stopped()` overwrites the last message | Accepted | `accepted`/`dequeued`/`discarded` with every transition specified; frozen at D; `tag` written only by message turns; stall end reports an observed range |
| Codex 8 | "Actor unaffected" overpromises; skipping abandoned callbacks changes R4; `InTurnEnvelope` treats panics as handler panics | Accepted | Dedicated `StatusEnvelope`; "a caught panic alone does not end the actor, no rollback"; the skip is a documented R4 exception for status envelopes with the slot's arbitration as linearization point; tests for `ReplyTo` drop in a caught panic, status during `stopped()` and destruction |
| agy P1-8 | `get_state` vs 0.8 reply slots, turn id, metrics | Accepted | Immediate slot with the caller's deadline, no credit, no deferral; own turn id and kind `Status`; `StatusReport.info` sampled before the status activity; bypasses `metrics` mailbox/request accounting both ways; `metrics` match arms gain `CallbackFailed` |
| Codex 9, agy P1-7 | Model invariants weaker than described; `gaveUp` unused; no failed CAS or panics | Accepted | Status is a real turn; commit recorded separately; `NoChargeFinishedTurn` checks the charged turn had not ended; response counts, deadline skip, panics, drop, D and publication modelled ([results](#tla-model-and-trace-validation)) |
| Codex 10, agy P1-7 | Trace mapping not concrete | Accepted | Event table with fields, actions and linearization points; CAS and record never merged; required suites; recorded-trace controls |
| Codex 11 | A kill-capable `check_now()` cannot be tape-neutral | Accepted | Neutrality only for reads and report-only checks; kill-capable checks promise identical replay (`kill_check_replays`) |
| Codex 12, agy P1-4 | Changing the umask is unsafe and racy; leaf-only checks | Accepted | Umask never touched; private `0700` directory, bind inside, `0600` as defence in depth, effective-uid peer check; ancestor and symlink checks; root refused unless the process is root |
| Codex 13, agy P1-5 | Pid-based stale cleanup and system-owned lifetime are unsafe | Accepted | Instance nonce, a held `File::lock` as liveness, removal only with the lock and a `(dev, ino)` match, the server removes only its own files; refcounted lifetime independent of any one system |
| agy P0-2 | Console on bare `.start()`, `atexit`, multiple systems, `fork()` | Accepted (coordinator decision 1) | Automatic only on `ActorSystem::new`; `console::start` guard for bare programs; refcount 0→1 bind, 1→0 unlink, rebind later; `bound_pid` check; no `atexit`, no `unsafe`; failures warn once |
| Codex 14, agy P1-6 | Console and watchdog can block the control plane | Accepted | Own console thread; per-frame 2 s write deadline; limits table (connections, requests, rows, bytes, depth, strings, rate); `state` awaited, not blocking; threads-only builds use a thread per connection; watchdog sampling separate from delivery; one outstanding probe per target |
| Codex 15 | `restart` is misleading and unfenced | Accepted (coordinator decision 3) | Renamed `kill`; consequences per supervisor policy documented; targets carry system id and an expected `id@generation`; `stale_target`; `kill`, `terminate`, `trace` allowed separately |
| Codex 16 | Wire and data contracts not frozen | Accepted | [Wire and data contract](#wire-and-data-contract): framing, envelope, error codes, encodings, `Unavailable` values, enums, evolution, cursors; `SystemId` in snapshots; `#[non_exhaustive]` on every public type; `serde_json` with dedicated wire structs |
| Codex 17, agy P2-2 | OTP claims too strong; contradictory scope text | Accepted | Alignment table distinguishes adaptations (lifecycle vocabulary, stronger redaction, one `CallbackFailed`); alternatives reconciled with the console decision; `get_status` timeout symmetry; `name` only for registry names; `monitors` direction defined |
| agy P1-11 | Threads mode has no `ActorShared` | Accepted | Step 0 consolidates threads mode onto `Arc<ActorShared<A>>`, measured on its own |
| agy P2-1 | Hash iteration breaks simulator determinism | Accepted | Ordered shards (`BTreeMap`, shard by `id & 63`), outputs sorted by `ActorId` |
| agy P2-3 | Four missing tests | Accepted | `fenced_kill_beats_handler_panic_and_self_stop`, `get_state_runs_while_deferred_reply_is_outstanding`, `slow_reader_does_not_wedge_control_plane`, `lazy_clock_started_mid_turn_does_not_false_kill` added |
| Codex test-plan audit | Missing failure-sensitive checks; three inconsistent tests | Accepted | Rows added for every listed gap; "tier 1 fields used without the feature" replaced by `Unavailable(Feature)` checks; "no introspection symbol" narrowed to activity/watchdog symbols; the D/publication oracle fixed |
| Codex Q9 | Use `serde_json` | Accepted | Under `console`, with wire structs independent of Rust types |
| Codex Q13 | Defer the console turn trace | Partly rejected | The user requires `trace` in the console; kept, but only as a bounded turn trace with the limits above, gated separately, tier 1; full `sys:trace` stays 0.10 |
| Codex Q5 | Per-system ownership with a process facade | Partly accepted | Tables are process-wide (bare actors have no system) but every entry carries its `SystemId`, and listings can filter by system; per-system watchdog scoping is open question 2 |

### Round two (revision 3)

| Finding | Claim | Verdict | Resolution |
|---|---|---|---|
| Codex F1, agy P1-3 | A pid check does not make the console fork-safe: inherited mutexes, guards and descriptors, and `flock` is shared across `fork()` | Accepted | v1 supports `fork()` followed by `exec` or `_exit` only; every console descriptor is close-on-exec; a child must not use, start or drop inherited systems or guards; the automatic post-fork reset is removed; the lock is released only by closing (never `unlock()`); a forked child that keeps running shows as `held, not answering`, never deleted ([`fork()`](#lifecycle)) |
| Codex F2, agy P1-3 | Binding before locking lets a cleaner delete a new socket; cleaning needs identity checks | Accepted | Publication: exclusive create, `try_lock`, `fstat == lstat` and owner checks, then bind; withdrawal removes the socket, then the lock file, then closes; cleaners act only while holding the lock and after re-verifying; interleavings listed and tested ([files](#files-permissions-and-stale-sockets)) |
| Codex F3, agy P2-3 | Limits do not bound retained memory or synchronous work | Accepted | Sorted `ps` is top-k (O(k) memory); id-order pages by keyset with no server-side cursor; work slices of 1,000 actors or 5 ms with yields; per-connection 8 MiB and process 32 MiB budgets checked before allocation; `state` at most 30 s; formatter allocation explicitly outside the guarantee |
| Codex F4 | Blocking delivery and audit logging have undefined shutdown | Accepted | The watchdog joins only its sampling thread; the delivery thread is detached, at most one may be stuck (`DeliveryStuck`); console audit goes through a bounded channel to a delivery thread, drops are counted, mutations are refused while their record cannot be queued |
| Codex F5, agy P1-1 | The model misses hook panics, checks commitments late, and uses fault-driven ghosts; `Committed` can reset | Accepted | `StartPanic` and `HookPanic`; `Committed == commitPreD`; `CommitOnOpen` and `NoChargeFinishedTurn` checked at the commitment; `KillSkipsStopped` against prior commitments (the round-two counterexample is now `LostBitRunsStopped`); `StatusReasonIsFinal`; every ghost derived in `Next` from the step's state change; 18 controls |
| Codex F6, agy P1-2 | Trace events do not refine the atomic actions (`BeginExit`, status answer, drop then D, async hooks) | Accepted | The model's actions are split to the code's linearization points: `ExitDecide`/`HookBegin`, `StatusAnswer`/`StatusEnd`, `DropSwap`/`Die` (with an ordinary kill possible between), `Suspend` for async hooks; one event per action; `ICas` carries the returned word; coverage excludes fault-only actions |
| Codex F7 | The trace flag inside the activity word has an unspecified multi-writer protocol | Accepted | The flag moves to its own `AtomicU32` owned by the console (CAS by session id), read with one relaxed load at activity end; cost table updated; `trace_flag_loom` |
| Codex F8 | `terminate --expect` is not fenced at the supervisor | Accepted | The expectation is passed into the supervisor's operation and checked inside its admission; `stale_target` otherwise; coordinated with PR #245 (`terminate_slot` gains `expect`) |
| Codex F9 | Post-push accounting is not frozen at D | Accepted (weakened) | `len` reads 0 from D; counters are final once every send that began before D has returned; tested with a failpoint between push and increment. A sharp freeze would cost a second RMW per send |
| Codex F10, agy P2-2 | Group listings retain published actors whose helpers drain | Accepted | Published actors are excluded; `GroupSnapshot.helpers_draining` counts them; no table write at D |
| Codex F11 | Contradictions: status during destruction, publication, console neutrality, large counts, codegen baseline | Accepted | Status sent after D gets `NoProc`; publication = `wait_exit()` ready; I1/I12 cover read commands only; every `u64` is a string on the wire; codegen compares against 0.8 plus step 0, total performance against 0.8 |
| Codex F12, agy P2-1 | Idle reads skip validation; finite counters can wrap | Accepted | Idle and dead reads use the same second-load validation; DEAD keeps the turn id; turn ids saturate (never reused); D1 identifies a poll by turn, count and stamp; reduced-width tests |
| agy P2-3 | `check_now` lacks system scope | Accepted | `check_now_for(SystemId)`; observation keyed by `ActorId`, limits and reports per system |
| Both, on the three revision-2 disagreements | Bounded console `trace` in v1, process tables with system ids, a dedicated console thread | Kept (endorsed by both reviews) | Codex's condition that system ids also scope watchdog state is met by `check_now_for` and per-system limits |

### Round three (confirmation)

| Finding | Claim | Verdict | Resolution |
|---|---|---|---|
| agy P1-1 | An idle read can accept `w1 == w2 == (N, closed)` with activity N+1's tag, stored before N+1's word | Accepted | `tag` becomes a 64-bit word stamped with the low 32 bits of its turn id (one relaxed store, unchanged cost); a read whose tag is ahead of `w1` is retried; the exact interleaving is added to `activity_slot_loom` and to the planted bugs. Not modelled in TLA+: the model's steps are atomic and sequentially consistent and reads are not actions, so a torn read cannot be expressed there. The bug is a memory-ordering property of separate atomics, which is what Loom checks against the C11 model |
| Codex r3 #1 | Idle validation still accepts a mixed activity (writer paused after payload writes, before open publication) | Accepted, same fix as agy P1-1 | In that schedule N+1's tag carries turn N+1, which is ahead of `w1 = (N, closed)`, so the read is retried, not accepted. Idle reads return only the turn-stamped tag, never `since` or `poll`, so no unstamped mutable field reaches an idle result. The Loom case pauses the writer after all payload stores and before the open store |
| Codex r3 #3 | Panic containment is checked only at the status turn's end; a panic recorded while the callback runs passes | Accepted | The ghost now fires on any step taken while a status turn is open that records a panic. New control `AnswerPanicStops` records it at the answer, and `CallbackPanicStops` is kept at the end; both fail `CallbackPanicContained` (re-run on fede) |
| Codex r3 #4 | Status callback admission has no linearization point of its own (merged with dispatch), so a valid trace in which the slot closes after dispatch but before the check is rejected | Accepted | `StatusAdmit` is a model step after `Dispatch` and before `StatusAnswer`/`StatusEnd`, with its own trace event `IStatusAdmit`; the slot can close on either side. New control `RunAfterSkip` (the callback runs after a skip decision) fails `NoCallbackAfterGiveUp`, as `CallbackAfterGiveUp` (admission ignores a closed slot) does; required trace suites cover expiry before dispatch, between dispatch and admission and after admission, and a recorded-trace control rejects an answer after a skip |
| Codex r3 #2 | Cancelled console `state` requests can accumulate in a wedged actor's mailbox | Accepted | Console status permits, at most 2 per target actor, 32 per process and 1 per connection, held by the envelope until it is dequeued or discarded at D, never released on timeout or disconnect; `busy` otherwise; `wedged_actor_state_requests_are_bounded` and a planted bug. Application `get_status` calls are ordinary requests and are not limited, as `request` is not |

## Open questions for reviewers

1. **`last_message` in default builds** stays tier 1 (both reviews). An
   exit-only extraction from the failing envelope, with no work on returning
   turns, could make it tier 0 later; worth a follow-up?
2. **Watchdog per process or per `ActorSystem`** (Codex Q5): v1 has one
   process watchdog with per-group and per-ChildSpec limits; multi-node
   simulations use `check_now` per system.
3. **A coherent published supervisor view** (Codex 6, option b) for 0.10, if
   operators need instant tree levels.
4. **Console thread in the default build**: one idle OS thread per process
   with an `ActorSystem`. Acceptable, or should the default build open the
   socket but serve it from the first system's control runtime?
5. **Default thresholds**: D1 100 ms, D2 10 s, D4 10,000/1,000 (both
   reviews: reasonable for report-only).
6. **`mailbox_len` vs the `metrics` gauge**: introspection counts every
   envelope, metrics counts application messages (Codex Q7: keep both,
   documented).
