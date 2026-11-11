# Introspection v1 (0.9 design, in review)

Status: design only, not implemented. Priority tier 2 of the
[roadmap](../../ROADMAP.md#priorities): ranks 8 (stall watchdog), 30
(read-only inspection, `spawned-top`), 29 (metrics), with 9 (flight recorder)
and 28 (causal tracing) placed explicitly. Builds on 0.8 (PR #239 deferred
replies, PR #240 OTP exit signals): it reads the reply registry's turn phase,
the death point D and `ExitReason::Killed`. Model:
[`Introspection.tla`](../../verification/tla/Introspection.tla).

The question this design must answer in minutes on a live node:

> Which actor is stuck, on what message, for how long, with how much queued,
> and what is it linked to?

Prior art and sources: [observability](../research/observability.md),
[deadlock and stall detection](../research/deadlock-and-stall-detection.md),
and the OTP/Elixir/Gleam comparison done for 0.9 (gaps G3, G4, G5, G9 and its
section 3.3), whose suggestions are folded in below
([decisions](#decisions-at-a-glance) lists what was accepted and rejected).

## Contents

1. [Decisions at a glance](#decisions-at-a-glance)
2. [Tiers and the opt-in mechanism](#tiers-and-the-opt-in-mechanism)
3. [API](#api)
4. [How the fields are kept](#how-the-fields-are-kept)
5. [Cost table](#cost-table)
6. [Consistency guarantees and non-guarantees](#consistency-guarantees-and-non-guarantees)
7. [The stall watchdog](#the-stall-watchdog)
8. [Determinism and the simulator](#determinism-and-the-simulator)
9. [Access paths and the `spawned-top` data model](#access-paths-and-the-spawned-top-data-model)
10. [Remote console (`spawned-ctl`)](#remote-console-spawned-ctl)
11. [OTP alignment](#otp-alignment)
12. [Alternatives considered](#alternatives-considered)
13. [Semantics contract: guarantees added](#semantics-contract-guarantees-added)
14. [TLA+ model](#tla-model)
15. [Test plan](#test-plan)
16. [Client mapping](#client-mapping)
17. [Coordination with other 0.9 work](#coordination-with-other-09-work)
18. [Implementation order](#implementation-order)
19. [Open questions for reviewers](#open-questions-for-reviewers)

## Decisions at a glance

| # | Decision | Why |
|---|---|---|
| 1 | **Two kinds of access.** `info()` (OTP `process_info`) is out of band: it reads shared atomics and tables and works while a handler is wedged. `get_state`/`get_status` (OTP `sys`) are in band: a request answered between turns, in mailbox order, which times out on a stuck actor, as in OTP. | The stuck case cannot be answered from inside the stuck turn, and the handler future holds `&mut A` across awaits. OTP and recon draw the same line. |
| 2 | **Tier 0 is free and always on**: identity, type, label, lifecycle state, a per-actor turn id, links, monitors, `trap_exit`, parent, supervisor position, placement, deferred replies and jobs, supervision trees, structured crash and supervisor reports. All read state that already exists or is written once at spawn. The turn id rides in 0.8's turn-phase word, which the run loop already stores at every turn begin and swaps at every end: the store gets wider, not more frequent. | Nothing is added on the send, dispatch or turn path, yet a default build can say "this actor has been in the same turn for at least T". |
| 3 | **Tier 1 is the `introspection` Cargo feature**: current message type, turn age, poll age, mailbox length, per-actor counters, the stall watchdog, per-actor tracing. Compiled out without it. With it, a turn costs a few relaxed stores and no clock read. | The fields that diagnose a stall need a store per turn and an RMW per send; the rule allows that only opt-in. |
| 4 | **No clock read per turn.** Turns stamp a process-wide coarse clock (one relaxed load) that a lazily started thread advances every 5 ms; ages are exact to one tick. Stall detection fences on a per-actor turn id, as Go's `sysmon` checks `schedtick`. | `Instant::now()` twice per turn and per poll is the dominant cost of today's `diagnostics` and `WarnOnBlocking`. |
| 5 | **The watchdog is an OS thread**, started at run time (`Watchdog::start`), never a Tokio task. Four detectors: a poll blocking a shared async worker (OTP `long_schedule`), a turn open too long across awaits, a starved execution group (Akka probe), a long mailbox (OTP 26 `long_message_queue`). | It must keep reporting when every runtime worker is occupied, which is exactly ethrex `d958dcba4` and `535647e45`. |
| 6 | **Policy: `Report` by default; `Kill` opt-in per actor type, ChildSpec or group, fenced.** A watchdog kill is a CAS on the actor's activity word naming the observed turn; a turn that already ended is never charged. The kill is an ordinary `Killed` exit: L3 is unchanged, so it takes effect when the turn returns. | OTP-aligned (`Killed`, no new reason); no false kills of a recovered actor (TLA+ `FencedKill`, two negative controls). |
| 7 | **Turn abort at an await is not in v1.** | Per-suspended-turn cost, a change to L3, and the comparison's "no `brutal_kill` emulation" rule; analysed in [alternatives](#alternatives-considered) as a separate item. |
| 8 | **Flight recorder (rank 9) deferred to 0.10.** v1 ships the shared event sink (bounded, isolated sessions, counted gaps) and, because the console needs it, only a minimal per-actor `trace` (one event per turn of a traced actor, rate-limited, auto-stopping, tier 1). Full `sys:trace` semantics (messages out, lifecycle events, filters) stay in 0.10 (G9). | None of the client incidents needed history; the current turn, the last message in crash reports and the watchdog cover them. The recorder and full tracing are the same per-turn ring and should be designed once with rank 5c's production sink. |
| 9 | **`get_state(f)` is typed and available on every actor; the type-erased status (`get_status` through `ChildHandle`, dumps, `spawned-top`) shows state only for actors that opt into `Inspect`, redacted by default** (OTP 25 `format_status`). | A typed closure is ordinary code bound by Rust privacy; dumps reach operators and logs, so peer keys must not leak by default. |
| 10 | **`which_children`/`count_children`/`tree()` on every supervisor**, with one `ChildInfo`/`ChildCounts` shared with the dynamic supervisor (PR #245). | `spawned-top` walks one tree shape. |
| 11 | **Labels** (`ctx.set_label`, OTP 27 `proc_lib:set_label`), set rarely, read by every report. | 10,000 `RLPxConnection`s are readable only by peer. |
| 12 | **Structured `tracing` events with stable names** for crashes, supervisor actions and stalls. | The cheapest high-value piece; exporters can rely on the names (1.0 stability promise). |
| 13 | Out of scope: `sys:replace_state`, `suspend`/`resume`, `change_code`. | They mutate state outside a turn or do not apply to Rust. |
| 14 | The `diagnostics` feature is folded into `introspection` (kept one release as a deprecated alias). `WarnOnBlocking` is removed when detector 1 lands. | One instrumentation point, cheaper than both. |
| 15 | **Every binary is attachable**: a remote console on a local Unix socket, on by default (Cargo feature `console` in `default`), read-only by default, with opt-in mutations, no TCP and no eval; `spawned-ctl` as the client. Its default cost is one idle listener off the application workers and the process-wide actor table (insert at spawn, remove at exit). | `erl -remsh` on every node is what makes BEAM operable; the cost stays off send, dispatch and turn. |

## Tiers and the opt-in mechanism

| Level | How it is enabled | What it adds | Cost when not enabled |
|---|---|---|---|
| Tier 0 | Always | `info()` minus tier 1 fields, labels, listing of managed actors, supervision trees, `get_state`/`get_status`, structured crash and supervisor events | None on send, dispatch or turn; a few words per actor written at spawn |
| Console | `console` feature, **in `default`**; `SPAWNED_CONSOLE=off` at run time | The process-wide actor and system tables (so `processes()` and `snapshot()` cover bare actors), `render_json`, the Unix-socket listener | Feature off: compiled out. Run-time off: the table is still kept (spawn/exit), no listener |
| Tier 1 | `features = ["introspection"]` | Activity slot (message type, turn and poll age), mailbox counters, the actor tables (also without `console`), the watchdog, per-actor `trace` | Compiled out entirely: no field, no store, no symbol |
| Watchdog | `Watchdog::start(config)` at run time (tier 1) | One OS thread sampling the tables; probe tasks per group | Not started: nothing runs |
| Per actor type | Associated consts set by `#[actor(...)]` | `INSPECT` (state formatter), `STALL` (limits and kill policy) | Constants, folded at compile time |
| Per ChildSpec / group | `ChildSpec::stall(..)`, `async_group(..).stall(..)` | Overrides of `STALL` | Read only by the watchdog |

Precedence for stall limits: ChildSpec, then execution group, then the actor
type's constant, then the watchdog's defaults.

## API

Tasks and threads expose the same names. Reads return immediately in both
modes. In-band calls return `Response<T>` (tasks: `.await`; threads: blocking),
as requests do. Types live in `spawned_concurrency::introspection` and are
shared by both modes.

### Per-actor info (`process_info`)

```rust
// Both modes. Tier 0 unless marked.
impl<A: Actor> ActorRef<A> { pub fn info(&self) -> ActorInfo; }
impl<A: Actor> Context<A>  { pub fn info(&self) -> ActorInfo; pub fn set_label(&self, label: impl Into<Arc<str>>); }
impl ChildHandle           { pub fn info(&self) -> ActorInfo; pub fn links(&self) -> Vec<ActorId>; pub fn monitors(&self) -> Vec<ActorId>; }

#[cfg(any(feature = "console", feature = "introspection"))]
pub fn introspection::info(id: ActorId) -> Option<ActorInfo>;   // None: unknown or past cleanup (OTP: undefined)

#[non_exhaustive]
pub struct ActorInfo {
    pub id: ActorId,
    pub actor_type: &'static str,          // type_name::<A>()  (OTP initial_call)
    pub label: Option<Arc<str>>,           // ctx.set_label     (OTP 27 proc_lib:set_label)
    pub name: Option<String>,              // first registered name (OTP registered_name)
    pub lifecycle: Lifecycle,
    pub trap_exit: bool,
    pub links: usize,                      // list: ChildHandle::links()
    pub monitors: usize,                   // outgoing monitors this actor holds
    pub parent: Parent,
    pub supervisor: Option<SupervisorPosition>,
    pub placement: Placement,
    pub deferred_outstanding: usize,       // live ReplyTo tokens + linked jobs (0.8 credits)
    pub jobs_running: usize,
    pub turns: u64,                        // dispatched turns so far, from the turn id (OTP reductions, roughly)
    pub in_turn_since: Option<TurnId>,     // the open turn's id; two reads with the same id = the same turn
    // Tier 1 (`introspection`); None without the feature.
    pub activity: Option<Activity>,
    pub mailbox_len: Option<u64>,
    pub messages_in: Option<u64>,          // accepted envelopes, monotonic
}

pub enum Lifecycle { Starting, Idle, InTurn, Exiting, Dead(ExitReason) }   // OTP status
pub struct Activity {
    pub kind: ActivityKind,                // Started | Message | ExitReceived | Status | Stopped
    pub message: Option<&'static str>,     // type_name of the message (Message, ExitReceived)
    pub turn: TurnId,                      // per-actor sequence; fences stall reports and kills
    pub age: Duration,                     // exact to one clock tick
    pub poll: Option<Duration>,            // Some: inside a poll for this long; None: suspended at an await
    pub kill_pending: bool,                // a fenced kill is waiting for this turn to return
}
pub enum Parent { None, Actor(ActorId), Group(String), Supervisor(SupervisorPath) }
pub struct SupervisorPosition { pub path: SupervisorPath, pub child: ChildKey, pub generation: u64, pub restarts: u64 }
pub struct Placement { pub backend: BackendKind, pub group: Option<String> }  // Async | Blocking | Thread | Native | Pinned(shard)
```

`Lifecycle` is derived from state 0.8 already keeps: phase 0 of the exit
request (`Starting`), the reply registry's turn phase (`Idle`/`InTurn`, and
`Exiting` from the exiting phase), and the death point's final reason
(`Dead`). `Exiting` covers `stopped()` and the cleanup before D; the activity
kind `Stopped` says `stopped()` itself is running.

### State and status (`sys:get_state`, `sys:get_status`)

```rust
impl<A: Actor> ActorRef<A> {
    /// OTP sys:get_state. Runs `f` on the state in a turn of its own,
    /// between handler turns, in mailbox order. Default timeout 5 s.
    pub fn get_state<R, F>(&self, f: F) -> Response<R>
    where F: FnOnce(&A) -> R + Send + 'static, R: Send + 'static;
    pub fn get_status(&self) -> Response<StatusReport>;
}
impl ChildHandle {
    /// Type-erased sys:get_status (spawned-top, dumps).
    pub fn get_status(&self, timeout: Duration) -> Response<StatusReport>;
}

pub struct StatusReport {
    pub info: ActorInfo,                   // sampled inside the status turn
    pub state: StateView,                  // Redacted | Formatted(String)
}

/// Opt-in, as OTP 25 format_status: without it the state is never shown.
pub trait Inspect: Actor {
    fn format_status(&self) -> String;
}
// #[actor(inspect)]          uses the Inspect impl;
// #[actor(inspect = debug)]  formats with Debug (the user accepts the exposure).
// Wired through `const FORMAT_STATUS: Option<fn(&Self) -> String> = None` on Actor.
```

Rules:

- The status turn is an envelope of the existing kind (`InTurnEnvelope`
  already runs a closure in a turn for `spawned-test`); there is no control
  lane. It queues behind the backlog (M2), runs between turns (M1), and is
  answered at the death point if still queued (R1): `Ok`, `RequestTimeout`,
  `ActorStopped(final reason)` or `NoProc`.
- A panic in `f` or `format_status` is caught and returned as the new
  `ActorError::CallbackFailed(String)` (OTP `{callback_failed, ..}` from
  `sys:replace_state`); the actor is unaffected and its next turn runs
  normally.
- If the caller already gave up (the reply slot is closed) the callback is
  skipped. Unobservable, and it keeps a stale dump from costing a `Debug`
  of a large state.
- The status turn does not count as a handled message in `metrics`, does not
  run `Handler` code and does not reset any user timer.
- On a stuck actor `get_status` times out. That is expected; the operator
  uses `info()`, which does not need the actor.

### Listing and snapshots

```rust
// Tier 0: managed actors (groups already keep their scopes).
impl ActorSystem    { pub fn actors(&self) -> Vec<ActorInfo>; pub fn snapshot(&self) -> SystemSnapshot; }
impl ExecutionGroup { pub fn actors(&self) -> Vec<ActorInfo>; pub fn snapshot(&self) -> Option<GroupSnapshot>; }

// With `console` (default) or `introspection`: every live actor in the process,
// managed or bare (OTP erlang:processes/0).
pub fn introspection::processes() -> Vec<ActorId>;
pub fn introspection::snapshot() -> Snapshot;            // all systems + unmanaged actors + watchdog state

pub struct Snapshot {
    pub schema: u32,                                     // 1
    pub taken_at: Instant, pub wall_clock: SystemTime,
    pub systems: Vec<SystemSnapshot>,
    pub unmanaged: Vec<ActorInfo>,
    pub watchdog: Option<WatchdogState>,                 // last probe delays, open stalls
}
pub struct SystemSnapshot { pub groups: Vec<GroupSnapshot>, pub supervisors: Vec<SupervisorTree> }
pub struct GroupSnapshot {
    pub name: String, pub profile: ExecutionProfile, pub worker_budget: usize,
    pub closing: bool,
    pub actors: Vec<ActorInfo>,
    pub busy_workers: Option<usize>,                     // tier 1: actors of this group inside a poll
    pub scheduling_delay: Option<Duration>,              // last watchdog probe
}
impl Snapshot {
    /// recon proc_count: top n by an attribute at this instant.
    pub fn top(&self, by: Rank, n: usize) -> Vec<&ActorInfo>;
    /// recon proc_window: top n by growth between two snapshots.
    pub fn window(&self, earlier: &Snapshot, by: Rank, n: usize) -> Vec<(ActorId, u64)>;
}
pub enum Rank { MailboxLen, TurnAge, PollAge, Turns, MessagesIn }
```

`busy_workers` counts this group's actors inside a poll: a lower bound on
occupied workers (helpers and foreign tasks are not counted). Native and
pinned groups report leased slots, which they already track. Tokio's stable
`RuntimeMetrics` (`num_workers`, `num_alive_tasks`, `global_queue_depth`) is
added to `GroupSnapshot` where available; per-worker busy time needs
`tokio_unstable` and is not used.

### Supervision trees (`which_children`, `count_children`)

```rust
impl Supervisor {                      // static; DynamicSupervisor (PR #245) gets the same three
    pub fn which_children(&self) -> Vec<ChildInfo>;
    pub fn count_children(&self) -> ChildCounts;
    pub fn tree(&self) -> SupervisorTree;  // nested supervisors expanded, workers with their ActorInfo
}

/// One type for static and dynamic children; replaces ChildSnapshot and #245's DynamicChildInfo.
pub struct ChildInfo {
    pub key: ChildKey,                 // Named(String) | Dynamic(DynamicChildId)
    pub actor: Option<ActorId>,        // last published generation (OTP: pid | restarting | undefined)
    pub ready: bool,
    pub state: ChildState,
    pub kind: ChildKind,               // Worker | Supervisor
    pub restart: RestartMode,
    pub generation: u64,
    pub restarts: u64,                 // this child's restarts (supervisor-wide budget below)
    pub last_exit: Option<ExitReason>,
    pub last_error: Option<String>,
}
pub struct ChildCounts { pub specs: usize, pub active: usize, pub supervisors: usize, pub workers: usize } // OTP, as in #245
pub struct SupervisorTree {
    pub name: String, pub strategy: SupervisorStrategy, pub phase: SupervisorPhase,
    pub budget: RestartBudget,         // used / max in the current window
    pub busy: Option<SupervisorBusy>,  // e.g. "terminate_child(id) for 12 s" (comparison 3.2.6)
    pub counts: ChildCounts,
    pub children: Vec<TreeChild>,      // { info: ChildInfo, node: Worker(Option<ActorInfo>) | Supervisor(SupervisorTree) }
}
```

`SupervisedChild::snapshot()` becomes `info()`; `Supervisor::snapshots()` is
kept one release as a deprecated alias of `which_children()`.

### Structured reports

Emitted only on exit, supervisor and watchdog paths, never per message. The
target and field names are a contract (stable from 1.0):

| Target, level | When | Fields |
|---|---|---|
| `spawned::crash`, ERROR | an actor's final reason is abnormal (`Panic`, `Kill`, `Killed`, `NoProc`), at D | `actor_id`, `actor_type`, `label`, `name`, `reason`, `ancestors` (`"sup/child#gen > sup"`), `generation`, `group`, `links`; tier 1: `last_message`, `mailbox_len`, `turn` |
| `spawned::supervisor`, INFO/WARN/ERROR | `child_terminated`, `child_restarted`, `start_failed`, `restart_intensity_reached`, `shutdown_error` | `supervisor`, `child`, `generation`, `reason`, `restarts_in_window`, `max_restarts`, `window_ms`, `action` |
| `spawned::stall`, WARN | a watchdog detector fires or a stalled turn ends | `detector`, `actor_id`, `actor_type`, `label`, `message`, `turn`, `age_ms`, `poll_ms`, `group`, `mailbox_len`, `action` (`reported`, `killed`, `kill_skipped`) |
| `spawned::starvation`, WARN | a group's probe is late | `group`, `delay_ms`, `workers`, `busy`, `occupants` (top 8 `Type(label)/Message poll_ms`) |

The existing `"Panic in message handler"` log becomes the `spawned::crash`
event; its text is kept as the message.

### Event sink and sessions

Reports also go to a bounded in-memory sink that several consumers read
independently, as OTP 27 trace sessions:

```rust
#[cfg(feature = "introspection")]
pub fn introspection::session(filter: EventFilter, capacity: usize) -> Session;
impl Session { pub fn drain(&mut self) -> (Vec<Event>, /* lost */ u64); }
```

Requirements, binding on the 0.10 work that extends it (rank 5c production
lifecycle monitors, rank 9 flight recorder, G9 `sys:trace`): bounded memory;
per-actor order preserved; each session has its own cursor and filter; a slow
session loses its own events, counted, and never blocks a producer or another
session; producing an event never allocates on a turn path. v1 producers are
the exit path, supervisors and the watchdog only.

## How the fields are kept

### Tier 0 (no feature)

| Field | Kept where | Written |
|---|---|---|
| `actor_type`, backend | `ActorShared` (one `&'static str`, one byte) | at spawn |
| `label` | `Mutex<Option<Arc<str>>>` in `ActorShared` | by `set_label` only |
| type-erased reads | `SignalTarget` gains `Weak<dyn Probe>` to the actor's shared state | at spawn |
| `Lifecycle` | 0.8 turn phase, exit request phase 0, final reason | already |
| turn id (tier 0) | 0.8's turn-phase word widened from `AtomicU8` to `AtomicU64`: `turn_id << 3 \| phase` | the same store at begin and swap at end; the run loop keeps the counter in a register. The drop-recording CASes preserve the high bits; `registry_turn_loom` is extended to cover them |
| actor and system tables | process-wide, with `console` or `introspection` | insert at spawn, remove at the death point's publication |
| links, monitors, `trap_exit`, parent | existing tables and flag | already |
| supervisor position | `Scope` gains child key, generation and restarts | at admission |
| deferred and jobs | 0.8 credit pool and job set | already |
| supervision trees | supervisor core under its own lock | already |

### Tier 1: the activity slot

One cache-aligned block per actor, written only by its run loop:

```text
word   AtomicU64  turn_id << 4 | kill << 3 | kind      store at begin, swap at end
tag    AtomicU32  interned message type                store at begin
since  AtomicU64  coarse clock at begin                load clock + store
poll   AtomicU64  poll_count << 1 | in_poll            store before and after each poll
poll_t AtomicU64  coarse clock at poll start           load clock + store
out    AtomicU64  dequeued envelopes                   store per dispatch
```

A separate cache line holds `in: AtomicU64`, incremented (`fetch_add`,
relaxed) by every accepted send, so senders do not contend with the run loop.
`mailbox_len = in - out` (saturating), a momentary value: an envelope being
pushed may be counted a moment late (as the `metrics` gauge today). It counts
every envelope: messages, requests, `Exit`, `Down`, status requests.

**Message type without `unsafe`.** The crates are `#![forbid(unsafe_code)]`,
and a `&'static str` cannot be stored in one atomic. Each run loop keeps a
small cache from the `type_name::<M>()` pointer (compared with `ptr::eq`) to
a `u32` tag from a process-wide interner (a mutex taken only on a cache miss:
once per message type per actor instance). Steady state per turn: one virtual
call returning a constant, a probe of a few entries, one relaxed store.

**Coarse clock.** `introspection::clock` is a process-wide `AtomicU64` of
milliseconds since process start, advanced every 5 ms by the
`spawned-introspect` thread. The thread starts lazily on the first tier 1
read or `Watchdog::start`; until then stamps are 0 and ages read "at least
since the clock started". Under `--cfg spawned_sim` the stamp is the virtual
clock, read directly, and there is no thread.

**Consistent activity reads.** A reader loads `word`, then `tag`, `since`,
`poll`, then `word` again, and retries (bounded, then reports `unknown`) if
`word` changed: the writer publishes `word` last at begin (Release) and
first at end. One turn's fields are never mixed with another's.

**Fenced kill.** The watchdog commits a kill with
`compare_exchange(word = (N, in turn), (N, in turn, kill))`. The run loop's
turn end is `swap(word, (N+1, idle))`; if the old value had the kill bit, it
records `Killed` before the next receive. The watchdog then also records the
kill through the ordinary path (idempotent). A turn that ended before the CAS
makes it fail: nothing is killed.

**Actor and system tables** (compiled with `console` or `introspection`). 64 shards of `RwLock<HashMap<u64, Arc<dyn
Probe>>>` keyed by `ActorId`; insert at spawn, remove at the death point's
publication. Systems register in a small list. Readers hold one shard's read
lock at a time.

## Cost table

"Turn" is one dispatched envelope; "send" is one accepted envelope.

| Field or mechanism | Tier | Send | Turn | Poll | Spawn / exit | Reader |
|---|---|---|---|---|---|---|
| id, type, backend, parent | 0 | – | – | – | 3 words stored | loads |
| label | 0 | – | – | – | – | short lock |
| lifecycle | 0 | – | – (reuses 0.8 phase store) | – | – | 1 load + exit-request lock |
| links, monitors, trap_exit | 0 | – | – | – | – | table lock, bounded copy |
| supervisor position, restarts | 0 | – | – | – | 3 words at admission | scope lock |
| deferred, jobs | 0 | – | – | – | – | loads |
| `get_state`/`get_status` | 0 | – | one status turn when called | – | – | a request |
| supervision trees | 0 | – | – | – | – | supervisor lock |
| crash / supervisor events | 0 | – | – | – | on abnormal exit | – |
| `Weak<dyn Probe>` | 0 | – | – | – | weak count +1 | upgrade |
| turn id (widened phase word) | 0 | – | same store and swap, 8 bytes instead of 1 | – | – | 1 load |
| actor and system tables | console (default) | – | – | – | 1 shard write lock at spawn, 1 at exit | shard read locks |
| console listener | console (default) | – | – | – | one idle task on the control runtime (or one OS thread, threads-only builds), created with the first system or actor | per command |
| `render_json` | console (default) | – | – | – | – | per request |
| activity: kind, turn id | 1 | – | 1 store + 1 swap | – | – | seqlock-style read |
| message type | 1 | – | virtual call, cache probe, 1 store | – | – | interner lookup |
| turn age | 1 | – | 1 load (shared, read-mostly) + 1 store | – | – | – |
| poll age, `polling` | 1 | – | – | 2 stores + 1 load + 1 store | – | – |
| `mailbox_len`, `messages_in` | 1 | 1 relaxed `fetch_add` (per-actor line) | 1 store | – | – | 2 loads |
| clock thread | 1 | – | – | – | – | wakes every 5 ms once started |
| watchdog sampling | 1 + started | – | – | – | – | O(actors) loads per sample on its own thread |
| starvation probe | 1 + started | – | – | – | – | 1 tiny task per group per second |
| fenced kill | 1 + policy | – | swap instead of store at turn end | – | – | 1 CAS when firing |
| per-actor `trace` flag | 1 | – | 1 relaxed load (activity line) | – | – | – |
| default build (`console` on, `introspection` off) | – | **nothing** | **nothing new** | **nothing** | tier 0 + table | – |
| `console` and `introspection` off | – | **nothing** | **nothing new** | **nothing** | tier 0 | – |

Expected with the feature on (to be measured, see the
[benchmark gate](#benchmark-gate)): a few nanoseconds per turn and per send,
against a request round trip of hundreds of nanoseconds. Today's
`diagnostics` takes a mutex and an `Instant::now()` per managed turn; debug
builds' `WarnOnBlocking` reads the clock twice per poll of every async actor.

## Consistency guarantees and non-guarantees

A snapshot of a live system is not atomic. What holds:

- **Per actor, per read.** Each scalar is one atomic load or one locked read.
  The activity fields (kind, message, turn, age, poll) are one consistent
  set from the same turn. Different fields of one `ActorInfo` are not from
  the same instant (a `mailbox_len` and a `Lifecycle` may straddle a turn).
- **Monotonic.** `turn`, `turns`, `messages_in`, `generation` and `restarts`
  never decrease across reads of the same actor. `Dead` is final, and an id
  never belongs to another actor (M4).
- **No ABA on stalls.** The same `TurnId` in two reads means the same turn
  was open in between. Ages are exact to one clock tick (5 ms).
- **Listings.** An actor admitted before a listing starts and not past D when
  it ends appears exactly once. Actors started or ended during it may or may
  not appear, possibly as `Dead`. No id appears twice.
- **Supervision.** Each `which_children`, `count_children` or `tree()` level
  is read under that supervisor's lock: the list and counts of one call agree
  with each other and with the supervisor's bookkeeping at one instant.
  Levels of a tree are read parent first, not at one instant. A listed actor
  may already be dead and not yet processed; a restarting child shows
  `ready: false` and its last id (OTP `restarting`).
- **No perturbation.** Out-of-band reads never enqueue, wake, block or run
  user code on the observed actor, and never wait for a turn to end.

Not guaranteed:

- an instant view across actors, groups or tree levels;
- `mailbox_len` exact under concurrent sends (it can lag a send in flight);
- that `get_status` answers a busy actor in time (it queues behind the
  backlog, as in OTP);
- detection when the watchdog thread itself is starved of CPU by the OS;
- that a watchdog kill ends a turn that never returns (L3: nothing preempts);
- memory per actor (rank 18a), message payloads, mailbox contents (recon's
  rule: never copy a mailbox), or the history of past turns (0.10).

## The stall watchdog

```rust
#[cfg(feature = "introspection")]
let watchdog = introspection::Watchdog::start(WatchdogConfig {
    sample_every: Duration::from_millis(50),
    long_poll: Some(Duration::from_millis(100)),   // shared async workers only
    long_turn: Some(Duration::from_secs(10)),
    long_mailbox: Some((10_000, 1_000)),           // enable, disable (OTP long_message_queue)
    starvation: Some(Duration::from_millis(250)),  // probe every second
    kill_after: None,                              // StallPolicy::Report
    dump_dir: None,                                // Some(dir): write render_json on each report
    ..Default::default()
})?;
watchdog.probe_runtime(tokio::runtime::Handle::current(), "main"); // unmanaged actors' runtime
// Groups of every ActorSystem are probed automatically.
let reports: Vec<StallReport> = watchdog.check_now();               // one synchronous pass (tests, sim)

#[actor(stall(long_turn = "4s", kill_after = "30s"))]               // per type; or ChildSpec::stall(..)
```

| Detector | Detects | Signal | Applies to |
|---|---|---|---|
| D1 long poll | synchronous code blocking a worker (OTP `system_monitor` `long_schedule`) | `in_poll` with the same poll count for `long_poll` | actors on shared async workers: `Backend::Async`, async groups. Not `Thread`, `Blocking`, native groups or threads mode, which own their thread |
| D2 long turn | a handler stuck across awaits, or a long synchronous turn on its own thread; also `started()`/`stopped()` | same `TurnId` for `long_turn` | every actor, both modes |
| D3 starvation | a group (or the probed ambient runtime) cannot run new tasks | the probe task has not run `starvation` after it was spawned (reported before it runs) | async groups and probed runtimes |
| D4 long mailbox | backlog | `mailbox_len >= enable`, cleared at `<= disable` | every actor |

Each report names the actor (id, type, label), the message type, the turn
id, the turn age and poll age; D3 names the group and the actors of that
group inside a poll, oldest first (the likely culprits). Reports are rate
limited per actor and turn: at the threshold, at 2x, 4x, and once when the
turn ends (with its total). Each also increments a counter and, with
`dump_dir`, writes the full snapshot (at most one file per second, the last
16 kept), which survives a runtime whose workers are all blocked.

**Policy.** `Report` is the default and the only policy for D1, D3 and D4.
`kill_after` adds a fenced kill when a turn's age passes it: the actor exits
`Killed`, `stopped()` is skipped, linked actors and monitors see `Killed`, and
a supervisor restarts it as any abnormal exit (OTP: an operator or supervisor
`exit(Pid, kill)`). Because L3 stands, a kill takes effect when the turn
returns. A turn wedged forever keeps the actor alive; the watchdog keeps
reporting it with `kill_pending: true`, and supervisor shutdown reports it as
stuck, as today. Erlang has no stall watchdog (preemption makes it less
needed); `long_schedule` and `long_message_queue` are the nearest.

## Determinism and the simulator

- Out-of-band reads, `snapshot()` and `check_now()` take no scheduler
  decision, register no waker and draw no randomness, so a seed's tape is the
  same with or without them. Under `--cfg spawned_sim` the clock stamp is
  virtual time and the clock thread does not exist.
- `Watchdog::start` is not available under `spawned_sim` (it would add an OS
  thread and wall-clock behaviour). Tests call `check_now()` after advancing
  virtual time; the simulator's turn budget (rank 8b) remains the check for a
  poll that never yields.
- `get_status`/`get_state` are requests: under the simulator they are
  scheduled like any request and replay identically.
- Usable inside `#[sim_test]`: assert mailbox lengths, lifecycle, the current
  message of an actor parked at an await, `which_children` across a restart,
  and that `check_now()` reports a turn after 10 virtual seconds.

```rust
#[sim_test]
async fn backlog_builds_behind_a_wedged_peer() {
    let peer = Peer::new().start();
    let gate = Gate::new();                       // a handler awaits it forever
    peer.send(Wedge(gate.clone())).unwrap();
    for i in 0..100 { peer.send(Ping(i)).unwrap(); }
    rt::sleep(Duration::from_secs(11)).await;     // virtual
    let info = peer.info();
    assert_eq!(info.mailbox_len, Some(100));
    assert_eq!(info.activity.unwrap().message, Some(type_name::<Wedge>()));
    assert!(info.activity.unwrap().poll.is_none()); // suspended, not blocking
    assert!(matches!(peer.get_status().with_timeout(1s).await, Err(ActorError::RequestTimeout)));
    let reports = introspection::Watchdog::check_now_with(WatchdogConfig::default());
    assert!(reports.iter().any(|r| r.detector == Detector::LongTurn && r.actor_id() == Some(peer.id())));
}
```

## Access paths and the `spawned-top` data model

1. **Rust API** above, identical in both modes.
2. **`tracing`**: the structured events; `spawned::stall` and
   `spawned::starvation` are emitted from the watchdog thread, so they are
   produced even when every runtime worker is blocked (provided the
   subscriber does not write from a runtime task).
3. **Metrics** (with `metrics` also enabled), fixed cardinality, each family
   checked by `scripts/check-metrics-exposition.py` with a negative control:
   `spawned_watchdog_long_polls_total`, `_long_turns_total`,
   `_long_mailboxes_total`, `_kills_total`, gauges `spawned_oldest_turn_age_seconds`,
   `spawned_max_mailbox_len`, `spawned_actors_in_turn`, and per group (label
   bounded by the groups an application creates):
   `spawned_group_scheduling_delay_seconds{group}` histogram and
   `spawned_group_starvation_total{group}`. No actor or message labels.
4. **The remote console** (next section), on by default, over the same
   JSON forms.
5. **`spawned-top`** (rank 30, later step): `introspection::render_json(&Snapshot)`
   renders a versioned document without a `serde` dependency (as
   `render_prometheus`); the application serves it on its admin endpoint, and
   the watchdog can write it to `dump_dir`. The tool polls one of them. The
   data model supports its views: a process list with observer's columns
   (id, type, label, lifecycle, current message, turn age, poll age, mailbox,
   group) sorted by any `Rank`; recon's sliding window from two snapshots'
   monotonic counters; the tree view from `SupervisorTree` and `Parent`;
   group load from `busy_workers` and `scheduling_delay`; open stalls from
   `WatchdogState`. Attaching to a process that serves nothing, and
   wait-for edges (rank 7), are later.

An admin endpoint on a starved runtime cannot answer; the dump file and the
watchdog's own `tracing` output are the paths that keep working (the smaller
first step rank 43 asks for).

## Remote console (`spawned-ctl`)

Every binary that runs spawned is attachable, as `erl -remsh` attaches to any
BEAM node, but with a closed command set over the introspection data and no
code evaluation (gap G23, moved to 0.9).

### Defaults and escape hatches

| | Default | Override |
|---|---|---|
| Compiled | yes: Cargo feature `console`, in `default` features | `default-features = false` (or omit `console`): no listener, no table, no symbol |
| Listening | yes, when the process creates its first `ActorSystem` or starts its first actor | `SPAWNED_CONSOLE=off`, or `ConsoleConfig { enabled: false, .. }` |
| Mode | read-only | `SPAWNED_CONSOLE_ALLOW=terminate,restart,trace` or `ConsoleConfig::allow(..)` |
| Path | `$XDG_RUNTIME_DIR/spawned/<pid>.sock`, else `/tmp/spawned-<uid>/<pid>.sock` | `SPAWNED_CONSOLE_PATH=/dir` or `ConsoleConfig::dir(..)` (containers, read-only filesystems) |
| Failure | one `WARN spawned::console` and startup continues | — |
| Transport | Unix domain socket only, never TCP | — |

```rust
// Optional: configure before the first ActorSystem or actor starts; otherwise
// the defaults and the environment apply.
spawned_concurrency::console::configure(ConsoleConfig::default()
    .dir("/run/ethrex")
    .allow([Allow::Terminate, Allow::Restart]))?;
```

**Where the listener runs.** Never on application workers: it would be
starved exactly when it is needed (ethrex `d958dcba4`). In tasks mode it is
one idle task on the `ActorSystem`'s control runtime, which already runs on
its own thread (`spawned-control`). A process with bare actors and no
`ActorSystem` gets that control thread lazily, at its first actor start. In
threads-only builds (no Tokio) it is one OS thread blocked in `accept`. Each
connection is served on the same control runtime or thread, with at most 8
concurrent connections.

**Coverage.** With `console` (or `introspection`) compiled, every actor is
inserted in the process-wide actor table at spawn and removed at its death
point's publication, managed or bare (`actor.start()`, `start_with_backend`,
`start_linked`, threads mode): that is the allowed spawn/exit cost. Bare
actors appear with `Parent::None` or `Parent::Actor` and no group. The 0.9
unified start funnels every start function through one spawn path, so
registration lives in exactly one place and covers every actor by
construction; a test compares the spawned counter with the table under churn
for every public start function.

### Socket lifecycle and permissions

1. Resolve the directory. Create it with mode `0700` if absent. If it
   exists, `lstat` it: it must be a directory (not a symlink), owned by the
   process uid, mode `0700`; otherwise refuse with one warning (a directory
   pre-created by another user in `/tmp` is the squatting attack).
2. Remove `<pid>.sock` if present: the pid is ours, so it is a stale socket
   from a dead process that had the same pid.
3. Bind with a `0177` umask, then `chmod 0600` before `listen` (the `0700`
   directory already closes the window).
4. On every accepted connection, read the peer's credentials
   (`SO_PEERCRED` on Linux, `getpeereid` on macOS, through Tokio's
   `peer_cred` or `rustix`, no `unsafe` in spawned) and drop any peer whose
   uid differs from ours. Root can pass; it can read our memory anyway.
5. Unlink the socket when the last `ActorSystem` shuts down and from an
   `atexit`-style guard on normal exit. A killed process leaves a stale
   socket: `spawned-ctl list` connects to each, and removes those whose pid is
   gone and whose file the caller owns.

Windows: not supported in v1 (`console` compiles to nothing there); named
pipes later.

### Wire protocol (version 1)

Line-delimited JSON over the socket, one object per line, UTF-8, at most 64
KiB per request line. The payloads are the `render_json` forms of the Rust
types in this design (`ActorInfo`, `Snapshot`, `SupervisorTree`,
`StallReport`, `StatusReport`): the console, `spawned-top` and the Rust API
share one data model and one schema version.

```text
<- {"v":1,"hello":{"pid":41237,"binary":"ethrex","spawned":"0.9.0","schema":1,
     "features":["introspection"],"allow":[],"watchdog":true}}
-> {"v":1,"id":1,"cmd":"ps","args":{"sort":"turn_age","limit":20}}
<- {"v":1,"id":1,"ok":{"taken_at_ms":91233411,"actors":[{"id":88123,"type":"…RLPxConnection",…}]}}
-> {"v":1,"id":2,"cmd":"restart","args":{"child":"p2p/peer_table"}}
<- {"v":1,"id":2,"err":{"code":"not_allowed","msg":"restart is disabled (SPAWNED_CONSOLE_ALLOW)"}}
-> {"v":1,"id":3,"cmd":"trace","args":{"actor":88123,"rate":"10/s","max":200,"for_ms":10000}}
<- {"v":1,"id":3,"event":{"turn":5531,"message":"…GetCells","ms":1910,"outcome":"ok"}}
<- {"v":1,"id":3,"end":{"reason":"max","lost":0}}
```

Rules: the server speaks first (`hello`); a request with an unknown `v`
gets `{"err":{"code":"version"}}` and the server's version; new fields may be
added within a version (clients ignore unknown fields); removing or renaming
one is a new version. Responses are capped (a `ps` on 100,000 actors is
paginated with `limit`/`after`). Idle connections close after 10 minutes.

### Commands

| Command | Default | Answers from | While an actor is wedged |
|---|---|---|---|
| `hello`, `ps [--sort mailbox\|turn_age\|poll_age\|turns\|id] [--group g] [--type t] [--limit n]` | read-only | actor table, tier 0 fields; mailbox and ages with `introspection` | answers |
| `info <actor>` (id, `name`, or `label~substring`), `links <actor>` | read-only | `info()` | answers |
| `tree [supervisor]`, `groups`, `snapshot` | read-only | supervision trees, group snapshots | answers |
| `stuck [--window 2s]` | read-only | the watchdog's open reports if it runs; otherwise two samples of the turn ids `window` apart | answers |
| `state <actor> [--timeout 5s]` | read-only | `get_status`: in band; state shown only for `Inspect` actors (opt-in per actor, so no console switch) | times out, with "use `info`" in the error |
| `trace <actor> <rate> [--max n] [--for t]` | **opt-in** (`trace`) | per-actor turn events through a sink session owned by the connection; stops at `max`, `for` (default 10 s) or disconnect; needs `introspection` | events stop while the turn is open; the open turn shows in `info` |
| `restart <child>` | **opt-in** (`restart`) | `ChildHandle::kill()` on the child's current generation: `Killed`, counted in the restart budget, as `exit(Pid, kill)` | takes effect when the turn returns (L3); the reply says so |
| `terminate <child>` | **opt-in** (`terminate`) | the supervisor's `terminate_child` (dynamic supervisors, PR #245; static supervisors once G6 adds it) | as the supervisor's own call |

There is no `eval`, `replace_state`, `suspend` or arbitrary send. Every
command is logged as `INFO spawned::console` (`WARN` for mutating ones) with
the peer pid and uid, the command and the outcome.

**`trace` in v1.** The console needs it, so v1 includes the minimal
per-actor trace that decision 8 otherwise left for 0.10: with the
`introspection` feature only, each turn of a traced actor emits one event
(message type, turn id, duration, outcome) into the connection's own sink
session, rate-limited and auto-stopping (recon's rules). The cost is one
relaxed load of a flag on the activity cache line per turn, compiled out
without `introspection`. The flight recorder stays in 0.10.

### `spawned-ctl`

A new workspace binary, `tools/spawned-ctl` (later `spawned-top` adds the
live TUI over the same protocol):

```text
spawned-ctl list                          # sockets in the default and configured dirs, with pid, binary, liveness
spawned-ctl attach <pid|path>             # interactive: ps, info, state, tree, stuck, trace, restart, terminate
spawned-ctl <pid|path> ps --sort mailbox  # one-shot; --json prints the raw protocol objects
```

### Security

- **Threat model: same user.** Anyone who can connect already runs as the
  process's uid (or root), and can read its memory with a debugger or
  `/proc/<pid>/mem` (subject to Yama's `ptrace_scope`), send it signals and
  read its files. The console gives that principal a structured, read-only
  view by default, and three narrow mutations only if the operator enables
  them. Other local users are stopped three times: the `0700` directory, the
  `0600` socket, and the peer-uid check.
- **What it reveals.** Actor types, labels, registered names, message type
  names, counters, exit reasons, supervision structure. Never message
  payloads or mailbox contents; state only for actors that opt into
  `Inspect`, through their redacting `format_status`. Labels may contain
  peer addresses: they are visible to the same user, as logs are.
- **Why no TCP.** A network listener needs authentication, encryption, key
  or cookie management and firewalling, and its failure mode is remote
  access. A Unix socket's access control is the filesystem plus kernel
  credentials, with nothing to configure or leak. Remote use goes through
  SSH, which already authenticates users (`ssh host spawned-ctl <pid> ps`).
- **Why no eval.** Rust is compiled: an evaluator would mean shipping an
  interpreter or loading code, which turns any access into arbitrary code
  execution. The closed command set keeps the console's capability equal to
  its read-only purpose.
- **Denial of service.** At most 8 connections, 64 KiB request lines,
  paginated responses, per-connection rate limit (20 commands per second),
  idle timeout. Commands read tables under short locks, never wait for a
  turn, and never block actors.
- **Compared with Erlang.** `erl -remsh` joins the node's distribution: epmd
  and a TCP port, authenticated by a shared cookie (`~/.erlang.cookie`),
  unencrypted unless TLS distribution is configured, and the shell has full
  `eval`. Anyone with the cookie and network reach has arbitrary code
  execution on the node. The spawned console has a smaller capability (no
  eval), a smaller surface (local only) and no secret to manage; it gives up
  remote attach without SSH and live code changes, which Rust cannot do
  anyway.

## OTP alignment

| OTP | Spawned v1 | Status |
|---|---|---|
| `process_info/2`: `status` | `Lifecycle` | aligned (names differ) |
| `message_queue_len` | `mailbox_len` | tier 1; aligned, approximate under concurrency |
| `current_function` / `current_stacktrace` | `activity.message`, `kind`, `age`, `poll` | adapted: Rust has no cheap stack sampling; the message type is the useful unit |
| `links`, `monitors`, `trap_exit` | `links`, `monitors`, `trap_exit` | aligned; `monitored_by` deferred (no reverse table today) |
| `registered_name`, `initial_call`, `parent`/`$ancestors` | `name`, `actor_type`, `parent`, `supervisor.path` | aligned |
| `reductions` | `turns`, `messages_in` | adapted: dispatched envelopes, not work units |
| `memory`, `heap_size`, `dictionary`, `group_leader` | not provided | Rust-forced (rank 18a for memory) |
| `proc_lib:set_label` (OTP 27) | `ctx.set_label` | aligned |
| `erlang:processes/0` | `introspection::processes()` | tier 1 |
| `sys:get_state/1,2` | `ActorRef::get_state(f)` | aligned: in band, between callbacks, 5 s default; a closure instead of returning the term |
| `sys:get_status`, `format_status/1` (OTP 25) | `get_status`, `Inspect::format_status` | aligned; redacted by default |
| callback crash in `sys` calls | `ActorError::CallbackFailed`, actor unaffected | aligned (`callback_failed`) |
| `sys:statistics` | `turns`, `messages_in`, `ActorInfo` | adapted: always counted with the feature, no enable/disable |
| `sys:trace`, `trace` sessions (OTP 27), `recon_trace` | sink with sessions in v1; per-actor message tracing 0.10 | deferred |
| `sys:replace_state`, `suspend`/`resume`, `change_code` | not provided | rejected (state outside a turn; not Rust) |
| `supervisor:which_children/1`, `count_children/1` | `which_children`, `count_children`, `tree` | aligned, plus `restarts`, `ready`, `busy` |
| SASL supervisor reports, proc_lib crash reports | `spawned::supervisor`, `spawned::crash` events | aligned fields where they exist |
| `erlang:system_monitor` `long_schedule`, `long_message_queue` (OTP 26) | watchdog D1, D4 | aligned in spirit; Spawned reports, OTP sends a message to one monitor process |
| observer, recon `proc_count`/`proc_window`, `recon:info` (no mailbox copy) | `spawned-top` data model, `Snapshot::top`/`window`; never copies a mailbox | aligned |
| `exit(Pid, kill)` for a hung process | fenced `Killed` | aligned reason; takes effect at turn end (L3, Rust-forced) |
| `erl -remsh` / remote shell over distribution | `spawned-ctl attach` over a local Unix socket | adapted: closed command set, no eval, local only (see [security](#security)) |

## Alternatives considered

| Alternative | Decision | Reason |
|---|---|---|
| Control lane so `get_status` jumps the queue | Rejected for v1 | OTP handles system messages in order; `info()` is the stuck-actor path. Rank 14 / G11 (0.10) may add it as a receiver opt-in. |
| Read the state from outside during a turn | Impossible | The handler future holds `&mut A` across awaits. |
| `Instant::now()` per turn and per poll | Rejected | ~20 ns per read; a coarse clock gives 5 ms accuracy for one load. |
| Tokio `on_before_task_poll`/`on_after_task_poll` hooks | Rejected as the mechanism | `tokio_unstable`, runtime-wide, no actor or message identity; may feed `GroupSnapshot` later behind a feature. |
| tokio-console integration | Complement, later | It sees tasks, not actors, mailboxes, `Thread` or threads-mode actors. |
| Mailbox length from the channel | Not possible | Tokio's `len()` is on the receiver, owned by the (possibly stuck) actor task; `std::sync::mpsc` has none. Rank 37's own mailbox can provide it later at no extra cost. |
| Per-actor counters always on | Rejected | An RMW per send on the default path. |
| Process-wide actor table always on | Rejected | Spawn cost for churn-heavy users; managed actors are listable for free. |
| A `&'static str` or `TypeId` in an atomic | Rejected | Needs `unsafe` or 128-bit atomics; the interned tag is safe. |
| A new `ExitReason::Stalled` | Rejected | OTP uses `killed`; a new reason breaks matches for no information the report lacks. |
| **Turn abort at an await** (a kill drops the handler future at its next await, opt-in per type) | **Deferred to its own item** | It would make `kill` work on the wedged `.await` ethrex describes, and Rust can drop a pending future safely. But it costs a kill-wakeup registration per suspended turn for the opted-in type, changes L3 (a semantics change needing its own model: reply obligations of the dropped turn, helpers, `stopped()`), and is close to the `brutal_kill` emulation the OTP comparison advises against. Ethrex fixed its case with a send timeout. Reviewers: see question 4. |
| **Flight recorder in v1** | **Deferred to 0.10** | Same per-turn ring as `sys:trace` and rank 22's dequeue log; design once with the sink. v1's current turn, crash `last_message` and watchdog cover the incidents. |
| Per-turn `tracing` span (G10) | Deferred to rank 28 | Cheap but not free even when filtered; same instrumentation point, no API conflict. |
| kameo-style console server in the library | Rejected | No network I/O in the library; `render_json` plus the application's endpoint, as metrics. |
| Watchdog as a Tokio task | Rejected | It would starve with the runtime it watches. |
| Console over TCP or HTTP | Rejected | Needs authentication, encryption and secrets; remote access goes through SSH. An application may still serve `render_json` on its own admin endpoint. |
| Console with `eval` | Rejected | Arbitrary code execution for any connected user; Rust has no safe interpreter to offer. |
| Console listener on the application runtime | Rejected | Unreachable during starvation; the control runtime already has its own thread. |
| Linux abstract-namespace socket | Rejected | No filesystem permissions (any local user can connect, only the peer-uid check would remain) and not portable to macOS. |
| Console off by default | Rejected (user decision) | Attachability must not depend on foresight; the cost is off the hot path, with `SPAWNED_CONSOLE=off` and the feature as escape hatches. |

## Semantics contract: guarantees added

Proposed new section 13 of [semantics.md](semantics.md), "Introspection";
ids proposed. L3, M1, M2 and R1 are unchanged.

| Id | Guarantee | Checked by (planned) |
|---|---|---|
| I1 | **Out of band.** `info`, listings, snapshots, supervision trees and watchdog sampling never enqueue, wake, block or run user code on the observed actor, and return while it is inside a turn. | `introspection::*::info_returns_during_a_wedged_turn` (every placement); `introspection_sim::reads_do_not_change_the_tape` |
| I2 | **Activity is consistent and fenced.** The activity fields come from one turn; the same `TurnId` in two reads means that turn was open in between; ages are exact to one clock tick. | `introspection::*::activity_*`; Loom `activity_slot_loom`; sim |
| I3 | **Monotonic and final.** Turn ids, counters, generations and restarts never decrease; `Dead(reason)` is final and equals `wait_exit()`. | `introspection::*::monotonic_counters_*`; fuzz oracle |
| I4 | **Status is in band.** `get_state`/`get_status` run in their own turn between handler turns, in mailbox order, with exactly one outcome (`Ok`, `RequestTimeout`, `ActorStopped(final)`, `NoProc`, `CallbackFailed`); a callback panic does not change the actor. | `Introspection!StatusBetweenTurns`, `!StatusDischarged`; `introspection::*::status_*`; sim race with death |
| I5 | **Redaction.** A type-erased status shows state only for actors that opted into `Inspect`. | `introspection::*::state_is_redacted_by_default`; trybuild |
| I6 | **Listings.** An actor live for a whole listing appears exactly once; no id twice. | `introspection::listing_under_churn` (Loom/Shuttle on the table); fuzz |
| I7 | **Supervision snapshots** are consistent per supervisor level (list and counts agree), not across levels. | `introspection::*::tree_*`, sim restarts |
| I8 | **Watchdog detection.** With the watchdog running, D1 reports a poll on a shared async worker lasting `long_poll` + 2 samples, D2 a turn lasting `long_turn` + 2 samples, D3 a probe late by `starvation`, D4 a mailbox at its threshold; none fires for a duration under its threshold minus one tick. | `watchdog::*` (real time, generous margins); `check_now` sim tests |
| I9 | **Fenced kill.** A watchdog kill applies only to the turn it observed: a turn that ended first is not charged, and once committed no later turn of that actor dispatches. The exit is `Killed` (L4, L5) and takes effect when the turn returns (L3). | `Introspection!FencedKill`, `!KillBitOnTarget`, `!KillSkipsStopped`, `!NoDispatchAfterStop`; Loom on the activity word; sim |
| I10 | **Reports.** Crash, supervisor and stall events use the documented targets and field names; sink sessions are isolated, bounded, and lose events only with a count. | `reports::*` with a capturing subscriber; `sink::*` |
| I11 | **No cost on the default path.** In the default build (`console` on, `introspection` off) and with both off, send, dispatch and turn code execute the same operations as the release before it (the turn-phase store is wider, not added). | the [benchmark gate](#benchmark-gate) and the codegen check |
| I13 | **Console.** The console listens only on a Unix socket whose directory is `0700` and owned by the process user and whose file is `0600`, and serves only peers with the process's uid; it never opens a network port; it is read-only unless mutations are enabled; every command is logged; its commands are out-of-band reads (I1) except `state` (in band, I4) and the enabled mutations; a socket failure never fails startup. | `console::*` (below) |
| I12 | **Deterministic.** Under `spawned_sim`, reads and `check_now()` take no scheduling decision and draw no randomness. | `introspection_sim::reads_do_not_change_the_tape` |

Section 10 (not guaranteed) gains the non-guarantees listed
[above](#consistency-guarantees-and-non-guarantees); section 11 gains the
`get_state` closure, `turns` for reductions and the absent memory fields.

## TLA+ model

New lifecycle interactions in v1: a status request answered between turns and
racing the death point; a watchdog that kills; the kill's race with the end
of the turn it targets. Snapshot races add none: reads change no state, so
they are not model actions (their consistency is a property of the tables,
checked with Loom and tests).

[`Introspection.tla`](../../verification/tla/Introspection.tla) is a small
standalone model (one actor, turns that may suspend forever at an await, a
FIFO mailbox with messages and one status request, cooperative stop, unfenced
`ChildHandle::kill`, and the watchdog's observe/CAS/record steps). It is kept
separate from `Lifecycle.tla`, which PR #240 is changing, and abstracts links
and monitors, which v1 does not touch. Invariants: `FencedKill`,
`KillBitOnTarget`, `StatusBetweenTurns`, `StatusDischarged`,
`KillSkipsStopped`, `NoDispatchAfterStop`.

Results (TLC 2.19 on fede, 4 workers, `nice 19`): `MCIntrospection` (3
messages) 3,304 distinct states, depth 20, no error; 5 messages 10,436
states. Each negative control fails on its named invariant, run one at a
time (`negative_checks.py`, `INTROSPECTION_CONTROLS`):

| Control | Bug | Fails |
|---|---|---|
| `CheckThenAct` | the watchdog checks the turn, then kills in a later step | `FencedKill` |
| `KillBitIgnored` | turn end drops the kill bit set by the CAS | `FencedKill` |
| `StatusInTurn` | the status request is answered while a turn is suspended | `StatusBetweenTurns` |
| `StatusLost` | a queued status request is not answered at death | `StatusDischarged` |
| `KillRunsStopped` | `stopped()` runs after a kill | `KillSkipsStopped` |
| `DispatchAfterStop` | the run loop dispatches after a stop is recorded | `NoDispatchAfterStop` |

No liveness property is checked: with L3 unchanged, an actor whose turn never
resumes legitimately never dies, and the remaining progress properties
(status answered when the actor is idle) are ordinary request progress,
already covered by `Lifecycle.tla` and `DeferredReplies.tla`. Trace
validation: the `lifecycle-trace` feature records `Observe`, `KillCas`,
`TurnEnd` (with the kill bit) and status dispatch, and `TraceIntrospection.tla`
replays them (implementation step 4).

## Test plan

Every guarantee has a check that fails if it breaks. Placements: A
`Backend::Async`, B `Blocking`, T `Thread`, N threads mode, G `async_group`, TG
`thread_group`, P `pinned_group` (Linux), using the `semantics_gaps` placement
macros.

### Integration tests (`concurrency/tests/introspection.rs`, both modes, every placement)

| Test | Guarantee | Fails if |
|---|---|---|
| `info_returns_during_a_wedged_turn` (a handler awaits a never-completed future; another blocks in sync code on T/N) | I1 | `info()` waits for the turn, or misreports `InTurn` |
| `lifecycle_walks_starting_idle_in_turn_exiting_dead` (gates in `started`, a handler, `stopped`) | Lifecycle | a state is skipped or wrong; `Dead` reason differs from `wait_exit` |
| `activity_names_the_message_and_turn` | I2 | wrong type name, turn id not incremented |
| `activity_fields_are_from_one_turn` (10^6 short turns, reader loop) | I2 | a read mixes one turn's tag with another's id |
| `turn_age_is_within_one_tick` (sleeps under paused and real clocks) | I2 | age off by more than a tick plus scheduling slack |
| `mailbox_len_counts_the_backlog` (100,000 sends behind a wedged turn) | fields | not exactly 100,000 when quiescent; nonzero after death |
| `monotonic_counters_never_decrease` | I3 | a decrease across reads |
| `links_monitors_trap_exit_parent_supervisor_position` | fields | any field wrong after link/unlink, monitor/demonitor, restart |
| `status_runs_between_turns_in_mailbox_order` | I4 | status answered before an earlier message or during a turn |
| `status_outcomes` (idle; queued at death; after D; timed out; caller dropped) | I4, R1 | an outcome other than the listed one, or two |
| `callback_panic_is_callback_failed_and_actor_continues` | I4 | the actor exits or the next turn does not run |
| `state_is_redacted_by_default` / `inspect_formats_state` | I5 | a non-`Inspect` actor's state appears |
| `listing_under_churn` (spawn and kill 10,000 actors while listing) | I6 | a stable actor missing or duplicated |
| `tree_counts_agree_with_children` (static, nested, dynamic once #245 lands) | I7 | counts disagree with the list of the same call |
| `labels_show_in_info_and_reports` | fields, I10 | label missing |
| `reports::crash_event_fields` (capturing subscriber) | I10 | a documented field missing or renamed |
| `reports::supervisor_event_fields` | I10 | same |
| `sink::sessions_are_isolated` (one slow session, one fast) | I10 | the fast session loses events, a producer blocks, a loss is uncounted |
| `watchdog::long_poll_names_actor_and_message` (A, G only; not T, B, N, TG) | I8 | no report within `long_poll` + 2 samples, or a report on a dedicated thread |
| `watchdog::long_turn_across_awaits` (all placements) | I8 | no report |
| `watchdog::starvation_reported_before_probe_runs` (a 2-worker group with both blocked) | I8 | report only after release, or culprits not named |
| `watchdog::no_report_under_threshold` | I8 | false positive |
| `watchdog::fenced_kill_kills_the_observed_turn` and `..._skips_a_finished_turn` | I9 | kill misses or hits the next turn |
| `watchdog::kill_waits_for_a_wedged_turn` | I9, L3 | the turn is preempted, or `kill_pending` not shown |
| `watchdog_dump_survives_starvation` | access paths | no file while every worker is blocked |
| `feature_off_api_shape` (`--no-default-features`, `async` only) | tiers | tier 1 fields are not `None`, or a tier 1 item compiles |

### Console tests (`concurrency/tests/console.rs`, Unix; `tools/spawned-ctl` tests)

| Test | Guarantee | Fails if |
|---|---|---|
| `socket_created_at_first_system_and_first_bare_actor` | lifecycle | no socket, wrong path, created before any actor |
| `socket_removed_on_shutdown_and_drop` | lifecycle | file left after `ActorSystem::shutdown` or normal exit |
| `stale_socket_with_our_pid_is_replaced` | lifecycle | bind fails or the stale file is reused |
| `ctl_list_removes_sockets_of_dead_processes` (child process killed with SIGKILL) | lifecycle | stale socket listed as live or not removed |
| `directory_0700_socket_0600` | I13 | other mode bits set |
| `foreign_or_symlinked_directory_is_refused_with_one_warning` | I13 | the console uses it, or startup fails, or more than one warning |
| `peer_with_another_uid_is_dropped` (CI job running as root, connecting from a `setuid` child) | I13 | a command is answered |
| `fallback_to_tmp_without_xdg_runtime_dir`, `configured_dir_is_used` | defaults | wrong path |
| `bind_failure_warns_and_startup_continues` (read-only dir) | I13 | startup fails |
| `commands_answer_while_a_handler_is_wedged` (`ps`, `info`, `stuck`, `tree`; one handler blocked in sync code on every worker of a 2-worker group) | I1, I13 | any command waits for the turn |
| `state_times_out_on_a_wedged_actor_and_says_use_info` | I4 | hangs or answers from inside the turn |
| `stuck_without_introspection_uses_turn_ids` | tier 0 | a wedged actor not listed, or an idle one listed |
| `eight_concurrent_attaches_and_the_ninth_refused` | DoS | a client starves another or the ninth is served |
| `mutations_refused_unless_allowed` / `allowed_restart_kills_the_generation` / `trace_stops_at_max_and_on_disconnect` | I13 | a mutation without allowance; trace keeps running |
| `every_command_is_logged` (capturing subscriber) | I13 | a command without a `spawned::console` event |
| `spawned_console_off_opens_nothing` | escape hatch | a socket or thread exists |
| `feature_off_build` (`--no-default-features --features async`; `cargo tree` and `nm`) | escape hatch | listener, table or console symbol present |
| `protocol_v1_roundtrip`, `unknown_version_is_rejected`, `oversized_line_is_rejected` | protocol | malformed handling |
| `every_start_function_registers` (spawned counter vs table under churn) | coverage | an actor missing from the table |

### Simulator tests (`concurrency/tests/introspection_sim.rs`, `#[sim_test]`, seed sweeps, PCT and DFS where marked)

- `snapshot_during_restarts` (PCT): a OneForAll cohort restarting while
  `tree()` and `actors()` run every step; I6 and I7 oracles (no duplicate id,
  counts agree, generations monotonic).
- `get_status_races_death` (PCT, DFS): status sent concurrently with `stop`,
  `kill` and a panic; exactly one outcome, `ActorStopped` carries the final
  reason, never answered during a turn.
- `fenced_kill_races_turn_end` (DFS): `check_now()` with `kill_after` while
  the observed turn ends; the next turn never runs after a committed kill;
  an ended turn is never killed.
- `reads_do_not_change_the_tape`: the same seed with and without `info`,
  `snapshot` and `check_now` calls between steps records the same decision
  tape (I1, I12).
- `check_now_reports_after_virtual_time`: the example above.
- `console_queries_do_not_change_the_tape`: under `spawned_sim` there is no
  socket; the test calls the console's command handler
  (`console::handle_line`, the same function the listener calls) with `ps`,
  `info`, `tree` and `stuck` between steps, and compares decision tapes with
  a run without them (I12, I13).

### Fuzzer operations (`fuzz/`)

New `Op`s: `Info { who }`, `Snapshot`, `Tree { sup }`, `GetStatus { who, ms }`,
`WatchdogCheck { kill_after_ms }`. Oracles against the driver's model:
`Dead` iff the driver saw the exit, with the same reason; `mailbox_len` 0 when
dead; counters monotonic per id; `GetStatus` outcome in the allowed set and
consistent with the actor's fate; no id twice in a snapshot; after a
`WatchdogCheck` kill, no later handled message of that actor.

### Loom

`activity_slot_loom` (writer begin/end, reader seqlock-style read, watchdog
CAS): no mixed read, no kill of a later turn, kill bit never lost.
`actor_table_loom`: insert/remove against a sharded listing (I6).

### Benchmark gate

- `concurrency/benches/introspection_overhead.rs`, shaped like
  `metrics_overhead`: async and threads; 1, 64 and 1024 actors; 1 and 64
  producers; send-only and request round trip; zero and small work.
- **Off vs main** (I11): alternating A/B runs of the release binary built
  from main and from the branch with the feature off, 10 pairs each, on fede:
  medians within noise (acceptance: no configuration slower by more than 1%,
  with a confidence interval that includes zero).
- **Default build vs console off vs main**: the same A/B on send, request
  and turn benchmarks for three builds: main, the branch's default features
  (`console` on), and the branch with `console` off. Acceptance as above
  (I11).
- **Spawn churn**: `lifecycle_soak`-style start/stop of 100,000 actors,
  reported with and without `console`, to show the table's spawn/exit cost
  (proposed budget: at most 5% of spawn-to-join time).
- **Codegen check**: `cargo asm` (or `objdump`) of the run loop's dispatch
  and `enqueue` with the feature off, diffed against main in CI; plus a test
  asserting `size_of` of `ActorRef`, `Context` and the envelope is unchanged,
  and that no `introspection` symbol is linked.
- **On vs off**: reported, with an acceptance budget for reviewers to set
  (proposed: at most 3% on request round trip, 5% on 64-producer send).

### trybuild (`concurrency/tests/ui`)

`#[actor(inspect)]` without an `Inspect` impl; `#[actor(inspect = debug)]`
without `Debug`; `#[actor(stall(long_turn = "x"))]` with a bad duration;
`get_state` with a non-`Send` closure or result; tier 1 fields used without
the feature. Passing forms in `ui-pass`.

### Planted bugs (`verification/planted/patches/`, PR #247)

| Bug | Must be caught by |
|---|---|
| The watchdog checks then kills without the CAS | `fenced_kill_races_turn_end` (sim), Loom, `Introspection!FencedKill` via trace validation |
| Turn end stores instead of swapping, dropping the kill bit | same |
| Turn end does not reset the activity word (a finished turn looks stuck) | `watchdog::no_report_under_threshold`, `activity_names_*` |
| Activity read without the second word load (mixed turns) | `activity_fields_are_from_one_turn`, Loom |
| The sender counter is not incremented for `Exit`/`Down` envelopes | `mailbox_len_counts_the_backlog` variant |
| The actor table is not cleaned at D (leak, dead actors listed) | `listing_under_churn`, leak tests |
| A queued status request is not discharged at death | `status_outcomes`, `get_status_races_death` |
| A `format_status` panic is treated as a handler panic | `callback_panic_is_callback_failed_*` |
| `Inspect` default formats with `Debug` | `state_is_redacted_by_default` |
| Two message types share an interned tag (pointer equality only, no length) | `activity_names_the_message_and_turn` with two types whose names are prefixes |
| D1 applied to `Thread` actors | `watchdog::long_poll_*` placements |
| The console socket bound before the directory check (or with umask 0022) | `directory_0700_socket_0600` |
| The peer-uid check skipped | `peer_with_another_uid_is_dropped` |
| The listener spawned on the ambient runtime instead of the control runtime | `commands_answer_while_a_handler_is_wedged` |
| A bare-actor start path that does not register | `every_start_function_registers` |
| The clock stamp read from real time under `spawned_sim` | `reads_do_not_change_the_tape`, `check_now_reports_after_virtual_time` |

### Coverage-matrix rows (TESTING.md, "Semantics coverage")

| Id | Strongest checks | Modes |
|---|---|---|
| I1 | `info_returns_during_a_wedged_turn`; `reads_do_not_change_the_tape` | A B T N G TG P, Sim |
| I2 | `activity_*`; `activity_slot_loom` | A B T N G TG, Loom, Sim |
| I3 | `monotonic_counters_*`; fuzz oracle | A B T N G TG, Fz |
| I4 | `status_*`; `get_status_races_death`; `Introspection!StatusBetweenTurns`, `!StatusDischarged` | A B T N G TG P, Sim (PCT, DFS), Model |
| I5 | `state_is_redacted_by_default`; trybuild | A N |
| I6 | `listing_under_churn`; `actor_table_loom`; fuzz | A G N, Loom, Fz |
| I7 | `tree_*`; `snapshot_during_restarts` | G TG, Sim (PCT) |
| I8 | `watchdog::*`; `check_now_*` | A G (D1), all (D2, D4), G (D3), Sim |
| I9 | `fenced_kill_*`; `Introspection!FencedKill` and three more | A G N, Sim (DFS), Loom, Model |
| I10 | `reports::*`; `sink::*` | A N |
| I11 | benchmark gate; codegen and `size_of` checks | A N |
| I12 | `reads_do_not_change_the_tape`; `console_queries_do_not_change_the_tape` | Sim |
| I13 | `console::*` | A N G (Linux, macOS) |

## Client mapping

What an operator would have seen with v1 and the watchdog at its defaults.

### Ethrex `d958dcba4`: `GetCells` served synchronously on every runtime worker

The incident: an eth/72 `GetCells` request could name 256 hashes, each
expanding all blob cells, "all of it ran synchronously on the peer connection
task, so a few concurrent requests could occupy every runtime worker". The fix
bounded the work and moved it to `spawn_blocking`.

Within about 300 ms of the workers filling, from the watchdog thread (the
runtime itself is unable to run anything):

```text
WARN spawned::starvation group="main" delay_ms=1840 workers=8 busy=8
     occupants="RLPxConnection(peer 0x3a1f…)/GetCells poll_ms=1910; RLPxConnection(peer 0x92c0…)/GetCells poll_ms=1720; …"
WARN spawned::stall detector="long_poll" actor_type="ethrex_p2p::rlpx::connection::server::RLPxConnection"
     label="peer 0x3a1f… 203.0.113.7:30303" message="ethrex_p2p::rlpx::eth::cells::GetCells" poll_ms=1910 group="main"
```

`spawned_watchdog_long_polls_total` and `spawned_group_scheduling_delay_seconds`
alert. The operator knows in one log line that a synchronous handler for one
message type, from peers, is blocking the runtime, and which peers sent it;
mitigation is to drop those peers, the fix is the one ethrex made. Requires
`watchdog.probe_runtime(handle, "main")`, since the connection actors run on
the default runtime.

#### The same incident, attached with `spawned-ctl`

The node is not answering RPC. On the host, as the node's user:

```text
$ spawned-ctl list
PID     BINARY  SPAWNED  ACTORS  SOCKET
41237   ethrex  0.9.0    10412   /run/user/1000/spawned/41237.sock

$ spawned-ctl attach 41237
spawned 0.9.0, ethrex pid 41237, read-only, introspection on, watchdog on
> stuck
GROUP main: probe late 1840 ms, 8/8 workers in a poll
  ACTOR   TYPE            LABEL                         MESSAGE   POLL     TURN
  88123   RLPxConnection  peer 0x3a1f… 203.0.113.7      GetCells  1910 ms  1910 ms
  90311   RLPxConnection  peer 0x92c0… 198.51.100.4     GetCells  1720 ms  1720 ms
  …6 more RLPxConnection / GetCells
> ps --sort mailbox --limit 3
  ACTOR   TYPE             STATE    MAILBOX  MESSAGE      TURN AGE
  12      PeerTableServer  Idle     4113     -            -
  15      RpcHandler       Idle     2380     -            -
  88123   RLPxConnection   InTurn   41       GetCells     1.9 s
> info 88123
  RLPxConnection 88123 "peer 0x3a1f…" InTurn GetCells poll 1.9 s, mailbox 41,
  parent p2p/connections (dynamic) generation 1, links 1, monitors 2, group main
```

Every worker is inside a poll of `GetCells` from a connection actor, and the
idle actors' mailboxes grow behind them: synchronous CPU in that handler. The
listener runs on the control runtime's thread, so it answers while every
application worker is blocked. Without the `introspection` feature, `stuck`
still lists the eight `RLPxConnection` actors as "same turn for at least
2.0 s" (from the tier 0 turn ids sampled two seconds apart) with labels and
actor types, without the message type or the mailbox column.

### Ethrex `535647e45`: L2 imports deadlocked at 0% CPU, RPC and logging stopped

Each L2 connection ran `add_block_pipeline` on a Tokio worker; each import
parked 17 jobs on the shared 17-thread merkle pool, so concurrent imports
split the pool and none finished. Workers blocked, the node froze at 0% CPU.

With v1, logs stopping is itself the symptom, so the dump directory matters:

```text
WARN spawned::starvation group="main" delay_ms=30000 busy=4 occupants="L2Connection(peer …)/NewBlock poll_ms=31200; ×4"
WARN spawned::stall detector="long_poll" actor_type="…::L2Connection" message="…::l2::NewBlockMessage" poll_ms=31200 (×4, one per connection)
dump: /var/lib/ethrex/spawned/2026-10-23T14:02:11.json   (written by the watchdog thread)
```

Four connection actors, all inside one poll of the same message for 30 s, at
0% CPU (the process is idle): concurrent blocking imports waiting on a shared
resource. `info()` on the RPC actors shows their mailboxes growing behind the
starved runtime. The operator restarts the node with one connection to the
sequencer as a workaround; the fix is the single importer ethrex wrote. If the
tracing subscriber writes through a runtime task, the `tracing` lines stop
too, which is why the dump file is written by the watchdog thread itself.

### Ethlambda: the chain actor moved to `Backend::Thread` (now a one-thread `thread_group`)

The chain actor's handlers (block import, state transition, signing) are
long synchronous CPU; on the shared runtime they starved P2P and HTTP. The
fix gave it its own thread.

On the shared runtime, v1 would have shown starvation of the network work
naming the culprit:

```text
WARN spawned::starvation group="main" delay_ms=640 occupants="BlockChain/NewBlock poll_ms=790"
WARN spawned::stall detector="long_poll" actor_type="ethlambda_blockchain::BlockChain" message="…::NewBlock" poll_ms=790
```

with the `P2P` actor's `mailbox_len` climbing during each import. After the
move, D1 no longer applies (the actor owns its thread), and the right
setting is a per-type turn limit tied to the slot:

```rust
#[actor(stall(long_turn = "3s"))]   // a block import must finish well within the 4 s slot
impl BlockChain { … }
```

so an import that would miss the slot is reported with its message type.
Ethlambda's shutdown overrun (`1d7902da`, 14.3 s against Docker's 10 s) shows
up as `activity.kind = Stopped, age = 9.8 s` on that actor in
`ShutdownReport` and in `info()` during shutdown.

### Two smaller cases from the client sweep

- Ethrex checks actor health with an ordinary request, which times out while
  a busy actor works. `info()` answers the same question without queueing.
- Ethrex `63167661a`: a `get_best_peer` timeout under mailbox pressure exited
  the node. D4 would have reported `PeerTableServer` crossing its
  `long_mailbox` threshold, with its current message, before the timeout.

## Coordination with other 0.9 work

- **Dynamic supervisor (PR #245):** adopt `ChildInfo` (with `ChildKey`) and
  `ChildCounts` from this design instead of `DynamicChildInfo`; add
  `tree()`; a slow `terminate_child` shows as `SupervisorTree::busy`
  (comparison 3.2.6). PR #245 owns `DynamicChildId`.
- **Unified start:** labels, `Inspect` and stall limits may become start
  options there; the per-type constants stay the zero-cost default.
- **0.8 (PR #240):** the design reads its turn phase, death point and
  `Killed`; nothing here changes their semantics.
- **Rank 5c, 9, G9 (0.10):** extend the event sink, not a new one.
- **Rank 7 (deadlock detection):** its wait-for edges become an `ActorInfo`
  field and a `spawned-top` view.
- **Rank 8a (cooperative budgets):** shares the poll stamps; per-turn work
  metrics read the same slot.

## Implementation order

Each step is a PR with its tests, docs and CHANGELOG entry.

1. Tier 0: per-actor metadata and labels, `info()` from existing state,
   `which_children`/`count_children`/`tree()` with the shared types, crash and
   supervisor events, `get_state`/`get_status`/`Inspect`,
   `ActorError::CallbackFailed`.
2. Tier 1: activity slot, interner, coarse clock, mailbox counters, actor and
   system tables, `processes()`, `snapshot()`, `render_json`; fold
   `diagnostics`; benchmark gate and codegen check.
3. Watchdog: D1-D4, reports, sink sessions, metrics, dump directory,
   `check_now`; remove `WarnOnBlocking`.
4. Fenced kill policy, Loom model, `lifecycle-trace` events and
   `TraceIntrospection.tla`.
5. Console: socket lifecycle and permissions, protocol v1, commands,
   `tools/spawned-ctl`, the per-actor `trace`; console tests and the
   default-build benchmark.
6. Examples (`introspection`, `stall_watchdog`, `console`, both modes), guide section,
   semantics section 13 and coverage rows.

## Open questions for reviewers

1. **`last_message` in crash reports without the feature.** Fetching the
   type name into a local before each handler costs one virtual call per turn
   and no store. Is that acceptable as tier 0 (most useful crash field), or
   must it stay tier 1?
2. **Typed `get_state` on every actor**, with `Inspect` gating only the
   type-erased path. The comparison suggested opt-in for both.
3. **One `ChildInfo` replacing `ChildSnapshot` and `DynamicChildInfo`**
   (breaking for `SupervisedChild::snapshot()`); agree with PR #245.
4. **Turn abort at an await** for opted-in actor types: a separate 0.9 item
   with its own model, 0.10, or never?
5. **Process-wide or per-`ActorSystem` tables and watchdog.** Rank 12 wants
   registries scoped per system so multi-node simulations do not share them;
   v1 proposes process-wide tables with a system id on each entry.
6. **The clock thread** starts lazily on first use; is one 5 ms waking thread
   acceptable in production builds with the feature?
7. **`mailbox_len` counts control envelopes** (`Exit`, `Down`, status) as
   OTP counts all messages; the `metrics` gauge counts user messages only.
   Keep both definitions?
8. **D1 default threshold** 100 ms (today's debug `WarnOnBlocking` uses
   10 ms), and D2's 10 s.
9. **JSON without `serde`** (hand-rendered and versioned) versus an optional
   `serde` feature.
10. **Kill policy scope:** only D2 (turn age) in v1; should a long poll also
    be killable once it returns, to remove a poison actor?
11. **Console defaults.** On by default and listening as soon as the first
    actor starts (not only with an `ActorSystem`), so bare-actor binaries are
    attachable too; one lazily created control thread for those. Is that
    acceptable, or should bare-actor processes need an explicit
    `console::start()`?
12. **`restart <child>` as a kill** of the current generation (counted in
    the budget, effective at turn end) rather than a new supervisor
    operation; and whether `terminate` on static supervisors waits for G6.
13. **Console `trace` in v1**, pulling a minimal per-actor trace forward
    from 0.10 (one flag load per turn under `introspection`).
14. **Widening 0.8's turn-phase word** to carry the turn id touches a
    Loom-verified protocol; worth it for tier 0 `stuck`, or keep turn ids
    tier 1 only?
