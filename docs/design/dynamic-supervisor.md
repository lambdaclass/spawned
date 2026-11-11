# Dynamic supervisor: design contract

**Status: design in review (revision 2), not implemented.** ROADMAP rank 12a.
Draft PR [#245](https://github.com/lambdaclass/spawned/pull/245). Revision 2
applies the maintainer's decisions on the seven open questions of revision 1
and resolves the first independent reviews (Codex and agy); see
[Decisions](#decisions) and [Review resolutions](#review-resolutions).

- Covers OTP's `simple_one_for_one` and Elixir's `DynamicSupervisor`: children
  started at runtime from one template, supervised one for one, terminated
  individually, and all stopped concurrently when the supervisor stops.
- Builds on the v0.8 exit-signal contract ([otp-exit-signals.md](otp-exit-signals.md),
  accepted, implementation in #240): the single death point D, `Killed`,
  `NoProc`, the parent exit signal (E3), first-start failure as
  `SupervisorError::FailedToStartChild` (S1, S2) and `stopped()` skipped for a
  non-trapping child whose parent sends `Shutdown`. It also builds on rank 11
  ([deferred-replies.md](deferred-replies.md), #239) for what callers of a
  dying child see.
- Model: [`DynamicSupervisor.tla`](../../verification/tla/DynamicSupervisor.tla);
  see [What the model establishes](#what-the-model-establishes).
- Client mapping: [dynamic-supervisor-client-mapping.md](dynamic-supervisor-client-mapping.md)
  (ethrex's per-peer connection actors; ethlambda has none).
- Illustrative API shapes are not copyable code.

**Project rule.** Match Erlang/OTP. Deviate only where:

- **R:** Rust forces it;
- **T:** Rust types add strictness; or
- **B:** Spawned adds a bound OTP lacks.

## Contents

- [Sources](#sources)
- [Goals and non-goals](#goals-and-non-goals)
- [Decisions](#decisions)
- [API](#api)
- [Semantics](#semantics)
- [Erlang alignment](#erlang-alignment)
- [Non-guarantees](#non-guarantees)
- [Semantics contract: what this adds and touches](#semantics-contract-what-this-adds-and-touches)
- [Interaction with rank 12 (scoped registries)](#interaction-with-rank-12-scoped-registries)
- [What the model establishes](#what-the-model-establishes)
- [Alternatives considered](#alternatives-considered)
- [Follow-ups](#follow-ups)
- [Review resolutions](#review-resolutions)

## Sources

Read for this design (line numbers refer to these revisions):

- OTP `lib/stdlib/src/supervisor.erl` at master
  [`186c0b94`](https://github.com/erlang/otp/blob/186c0b94da565e4cd234685307b3c3ea41c4af1c/lib/stdlib/src/supervisor.erl)
  (OTP 29 development; compared with `OTP-28.5.0.8`, no behavioural difference
  for dynamic children).
- Elixir `lib/elixir/lib/dynamic_supervisor.ex` at main
  [`b57b70bd`](https://github.com/elixir-lang/elixir/blob/b57b70bd96e2e76d66cb81eba6b39c172fa8e8e4/lib/elixir/lib/dynamic_supervisor.ex)
  (compared with `v1.19.7`; only the shutdown code differs).

What OTP and Elixir do, in short:

| Topic | OTP `simple_one_for_one` | Elixir `DynamicSupervisor` |
|---|---|---|
| `start_child` | `gen_server:call(Sup, …, infinity)`; the supervisor runs the start function, which normally waits for the child's `init` (proc_lib), so starts are serialised with each other and with `'EXIT'` handling ([L652](https://github.com/erlang/otp/blob/186c0b94da565e4cd234685307b3c3ea41c4af1c/lib/stdlib/src/supervisor.erl#L652), [L1077-1092](https://github.com/erlang/otp/blob/186c0b94da565e4cd234685307b3c3ea41c4af1c/lib/stdlib/src/supervisor.erl#L1077-L1092)) | same; "will block the `DynamicSupervisor` until the child initializes" ([L359](https://github.com/elixir-lang/elixir/blob/b57b70bd96e2e76d66cb81eba6b39c172fa8e8e4/lib/elixir/lib/dynamic_supervisor.ex#L359)) |
| Arguments | `ExtraArgs` appended to the template's | `extra_arguments` (fixed per supervisor) prepended to each spec's args ([L752](https://github.com/elixir-lang/elixir/blob/b57b70bd96e2e76d66cb81eba6b39c172fa8e8e4/lib/elixir/lib/dynamic_supervisor.ex#L752)) |
| Specs | one template | any child spec per call |
| Start result | `{ok, Pid}`, `{ok, Pid, Info}`, `{ok, undefined}` on `ignore` (nothing stored), `{error, Reason}` unwrapped | same, but `:ignore` bare |
| Start failure and the budget | not counted: `add_restart` runs only in `restart/2` | not counted |
| `terminate_child` | by pid; any restart type; removed, never restarted; an unknown dead pid returns `ok` ([L1108-1115](https://github.com/erlang/otp/blob/186c0b94da565e4cd234685307b3c3ea41c4af1c/lib/stdlib/src/supervisor.erl#L1108-L1115), [L1861-1875](https://github.com/erlang/otp/blob/186c0b94da565e4cd234685307b3c3ea41c4af1c/lib/stdlib/src/supervisor.erl#L1861-L1875)) | by pid; unknown pid `{:error, :not_found}` ([L712-721](https://github.com/elixir-lang/elixir/blob/b57b70bd96e2e76d66cb81eba6b39c172fa8e8e4/lib/elixir/lib/dynamic_supervisor.ex#L712-L721)) |
| `restart_child`, `delete_child` | `{error, simple_one_for_one}` | not provided |
| `which_children` | `{undefined, Pid \| restarting, Type, Mods}`, unordered | same |
| `count_children` | `specs` always 1; `active` excludes restarting; `workers`/`supervisors` include them ([L1218-1229](https://github.com/erlang/otp/blob/186c0b94da565e4cd234685307b3c3ea41c4af1c/lib/stdlib/src/supervisor.erl#L1218-L1229)) | `specs` = number of children; otherwise the same |
| Default restart type | `permanent` ([L449](https://github.com/erlang/otp/blob/186c0b94da565e4cd234685307b3c3ea41c4af1c/lib/stdlib/src/supervisor.erl#L449)) | `:permanent` ([L404-413](https://github.com/elixir-lang/elixir/blob/b57b70bd96e2e76d66cb81eba6b39c172fa8e8e4/lib/elixir/lib/dynamic_supervisor.ex#L404-L413)); `Task.Supervisor` defaults to `:temporary` |
| Temporary children | start args not kept (`mapsets`), "it could become very costly" ([L1813-1817](https://github.com/erlang/otp/blob/186c0b94da565e4cd234685307b3c3ea41c4af1c/lib/stdlib/src/supervisor.erl#L1813-L1817)) | args dropped (`mfa_for_restart`, [L784](https://github.com/elixir-lang/elixir/blob/b57b70bd96e2e76d66cb81eba6b39c172fa8e8e4/lib/elixir/lib/dynamic_supervisor.ex#L784)) |
| Restart intensity | default 1 in 5 s; `add_restart` before every restart attempt, also each `try_again_restart` retry ([L1477-1531](https://github.com/erlang/otp/blob/186c0b94da565e4cd234685307b3c3ea41c4af1c/lib/stdlib/src/supervisor.erl#L1477-L1531)) | default 3 in 5 s; each `$gen_restart` retry counts |
| Exhaustion | logs `reached_max_restart_intensity`, terminates every child, exits `shutdown` | same |
| `max_children` | none | `:infinity` by default; checked only in `start_child`, `map_size(children) < max_children`, which includes restarting entries; restarts never re-check ([L742](https://github.com/elixir-lang/elixir/blob/b57b70bd96e2e76d66cb81eba6b39c172fa8e8e4/lib/elixir/lib/dynamic_supervisor.ex#L742)) |
| `terminate_child` duration | blocks the supervisor until the child is gone: `shutdown` then `kill` after the timeout, or forever with `infinity` (the default for supervisor children) | same |
| Shutdown | `terminate_dynamic_children`: monitor and `exit(P, shutdown)` to every child first, one timer for the template's shutdown value, then `kill` to every child still alive; order "is not defined" ([L1701-1799](https://github.com/erlang/otp/blob/186c0b94da565e4cd234685307b3c3ea41c4af1c/lib/stdlib/src/supervisor.erl#L1701-L1799)) | same, one timer per distinct shutdown value |
| Static strategies, for contrast | children stopped one at a time in reverse start order, each waited for | |
| Restarted supervisor | starts with no children | same |
| `auto_shutdown`, significant children | rejected for `simple_one_for_one` | only `:never` |

The ROADMAP entry says counting restarting children against `max_children`
"goes beyond Elixir". It does not: Elixir keeps a restarting child in its
`children` map under the old pid, so it counts. This design matches Elixir
there, and the ROADMAP row is corrected in this PR.

## Goals and non-goals

Goals:

1. Start supervised children at runtime, each from a template plus per-call
   arguments, with a synchronous `start_child` that returns the child or a
   typed error.
2. OTP's operation order: one control loop serialises starts, restart
   decisions and terminations, as the supervisor process does.
3. OTP's restart types, supervisor-wide restart budget and escalation.
4. Terminate one child without restarting it.
5. Optional `max_children` that rejects at once, counting restarting
   children.
6. Stop every child concurrently on shutdown, bounded by the per-child
   shutdown timeout and the kill, not by the number of children.
7. One child that cannot finish its cleanup never takes healthy siblings
   down unless OTP would also stop them.
8. Placement on one `ExecutionGroup`; ownership by an `ActorSystem`;
   nesting under a static supervisor.

Non-goals (v1):

- Heterogeneous children (Elixir's any-spec `start_child`): decision Q2.
- A keyed registry owned by the supervisor (rank 12; see
  [Interaction with rank 12](#interaction-with-rank-12-scoped-registries)).
- Supervisor-type dynamic children (a template that builds a nested
  supervisor per call). Possible later through the nested adapter.
- `ignore` from a factory (the static supervisor does not have it either,
  S1).
- `Task.Supervisor` (ranks 11 and 15), `PartitionSupervisor` (sharding),
  significant children and `auto_shutdown` (OTP rejects them for
  `simple_one_for_one`).
- Draining children on shutdown (rank 13), bounded mailboxes (rank 17).
- `terminate_child` on static supervisors (decision Q6, a follow-up).

## Decisions

The maintainer settled revision 1's open questions under the match-Erlang
rule:

| # | Question | Decision |
|---|---|---|
| Q1 | Concurrent or serialised construction | **Serialised, as OTP.** One control loop runs each start (admission, factory, `started()`) to completion before the next operation. Factories still run on the target group. Long work (ethrex's RLPx handshake) moves out of `started()` into the first message, OTP's `handle_continue` idiom |
| Q2 | Heterogeneous children | The typed template is enough for v1 |
| Q3 | Defaults | Elixir's: `max_restarts = 3`, `restart_window = 5 s`; backoff **opt-in** (`None` by default); `max_children = None` |
| Q4 | Group capacity across restarts | The slot holds a **transferable native-capacity reservation** from admission to removal, separate from any generation's scope lease |
| Q5 | Exit reason of an exhausted supervisor | `Shutdown`, as OTP, keeping the diagnostic in `failure()`. Static nested supervisors need the same change: a separate follow-up PR |
| Q6 | `terminate_child` on static supervisors | Separate follow-up |
| Q7 | Arguments on rejection | Returned on pre-admission rejection (`MaxChildren`, `Stopped`, placement); a distinct error for a start aborted after construction began |

## API

### Template

```rust
/// One template for every child: placement, factory, restart type, limits.
/// The factory gets the per-call arguments (OTP's ExtraArgs).
pub struct DynamicChildSpec<A: Actor, Args> { /* name, group, factory, restart, timeouts */ }

impl<A: Actor, Args: Clone + Send + 'static> DynamicChildSpec<A, Args> {
    /// Permanent, as OTP's default. A restartable child is rebuilt from its
    /// arguments, hence `Clone`.
    pub fn new<F, U, E>(name: impl Into<String>, group: &ExecutionGroup, factory: F) -> Self
    where
        F: Fn(FactoryContext, Args) -> U + Send + Sync + 'static,
        U: Future<Output = Result<A, E>> + Send + 'static,
        E: std::fmt::Display;
    pub fn restart(self, restart: RestartMode) -> Self;
}

impl<A: Actor, Args: Send + 'static> DynamicChildSpec<A, Args> {
    /// A Temporary template. Arguments are moved into the only generation
    /// and never kept (OTP stores no arguments for temporary children), so
    /// they need not be `Clone`: a `TcpStream` works.
    pub fn temporary<F, U, E>(name: impl Into<String>, group: &ExecutionGroup, factory: F) -> Self;
}

impl<A: Actor, Args> DynamicChildSpec<A, Args> {
    pub fn startup_timeout(self, limit: Duration) -> Self;   // default 5 s
    pub fn shutdown_timeout(self, limit: Duration) -> Self;  // default 5 s
}
```

- Two constructors is a Rust-forced shape (R): a restartable child must be
  rebuilt from its arguments, a temporary one must accept move-only values.
  `RestartMode::Temporary` passed to `restart()` is allowed and never
  clones. The restart type is per template, as in `simple_one_for_one`.
- `Args` needs `Send + 'static`, plus `Clone` for restartable templates; not
  `Sync` (one owner holds the retained copy).
- `DynamicChildSpec`, `DynamicSupervisor`, `SupervisedDynamicSupervisor` and
  `DynamicChild` implement `Clone` without requiring `A: Clone` or
  `Args: Clone` (shared handles), so a supervisor of move-only arguments can
  be stored in a cloneable context.

### Starting the supervisor

```rust
pub struct DynamicSupervisorConfig {
    pub max_restarts: usize,          // default 3 (Elixir)
    pub restart_window: Duration,     // default 5 s (Elixir)
    pub max_children: Option<usize>,  // default None (Elixir :infinity); Some(0) allowed
    pub backoff: Option<Backoff>,     // default None: restart at once, as OTP
}

// Root, owned by the system (a shutdown unit in admission order, S2):
let peers: DynamicSupervisor<PeerConnection, PeerArgs> =
    system.dynamic_supervisor(config, spec)?.start().await?;

// Nested under a static supervisor, as `add_supervisor` (V8):
let slot: SupervisedDynamicSupervisor<PeerConnection, PeerArgs> =
    builder.add_dynamic_supervisor(DynamicSupervisorSpec::new("peers", config, spec))?;
let peers = slot.current()?;   // re-resolve after the parent restarts it
```

- `start()` returns once the supervisor is admitted; it has no children to
  start, so it cannot fail with `FailedToStartChild`.
- Configuration errors (zero window, `backoff.maximum < initial`, zero
  timeouts, a foreign group) are `SupervisorError::Configuration` or
  `System(ForeignGroup)` at construction. `max_children = Some(0)` is valid
  and rejects every start, as Elixir's `max_children: 0`. The static
  supervisor's `max_children <= 65536` ceiling does not apply.
- `DynamicSupervisorSpec::new(name, config, spec)` takes the template; the
  adapter runs on the template's group. Its `shutdown_timeout` (how long the
  parent waits for the whole dynamic supervisor) defaults to
  **2 × the child `shutdown_timeout` + 1 s**, so the parent never gives up
  before the inner kill stage ran (agy 6). OTP defaults supervisor children
  to `infinity` for the same reason; Spawned keeps a finite default so a
  stuck subtree is still reported (B). It can be raised with
  `.shutdown_timeout()`.

### Operations

```rust
impl<A: Actor, Args> DynamicSupervisor<A, Args> {
    /// OTP start_child(Sup, ExtraArgs). Queued on the control loop; resolves
    /// when the first generation is ready (started() returned) or failed.
    pub async fn start_child(&self, args: Args) -> Result<Started<A>, StartChildError<Args>>;
    /// OTP terminate_child(Sup, Pid): by the child's last published ActorId.
    pub async fn terminate_child(&self, id: ActorId) -> Result<(), TerminateChildError>;
    /// The same by slot, for a child seen in which_children (any state).
    pub async fn terminate_slot(&self, id: DynamicChildId) -> Result<(), TerminateChildError>;
    pub fn which_children(&self) -> Vec<DynamicChildInfo>;
    pub fn count_children(&self) -> ChildCounts;
    pub fn stop(&self);
    pub async fn shutdown(&self, deadline: Duration) -> SupervisorReport;
    pub async fn join(&self);
    pub fn failure(&self) -> Option<String>;
}

pub struct Started<A: Actor> {
    pub actor: ActorRef<A>,       // the first generation, as OTP's Pid
    pub child: DynamicChild<A>,   // the slot, across restarts
}

/// Like `SupervisedChild<A>` for a dynamic slot. Holds no arguments and no
/// scope; after removal it keeps only terminal metadata.
impl<A: Actor> DynamicChild<A> {
    pub fn id(&self) -> DynamicChildId;
    pub fn current(&self) -> Result<ActorRef<A>, ChildUnavailable>;
    pub async fn wait_for_generation(&self, after: u64) -> Result<ActorRef<A>, ChildUnavailable>;
    pub fn snapshot(&self) -> ChildSnapshot;
    pub async fn terminate(&self) -> Result<(), TerminateChildError>;   // = terminate_slot
}

#[non_exhaustive]
pub enum StartChildError<Args> {
    /// Not admitted: nothing was constructed and the arguments come back.
    Rejected { reason: RejectReason, args: Args },
    /// Admitted, then the supervisor began shutting down before readiness
    /// committed. The arguments were consumed by the factory.
    Aborted,
    /// The first generation failed: factory error or panic, panic or stop
    /// in started(), or startup_timeout. Not retried, not charged.
    FailedToStart { cause: String, exit: Option<ExitReason>, cleanup_stuck: bool },
}

#[non_exhaustive]
pub enum RejectReason {
    MaxChildren,           // Elixir {:error, :max_children}
    Stopped,               // shutting down or stopped (OTP: the call exits)
    Placement(SystemError),// the group is closed or out of native capacity
}

#[non_exhaustive]
pub enum TerminateChildError {
    NotFound,      // not a current child (Elixir {:error, :not_found})
    Stopped,       // called after shutdown began: the supervisor stops it
    CleanupStuck,  // the child missed its deadlines; see "Stuck children"
}

pub struct DynamicChildInfo {
    pub child: DynamicChildId,
    pub actor: Option<ActorId>,   // last published id; None before the first readiness
    pub ready: bool,              // false while restarting (OTP `restarting`)
    pub state: ChildState,
    pub generation: u64,
}

pub struct ChildCounts {
    pub specs: usize,        // 1, as OTP simple_one_for_one
    pub active: usize,       // children with a live, ready generation
    pub supervisors: usize,  // 0 in v1
    pub workers: usize,      // every child held, restarting and stuck included
}
```

- `StartChildError<Args>` implements `Error` when `Args: Debug`;
  `into_args()` returns the arguments of a `Rejected`, and `without_args()`
  maps to `StartChildError<()>` for `?` in code that does not need them.
- `FailedToStart` carries the generation's `ExitReason` when an actor
  existed (`Panic` from `started()`, `Normal` from `ctx.stop()` in
  `started()`), so callers need not match strings (agy 9).
- `ChildState` gains no variant: a dynamic child is `Starting`, `Running`,
  `Backoff` (restart pending), `Stopping` (including stuck), then leaves
  `which_children`. `which_children` is a snapshot in no particular order.
- Removed slots are evicted from the supervisor's table at removal, after
  their retained arguments are dropped: memory is O(children held), not
  O(starts).

### Restart types

| | OTP | This design |
|---|---|---|
| Default | `permanent` | `Permanent` (`DynamicChildSpec::new`); `temporary()` for one-shot children |
| Table | `do_restart/3` | V1 with the v0.8 rule: Permanent always; Transient on an abnormal reason (`Panic`, `Kill`, `Killed`, `NoProc`) or a recorded cleanup failure; Temporary never |
| A replacement failing before readiness | `try_again_restart`, retried | retried unless Temporary, each attempt charged |
| `terminate_child` | never restarted | never restarted, any type |

### Restart budget

- **Scope:** one rolling window per supervisor, as OTP's intensity and
  period. Defaults 3 per 5 s (Elixir).
- **What counts:** every restart attempt: the factory call of every
  generation after a child's first, including retries of a replacement that
  failed before readiness (OTP's `add_restart` before each restart and each
  `try_again_restart`). There are no collateral restarts, so v0.7's "every
  factory call" and rank 5f's "recovery rounds" count the same here.
- **What does not count:** a failed `start_child`, a Temporary child's exit,
  an exit after `terminate_child`, exits during shutdown.
- **Reservation protocol** (as V5, modelled here): when the loop decides a
  restart, it reserves one grant; if none is left the supervisor fails at
  once (no backoff is entered). The reservation is stamped into the window
  when the restart attempt is admitted, and refunded if the pending restart
  is cancelled by `terminate_child` or shutdown. Window expiry releases only
  stamped grants, never reservations.
- **Backoff** is opt-in (`backoff: Some(..)`): without it the restart is
  attempted as soon as the loop handles the exit, as OTP. With it, the
  per-child jitter stream is seeded from the supervisor seed mixed with the
  slot's `DynamicChildId`, not only the shared template name, so siblings do
  not draw identical delays (agy 9).
- **Exhaustion:** the supervisor fails (`failure()` records "restart
  intensity reached" with the child and its last reason, as OTP's
  `reached_max_restart_intensity` report), closes admission, stops every
  child as in a shutdown, and **exits `Shutdown`** (OTP): a nested dynamic
  supervisor under a Transient parent is then not restarted; under a
  Permanent parent it is (decision Q5). Stuck cleanups and coordinator
  faults still exit abnormally.

### `max_children`

- `None` by default. `Some(n)` caps the children held: starting, running,
  cleaning up, in backoff, constructing a replacement, or stuck. A slot is
  freed only when its child is removed: its last generation finished owned
  cleanup (milestone 7 of the exit phases), its retained arguments were
  dropped, and no restart follows.
- Checked only when the loop admits a start; a full supervisor answers
  `Rejected { MaxChildren, args }` and nothing waits. Restarts are never
  re-checked (Elixir) and never need to be: their slot was never freed.
- The slot frees after cleanup, not at the exit signal as OTP removes the
  child at `'EXIT'` (B: the cap bounds what is really held).

### Placement and capacity

- Every child and every factory runs on the template's `ExecutionGroup`.
  The group must belong to the supervisor's `ActorSystem`.
- Async groups: children share the group's workers; nothing is reserved.
  The supervisor's control loop runs on the system's control runtime,
  outside every group budget, as today.
- Native groups (`thread_group(n)`, one thread per live actor): each slot
  takes a **transferable native-capacity reservation** at admission and
  keeps it until removal. A generation's scope draws its thread from that
  reservation and returns it when the generation's scope has joined; the
  replacement draws it again. The reservation is a separate permit owned by
  the slot, not the old scope's completion lease, so it never keeps a
  finished scope from joining (decision Q4). A start that cannot reserve
  answers `Rejected { Placement(Capacity), args }`.
- Pinned groups multiplex actors on a shard and have no per-actor native
  capacity; the slot keeps its shard (affinity) across restarts and
  reserves nothing.
- A closed group rejects starts (`Placement(Closed)`) and fails restarts,
  which fails the supervisor, as static supervisors treat group closure.

### Nesting and the system

- Root: `system.dynamic_supervisor(config, spec)` is a shutdown unit in
  admission order, stopped in its turn by `ActorSystem::shutdown` (S2); its
  children stop concurrently in that turn.
- Under a static supervisor: `add_dynamic_supervisor` uses the nested
  adapter (V8), with its join barrier over every descendant. A restart by
  the parent creates a new, empty dynamic supervisor (OTP). Old handles
  return `Stopped`; `SupervisedDynamicSupervisor::current()` resolves the new
  one.

## Semantics

### The control loop

One logical control loop per supervisor handles, in arrival order and one at
a time, as OTP's supervisor process does:

- a `start_child`: admission (gate, `max_children`, capacity), then the
  first generation's factory and `started()`, until readiness commits or the
  attempt failed and was cleaned up (or reached its cleanup deadline);
- a child's exit: the restart decision, the reservation, and (after the
  optional backoff, which does not hold the loop) the restart attempt until
  its readiness or failure;
- a `terminate_child`: signal, kill escalation and removal of that child.

Not serialised: children run, exit and clean up concurrently; the backoff
timer runs off the loop; `which_children`, `count_children` and `current()`
read a snapshot without waiting for the loop; shutdown closes admission at
once and cancels the operation in progress (row 21 of the alignment table,
B). Factories run on the target group even though the loop awaits them.

Consequences, all as in OTP:

- A slow `started()` delays every later start, restart and termination of
  that supervisor. Keep `started()` short and do slow initialisation in the
  first message (OTP's `handle_continue`). `startup_timeout` bounds the
  delay per start.
- With `max_children = 1`, a start that arrives while another is
  constructing waits; if the first fails, the second is admitted (Codex 1).
- A `terminate_child` queued behind a start of the same child finds it.

### `start_child`

1. **Queued** on the loop. If the supervisor is already closed:
   `Rejected { Stopped, args }` at once.
2. **Admission** when the loop reaches it: closed → `Rejected { Stopped, args }`
   (also for a request still queued when shutdown begins); cap reached →
   `Rejected { MaxChildren, args }`; native capacity → `Rejected { Placement, args }`.
   Otherwise the slot is created (`Starting`), counted from now on.
3. **Construction**: arguments are moved (Temporary) or cloned (restartable;
   the original is retained by the slot) inside the tracked attempt, on the
   target group, outside framework locks. The factory runs, then
   `started()`, bounded together by `startup_timeout`. A panicking `clone()`
   is a failed attempt like a factory panic.
4. **Commit.** Readiness commits under the gate, only while it is open. The
   commit is the success: from then on `start_child` returns
   `Ok(Started)`, even if shutdown begins before the caller observes it
   (success is historical, like `ActorRef::ready()` and `start()`'s
   readiness, `supervision.rs` `start_owned`). The returned `actor` may
   already be dead: if the child crashed between the commit and the caller's
   wake-up, it is handled by the restart table and `child.current()`
   resolves the replacement (Codex 2).
5. **First-start failure.** Factory error or panic, panic or stop in
   `started()`, or `startup_timeout`: the generation is stopped and cleaned
   up, the slot removed, and `FailedToStart { cause, exit, cleanup_stuck: false }`
   returned. Never retried, never charged (OTP). If the cleanup misses its
   deadlines (E3 rows 3-4), `FailedToStart { .., cleanup_stuck: true }` is
   returned at the deadline; the slot stays counted and its work stays owned
   by the system until it really ends, as S2 releases `start()`'s waiter
   (Codex 4, agy 7). The supervisor is not failed: see
   [Stuck children](#stuck-children).
6. **Shutdown during construction**: the attempt is cancelled (state dropped
   without hooks if `started()` had not begun, L2; otherwise the parent exit
   signal); `Aborted` is returned after its cleanup or deadline. The error
   reflects what ended the attempt: a factory failure recorded before
   shutdown stays `FailedToStart` (agy 7).
7. **Cancellation** of the `start_child` future does not cancel the start
   (R4); a child that becomes ready is supervised and listed. OTP behaves the
   same when the caller dies.

### Races

| Race | Outcome |
|---|---|
| `start_child` vs shutdown, while queued | `Rejected { Stopped, args }` |
| `start_child` vs shutdown, during construction | `Aborted`; never `Ok` unless readiness committed first (model: `NoStartAfterShutdown`) |
| `start_child` vs shutdown, after the commit | `Ok`; the child is then stopped with the others |
| `start_child` vs a sibling's crash | the crashed child keeps its slot through cleanup, backoff and the replacement, so at the cap the new start gets `MaxChildren` (`MaxChildrenBound`, control `UncountedRestart`) |
| `start_child` vs a sibling's removal | the slot frees only after the removed child's cleanup and argument drop |
| `start_child` vs budget exhaustion | exhaustion closes the gate; as shutdown |
| `terminate_child` vs a crash of that child | serialised by the loop. Exit handled first: if a restart is pending, `terminate_child` cancels it (refunding the reservation) and removes the child; if the restart attempt is running, `terminate_child` waits behind it and then terminates the new generation. Terminate first: the crash is recorded, no restart. Either way the child is removed and never restarted (`TerminatedNeverRestarted`, `NoPublishAfterStop`) |
| `terminate_child` vs shutdown | a call queued or in progress when shutdown begins resolves when the child is removed (`Ok`, or `CleanupStuck`); a call made after shutdown began returns `Stopped` |
| two `terminate_child` calls for one child | the second waits for the same removal and gets the same result; a call after removal gets `NotFound` |

### Restart identity and termination keys

- Every generation has a new `ActorId`, mailbox and state (M4). The slot's
  `DynamicChildId` never changes. `current()` returns the latest ready
  generation; `wait_for_generation(n)` waits for a newer one (V4).
- The **termination key** of a slot is its **last published** `ActorId`.
  It is kept through cleanup, backoff and failed replacement attempts, and
  switches only when a replacement's readiness commits, as OTP keeps the
  original pid as the `restarting` entry's key and Elixir keeps
  `current_pid` (Codex 3, agy 5). A call with an older id after a
  successful replacement gets `NotFound`; only a slot-based call
  (`terminate_slot`, `DynamicChild::terminate`) is guaranteed to reach the
  child whatever its generation.
- `which_children` reports that key and whether the child is ready, so a
  child seen restarting can be terminated by key or by slot.
- Callers that find children by a domain key keep their own map until rank
  12 ([below](#interaction-with-rank-12-scoped-registries)).

### Terminating one child

`terminate_child` is OTP's `shutdown/2` for one child, run by the loop:

1. Mark the slot terminated (no restart from now on); cancel a pending
   restart and refund its reservation.
2. Send the parent exit signal `Shutdown`: a non-trapping child begins exit
   after its current handler and skips `stopped()` (E2, decision 2); a
   trapping child handles the messages queued before the signal, then runs
   `stopped()` (E3).
3. If the child has not reached its death point `shutdown_timeout` after the
   signal, kill it (`Killed`, E1). If it misses a second `shutdown_timeout`,
   or its owned cleanup misses its deadline, the child is stuck:
   `terminate_child` returns `CleanupStuck`.
4. Otherwise resolve `Ok` once the child is removed (`TermOkMeansRemoved`).

The loop is held for the whole call, as OTP's supervisor is blocked in
`terminate_child`; it is bounded by 2 × `shutdown_timeout` plus the cleanup
deadline.

### Stuck children

OTP's `kill` always ends a process, so an OTP supervisor never waits on a
child that ignores `shutdown` longer than the shutdown timeout (except with
`infinity`). Spawned's kill is cooperative (R): a child can stay in a
handler, in `stopped()` or in owned cleanup past every deadline. A child is
**stuck** when, after its stop signal, it misses the kill and the second
interval, or when its owned cleanup misses its deadline (`shutdown_timeout`
from exit begin for a child that ended on its own, E3 row 4: one interval,
no kill). Policy (agy P0):

| The stuck child | Effect |
|---|---|
| was retiring anyway (Temporary, Transient after a normal exit, removed by `terminate_child`, or a failed first start) while the supervisor is open | **only that child is affected.** Its slot stays counted (it still holds memory, sockets, capacity) and its work stays owned until it really ends, then the slot is removed. The failure is recorded in its `ChildSnapshot::last_error` and the supervisor's report; a waiting `terminate_child` gets `CleanupStuck`, a waiting `start_child` gets `FailedToStart { cleanup_stuck: true }`. The supervisor and its siblings are unaffected, as OTP's supervisor carries on after killing a child |
| would be restarted (Permanent, Transient after an abnormal exit or with the cleanup failure the stall implies) | the supervisor **fails** and exits abnormally: it cannot start the replacement while the old generation lives (V3), and OTP would have restarted it; escalation lets the parent decide (V8) |
| is being stopped by shutdown | the shutdown report is incomplete and records the failure (V9, S3); "done" waits for it |

Model: `StuckIsolated`, control `StuckFailsAll`. A retiring stuck child is
also never restarted later; its slot simply ends when the work ends.

### Shutdown of all children

When the supervisor closes (`stop`, `shutdown(deadline)`, system shutdown,
its parent, or a failure):

1. Admission closes: queued starts get `Rejected { Stopped, args }`, the
   construction in progress is cancelled (`Aborted`), pending restarts are
   cancelled and refunded.
2. **Fan-out:** the supervisor sends every live child the parent exit
   signal, one after another without awaiting any child's completion, then
   waits. Children have no order (OTP: "not defined"). Model:
   `ConcurrentShutdown`, control `Sequential`.
3. **Deadlines:** each child's intervals are measured from its own signal
   (E3): kill after `shutdown_timeout`, stuck after a second one. OTP arms
   one timer after the fan-out; measuring from each child's own signal
   differs only by the fan-out time and never shortens a child's interval
   (Codex 9). Shutdown therefore takes at most 2 × `shutdown_timeout` plus
   the fan-out time plus owned cleanup up to its deadline, whatever N is.
4. **Done** means every slot was removed: every generation reached
   publication and its owned cleanup finished, its arguments dropped (V9;
   `NoChildOutlivesSupervisor`, `ClosedLeadsToDone`).
5. `shutdown(deadline)` keeps the static meaning: `deadline` is the report
   deadline; children still running then stay owned by the system (S3).

The model takes the fan-out as one step: the implementation sends the
signals in a loop with no await between them, and a child acting before its
own signal only ends earlier. Trace validation of the implementation will
check this abstraction (signal events precede any wait event).

### Retained arguments

- Restartable slots keep the original `Args`; each attempt receives a clone
  made inside the attempt (on the target group, outside framework locks,
  within `startup_timeout`). A panicking or hanging clone is a failed
  attempt (Codex 10).
- At removal the slot drops its retained arguments as part of its cleanup,
  before its native-capacity reservation and its `max_children` slot are
  released. A panicking destructor is recorded as a cleanup failure (after
  the child's death it changes no exit reason); a hanging one makes the slot
  stuck, under the policy above.
- `DynamicChild` handles hold no arguments and no scopes; after removal they
  report terminal metadata only.

### In-flight requests and deferred replies of a dying child

As for any actor (exit-signals design, phases of an exit, with rank 11):

- At the child's death point D every queued request and every open
  `ReplyTo` token is answered `ActorStopped(final reason)`: `Shutdown` after
  a clean stop, `Killed` after the kill, the crash reason after a crash. A
  request or registration after D gets `NoProc`.
- A non-trapping child stopped by its supervisor (`terminate_child` or
  shutdown) skips `stopped()`, so cleanup that must always run belongs in
  `Drop`, in an owner that monitors the child, or needs `trap_exit` (and
  even then not after a kill).
- Pipe jobs of the child are cancelled at exit begin and joined after
  publication (rank 11 step 4) within the cleanup deadline; the slot frees
  only after that join.
- A replacement never receives the old generation's queue, tokens or jobs
  (V4; rank 11 `ActorId` fencing).

### Links and monitors

- The supervisor is the parent of every child generation (E3). It is not an
  actor: it observes children through internal completion state, not links
  or a mailbox (V9), so a child's backlog never delays its exit handling and
  a child cannot unlink itself from supervision. (OTP links to children and
  uses a monitor only while shutting one down, so that a child that called
  `unlink(Sup)` cannot hang the supervisor; Codex 14.)
- Children can link to and monitor each other and outside actors as usual.
- Owners that track children should monitor them; the `Down` arrives exactly
  once with the final reason, also after a kill or a skipped `stopped()`.

## Erlang alignment

| # | Rule | OTP / Elixir | This design | Reason for any deviation |
|---|---|---|---|---|
| 1 | `start_child` is synchronous and returns the child | `{ok, Pid}` after the start function returns | `Ok(Started { actor, child })` after `started()` | aligned; `child` is an addition (row 9) |
| 2 | Operations serialised by the supervisor | one process: starts, `'EXIT'` handling, restarts and `terminate_child` run one at a time | one control loop, same order and blocking | aligned (decision Q1) |
| 3 | Template plus per-call args | `ExtraArgs` appended (Elixir: fixed `extra_arguments` prepended) | typed `Args` passed to the factory | T |
| 4 | `ignore` | `{ok, undefined}`, nothing stored | not supported | as the static supervisor (S1) |
| 5 | Start failure | `{error, Reason}`, not counted, not retried | `FailedToStart`, not counted, not retried, after cleanup or at its deadline | aligned; the deadline release is R (S2) |
| 6 | Default restart type | `permanent` | `Permanent` | aligned |
| 7 | Temporary children keep no args | `mapsets` | `temporary()` moves args once, no `Clone` | aligned; R for the separate constructor |
| 8 | `terminate_child` key | pid, kept through failed restarts | last published `ActorId`, kept through failed restarts; `NotFound` (Elixir) for anything else | aligned with Elixir's `not_found`; OTP's `ok` for a dead unknown pid is a `find_child` artefact |
| 9 | Restart identity | new pid, found through names | new `ActorId`; stable `DynamicChildId`, `current()`, `terminate_slot` | addition, consistent with `SupervisedChild` (instance-bound references) |
| 10 | `restart_child`, `delete_child` | `{error, simple_one_for_one}` | not provided | aligned (absent at compile time: T) |
| 11 | `which_children` | pid or `restarting`, unordered | last published id plus `ready`, unordered | aligned plus fields |
| 12 | `count_children` | `specs` 1, `active` excludes restarting | same | aligned with OTP |
| 13 | `max_children` | Elixir: default infinity, `0` allowed, start only, counts restarting | same; frees after cleanup | aligned; B for freeing after cleanup |
| 14 | Restart budget | every restart attempt counts; Elixir default 3 in 5 s | same rule and defaults | aligned (decision Q3) |
| 15 | Exhaustion | terminate children, exit `shutdown` | terminate children concurrently, exit `Shutdown`, diagnostic in `failure()` | aligned (decision Q5); static nested supervisors follow in a separate PR |
| 16 | Shutdown order | parallel, undefined, one timer after the fan-out, then `kill` | parallel, per-child intervals from each child's signal, `kill`, then stuck | aligned up to the fan-out time; the stuck stage is R |
| 17 | Shutdown signal | `exit(Child, shutdown)`; a trapping child runs `terminate` | parent exit signal; trapping child runs `stopped()` after its backlog; non-trapping skips it | aligned (E2, E3) |
| 18 | `start_child` during shutdown | the queued call exits with the supervisor's reason or `noproc` | `Rejected { Stopped, args }` or `Aborted` | R: no caller exit in Rust; T: the arguments come back |
| 19 | Restarted dynamic supervisor | empty | empty | aligned |
| 20 | Backoff | none | none by default; opt-in per supervisor | aligned by default (decision Q3) |
| 21 | Shutdown admission | the supervisor processes its queue in order; a start queued behind the shutdown is never handled | admission closes at once, the construction in progress is cancelled | B (exit-signals design) |
| 22 | When a slot frees | at `'EXIT'` | after owned cleanup and the argument drop (V3) | R and B: destructors and owned helpers outlive the last hook |
| 23 | A child that ignores `shutdown` | killed; the kill always works | killed cooperatively; if it still does not end it is stuck, isolated unless it would restart | R: Rust cannot kill a thread; policy in [Stuck children](#stuck-children) |
| 24 | Calls in flight to a dying child | `'EXIT'`/monitor tells callers | `ActorStopped(reason)` at D, `NoProc` after | aligned (rank 11, v0.8) |
| 25 | Heterogeneous specs | Elixir: any spec per call | one typed template | T; decision Q2 |
| 26 | Nested supervisor's shutdown wait | `infinity` by default for supervisor children | 2 × child `shutdown_timeout` + 1 s by default | B: a stuck subtree is still reported |

## Non-guarantees

- **No order** among dynamic children, at start (beyond arrival order on the
  loop) or shutdown, and none in `which_children`.
- **No queueing** at `max_children`: a full supervisor rejects. Waiting for
  a slot is rank 17's admission.
- **No parallel construction:** one slow `started()` delays the supervisor's
  other operations (OTP's behaviour). Use the first message for slow work.
- **No exactly-once start under cancellation:** a dropped `start_child`
  future may still produce a supervised child.
- **No state or queue carried across a restart** (V4).
- **No transparent reference:** `Started::actor` is the first generation
  only and may already be dead when observed.
- **No preemption:** a stuck child keeps its slot and its resources until
  its code returns; Spawned only reports it.
- **`which_children`/`count_children` are snapshots.**
- **No bounded memory per child** (rank 18a) or per mailbox (rank 17).

## Semantics contract: what this adds and touches

New guarantees (section 6 of [semantics.md](semantics.md), ids proposed):

| Id | Guarantee | Checked by (planned) |
|---|---|---|
| V10 | **Dynamic admission.** One control loop serialises starts, exit handling and terminations. A start is admitted atomically with the gate and the count; success commits only while admission is open and is then historical; a rejected start returns its arguments; a first-start failure is returned after cleanup or at its deadline, uncharged and unretried. | `DynamicSupervisor!SerialConstruction`, `!NoStartAfterShutdown`, `!OkMeansReady`, `!FirstStartNeverRetried`, `!FailedAfterCleanup`, `!FailureCauseKept`, `!StartAnswered`; tests |
| V11 | **Cap counts restarting children.** A child holds its slot from admission until removal, through cleanup, backoff, replacement and a stuck cleanup. | `!MaxChildrenBound` |
| V12 | **Terminated means removed.** After `terminate_child` a child is never restarted or republished; a pending restart is cancelled and refunded; `Ok` means removed. | `!TerminatedNeverRestarted`, `!NoPublishAfterStop`, `!ReservationsMatch`, `!TermOkMeansRemoved`, `!TerminateAnswered` |
| V13 | **Concurrent shutdown.** Every live child is signalled without awaiting another; the supervisor is done only after every child finished. | `!ConcurrentShutdown`, `!NoChildOutlivesSupervisor`, `!ClosedLeadsToDone` |
| V14 | **Stuck isolation.** A child stuck while retiring fails only itself; the supervisor fails only for a stuck child it would restart, budget exhaustion, or during shutdown. | `!StuckIsolated` |

Touched:

- **V1** (restart table) per child, including the cleanup-failure rule:
  `!RestartFollowsPolicy` (both directions: no forbidden restart, no
  retirement of a restartable child while open), `!TemporaryNeverRestarted`,
  `!RecoveryProgress`.
- **V3, V4** per slot: `!AtMostOneGeneration`, `!PublishedIsLive` (also
  requiring readiness).
- **V5** (budget): supervisor-wide, start failures excluded, reservations
  modelled: `!ChargedRestarts`, `!ReservationsMatch`, `!BudgetBounded`,
  `!ExhaustionCloses`. Exhaustion stops all children concurrently and exits
  `Shutdown`.
- **V8** (escalation): a dynamic supervisor is a nestable subtree.
- **V9** (shutdown): concurrent instead of reverse order for dynamic
  children; `!NoGenerationAfterClose`.
- **S1, S2**: the dynamic supervisor is a shutdown unit; its admission
  closes with the tree.
- **§10** gains the non-guarantees above; **§11** gains rows 16, 21, 22, 23
  and 26.

No guarantee in sections 1-5 changes.

## Interaction with rank 12 (scoped registries)

**Registry-owned lookup is deferred to rank 12.** Rank 12a ships
`start_child` returning a handle and nothing keyed:

- The two are independent: a dynamic supervisor is useful without keys (one
  actor per accepted socket counted by `max_children`), and a keyed registry
  is useful without supervision.
- Conflict policy for a key is domain policy (ethrex keeps the first
  connection; another user may replace-and-stop), which belongs in the
  registry's design.
- Keys in 12a would make it depend on rank 12's scoping before it exists.

Until rank 12, the pattern depends on the restart type (Codex 13):

**Temporary children** (one generation): the owner's map plus a monitor of
that generation, guarded by the monitor reference.

```rust
let started = peers.start_child(args).await?;
let mref = ctx.monitor(&started.actor.child_handle());
self.by_key.insert(key, Entry { child: started.child, monitor: mref });
// Handler<Down>: remove the entry only if it still holds this monitor.
fn on_down(&mut self, down: Down) {
    if let Some(key) = self.by_monitor.remove(&down.monitor_ref) {
        if self.by_key.get(&key).is_some_and(|e| e.monitor == down.monitor_ref) {
            self.by_key.remove(&key);
        }
    }
}
```

**Restartable children:** the entry belongs to the slot, not to a
generation. Keep the `DynamicChild` in the map until the slot is removed
(learned from `terminate_child`'s result, or from `child.snapshot()` being
terminal), and resolve the current generation with `child.current()` on
each use; or let each generation register itself (`ctx.register_name` in
`started()`, generation-owned, S5). A monitor of one generation only says
that generation died, not that the slot is gone.

**Composition once rank 12 lands:**

- A keyed start is `start_child` plus a generation-owned registration made
  by the child or by the supervisor on readiness, released when that
  generation's cleanup finishes, so an old cleanup can never remove a
  replacement (S5, the ethrex #7435 bug).
- Restartable children re-register per generation; lookups between a crash
  and the replacement's readiness find nothing (V4).
- Duplicate policy is the registry's; `max_children` and the budget stay the
  supervisor's.

## What the model establishes

[`DynamicSupervisor.tla`](../../verification/tla/DynamicSupervisor.tla)
(revision 2) models one dynamic supervisor, N `start_child` calls made at any
moment, and one `terminate_child` per started child. A control loop (`op`)
serialises admission and first construction, exit handling and restart
attempts, and terminations. Each slot's restart type is chosen in `Init`,
so one run covers every combination. The instances use two slots: every
property relates at most a pair of children (one holds the cap or restarts
while the other races it), and with revision 2's detail three slots passed
6.7 GB of TLC state in 38 minutes without finishing. Budget reservations, stamping, refunds and window expiry
are explicit. Exits come from children (`Normal`, `Panic`), stop signals
(`Shutdown`, or `Killed` after the kill), start failures (factory error,
`Normal` before readiness), and cleanup may record a failure (the Transient
rule). A child signalled but still running, or with stuck cleanup, can
become stuck, with the policy above.

Invariants that name an event (a commit, a publication or a generation
after close or terminate; a forbidden restart or retirement; restart
attempts) read history variables that `GhostUpdate` computes from every
step's state change, separately from the actions, so a mutated action cannot
hide its own violation (agy 3). `alive` accumulates generations and only the
one whose cleanup ended is removed (Codex 7).

| Invariant | Meaning |
|---|---|
| `AtMostOneGeneration` | a slot never has two live generations |
| `PublishedIsLive` | `current()` returns only the live, latest, ready generation |
| `NoChildOutlivesSupervisor` | done means every child finished and was removed |
| `MaxChildrenBound` | children held never exceed `max_children` |
| `SerialConstruction` | at most one generation is being constructed |
| `TemporaryNeverRestarted` | a Temporary child has one generation |
| `TerminatedNeverRestarted` | no generation begins after `terminate_child` |
| `NoGenerationAfterClose` | no generation begins after admission closed |
| `NoPublishAfterStop` | no generation is published after close or `terminate_child` |
| `NoStartAfterShutdown` | no `start_child` success commits after admission closed |
| `RestartFollowsPolicy` | every restart is allowed by the table, and no restartable exit is retired while open |
| `FirstStartNeverRetried` | a failed first start is never retried |
| `FailedAfterCleanup` | `FailedToStart`/`Aborted` only after cleanup or at the stuck deadline |
| `FailureCauseKept` | the start error reflects what ended the attempt |
| `ChargedRestarts` | every restart attempt is charged exactly once; first starts never |
| `ReservationsMatch` | reservations are exactly the pending restarts |
| `BudgetBounded` | stamped grants plus reservations stay within `max_restarts` |
| `ExhaustionCloses` | exhaustion fails and closes the supervisor |
| `StuckIsolated` | the supervisor fails only by exhaustion, a stuck child it would restart, or while closing |
| `ConcurrentShutdown` | once stopping, every running child has its stop signal |
| `OkMeansReady` | `Ok` only for a child that became ready |
| `TermOkMeansRemoved` | `terminate_child` returns `Ok` only after removal |

Action property `NeverReopens`. Liveness (fair loop, construction, stops,
cleanup and stuck children ending; crashes, start failures and getting stuck
not fair): `ClosedLeadsToDone`, `StartAnswered`, `TerminateAnswered`,
`RecoveryProgress` (a restartable exit is restarted or superseded by
terminate, shutdown, exhaustion or a stuck cleanup).

TLC results (TLC 2.19, fede, 4 workers, `nice -n 19`):

| Configuration | Instance | Distinct states | Depth | Time |
|---|---|---|---|---|
RESULT_ROWS

Negative controls (`negative_checks.py`, run one at a time; each checks
`TypeOK` and only the named property, on `MCDynamicSupervisor`):

| `Fault` | Mutation | Fails | Origin |
|---|---|---|---|
| `IgnoreMax` | admission ignores the cap | `MaxChildrenBound` | rev. 1 |
| `UncountedRestart` | a crashed child frees its slot until it restarts | `MaxChildrenBound` | rev. 1 |
| `RestartTemporary` | Temporary children follow the Permanent rule | `TemporaryNeverRestarted` | rev. 1 |
| `RestartTerminated` | `terminate_child` does not cancel a restart | `TerminatedNeverRestarted` | rev. 1 |
| `AcceptDuringShutdown` | a start admitted and committed after close | `NoStartAfterShutdown` | rev. 1 |
| `RestartAfterClose` | a pending restart proceeds after close | `NoGenerationAfterClose` | rev. 1 |
| `NoEscalate` | exhaustion drops only that child | `ExhaustionCloses` | rev. 1 |
| `EarlyDone` | done before every child finished | `NoChildOutlivesSupervisor` | rev. 1 |
| `Sequential` | children stopped one at a time | `ConcurrentShutdown` | rev. 1 |
| `ConcurrentStart` | constructions not serialised | `SerialConstruction` | Codex 1 |
| `StuckFailsAll` | any stuck child fails the supervisor | `StuckIsolated` | agy P0 |
| `PublishAfterClose` | a constructing replacement publishes after close | `NoPublishAfterStop` | agy 3 |
| `OverlapGeneration` | the replacement starts before the old cleanup ended | `AtMostOneGeneration` | Codex 7 |
| `PublishBeforeReady` | a replacement is published at construction | `PublishedIsLive` | Codex 7 |
| `PrematureOk` | `Ok` committed at admission | `OkMeansReady` | Codex 7 |
| `UnchargedRetry` | a failed replacement's retry is not charged | `ChargedRestarts` | Codex 7 |
| `ChargeFirstStart` | a first start is charged | `ChargedRestarts` | agy 2 |
| `TransientNormalRestart` | Transient restarts after a normal exit | `RestartFollowsPolicy` | agy 2, Codex 8 |
| `PermanentRetires` | Permanent is not restarted | `RestartFollowsPolicy` | Codex 8 |
| `RetryFirstStart` | a failed first start is retried | `FirstStartNeverRetried` | agy 2 |
| `EarlyFailedReply` | `FailedToStart` returned before cleanup | `FailedAfterCleanup` | agy 2 |
| `LateFailedOverwrite` | a pre-shutdown failure reported as `Aborted` | `FailureCauseKept` | agy 7 |
| `NoRefund` | a cancelled restart keeps its reservation | `ReservationsMatch` | agy 4, Codex 6 |
| `ExpireReserved` | window expiry releases a reservation | `ReservationsMatch` | Codex 6 |
| `TermReplyEarly` | `terminate_child` returns before removal | `TermOkMeansRemoved` | agy 7 |
| `NoAbort` (liveness) | a start racing shutdown is not cancelled | `StartAnswered` | rev. 1 |
| `StuckBackoff` (liveness) | shutdown does not cancel a pending restart | `ClosedLeadsToDone` | rev. 1 |

CONTROLS_NOTE

Not modelled: handler bodies, mailboxes and requests (`Lifecycle.tla`;
rank 11's models); real time (backoff delays, the window clock, deadlines
as durations); native-capacity reservations (a counting resource like the
cap, checked by tests); nesting (a failed dynamic supervisor is one child of
`NestedSupervision.tla`'s parent); the fan-out as separate steps (see
[Shutdown](#shutdown-of-all-children)). Trace validation of real runs is part
of the implementation (verification rule). Apalache inductive invariants
are a follow-up.

## Alternatives considered

1. **Concurrent construction with atomic admission** (revision 1).
   Rejected (Q1, Codex 1): it changes outcomes, not only timing (with a cap
   of one, a start arriving during a failing start is rejected instead of
   admitted), and OTP's own idiom for slow initialisation is to leave it out
   of `init`.
2. **Heterogeneous children as the primary API.** Rejected for v1 (Q2); can
   be added as a separate untyped handle without changing this contract.
3. **`max_children` that waits.** Rejected: Elixir rejects; waiting is rank
   17's admission.
4. **Free the slot at the exit signal.** Rejected: owned cleanup still holds
   resources (B).
5. **Stop children in reverse start order.** Rejected: OTP does not order
   them, and sequential stops multiply shutdown time.
6. **Fail the supervisor on any stuck child** (revision 1, as static V3).
   Rejected (agy P0): a retiring child needs no replacement, so V3 does not
   force it, and OTP's supervisor carries on after killing such a child.
7. **Terminate by slot only.** Rejected: OTP and Elixir terminate by pid;
   both forms are offered.
8. **Keyed registration in 12a.** Rejected; see rank 12 above.
9. **Hold the old scope's capacity lease across a restart** (one reading of
   revision 1's Q4). Rejected (Codex): it would keep the finished scope from
   joining; the reservation is a separate permit owned by the slot.
10. **Return the arguments from every error.** Not possible after the
    factory consumed them; only pre-admission rejections return them.
11. **A `try_reserve()` permit before `start_child`.** Not needed for the
    ethrex migration once rejections return the arguments; it adds permit
    lifetime and cancellation semantics. Later if a user needs it.

## Follow-ups

- **Static nested supervisors exit `Shutdown` on budget exhaustion** (Q5):
  separate PR, changing `nested.rs`'s failure path; `NestedSupervision.tla`
  and semantics V8 accordingly. The nested adapter's default
  `shutdown_timeout` collision (agy 6) applies to static nested supervisors
  too and belongs in that PR.
- **`terminate_child` on static supervisors** (Q6): separate design (OTP
  keeps the spec, `restart_child`, `delete_child`; `Supervision.tla` and
  `Cohorts.tla` changes).
- **Apalache inductive invariants** for `DynamicSupervisor.tla`.
- **Implementation acceptance:** trace validation, the fan-out abstraction
  check, evidence that dynamic tables, retained arguments and capacity
  reservations add nothing to the ordinary actor and message paths (Codex
  closing note).

## Review resolutions

Round 1: Codex (1 P0, 12 P1, 1 P2) and agy (1 P0, 7 P1, 1 P2), both "approve
after the listed changes", run without the author's answers. Each finding
was checked against the code and sources before acting on it.

### Codex

| # | Finding | Resolution |
|---|---|---|
| 1 (P0) | Concurrent construction changes outcomes (cap of one, a failing start), not only timing; not Rust-forced. Also: OTP's supervisor runs the start function, not `init` | **Accepted** (decision Q1). [The control loop](#the-control-loop) serialises starts, exit handling and terminations; model `SerialConstruction`, control `ConcurrentStart`. Sources row corrected |
| 2 | "Never `Ok` after shutdown" conflates commit with observation; `start()` keeps readiness historical | **Accepted.** Success commits only while open and is then historical, possibly with a dead first-generation reference ([`start_child`](#start_child) step 4). The model separates the commit (`Ready`) from anything the caller does later; `NoStartAfterShutdown` is about the commit |
| 3 | Termination identity must survive failed replacement attempts | **Accepted.** The termination key is the last published `ActorId`, kept until a replacement's readiness commits, as OTP and Elixir ([Restart identity](#restart-identity-and-termination-keys)); duplicates and stale ids specified in [Races](#races) |
| 4 | A failed start whose cleanup never joins leaves `start_child` unanswered | **Accepted.** `FailedToStart { cleanup_stuck: true }` at the deadline, slot still counted, work still owned (S2's rule). Model: `StuckDone` is fair only for liveness; `StartAnswered` holds because the stuck step answers |
| 5 | Exhaustion must exit `Shutdown`, not abnormally | **Accepted** (decision Q5): exits `Shutdown` with the diagnostic in `failure()`; static nested supervisors in a follow-up PR |
| 6 | The model omits reservation, stamping and refund | **Accepted.** Explicit `reserved`, `stamped`, `windowed`; reservation at the decision with immediate exhaustion; refund on cancel; expiry only of stamped grants. `ReservationsMatch`, `ChargedRestarts`, `BudgetBounded` (no longer implied by `TypeOK`); controls `NoRefund`, `ExpireReserved`, `UnchargedRetry`, `ChargeFirstStart`, and the per-PR `MCDynamicSupervisorZero` (budget 0) |
| 7 | Invariants too weak: `alive` overwritten, `BudgetBounded` implied by `TypeOK`, `PublishedIsLive` ignores readiness | **Accepted.** `alive` accumulates and loses only the cleaned generation; restart attempts are counted by a history variable; publication requires readiness. Controls `OverlapGeneration`, `UnchargedRetry`, `PublishBeforeReady`, `PrematureOk`. The point that controls prove sensitivity only for their mutation stands; every named invariant now has at least one control |
| 8 | Restart table and progress unverified; configs miss normal pre-readiness replacement failures and the cleanup-failure rule | **Accepted.** `RestartFollowsPolicy` in both directions, `RecoveryProgress`; `StartFails` includes `Normal`, `CleanupFails = {FALSE, TRUE}`, a zero-budget configuration. Controls `TransientNormalRestart`, `PermanentRetires`. "Fence asynchronous events by attempt identity": the model's per-slot generation plays that role; the implementation keeps the existing generation fencing |
| 9 | Concurrent shutdown needs a fan-out and deadline contract; stuck detection before D | **Accepted in the doc, kept abstract in the model.** Fan-out without awaits, each child's intervals from its own signal, timing including the fan-out ([Shutdown](#shutdown-of-all-children)); stuck detection before D (signalled but still running) and after D (cleanup) are both `CleanupStuck` in the model. The one-step fan-out is a documented abstraction, to be checked by trace validation |
| 10 | Retained arguments: clone can panic or block, drop can panic or hang | **Accepted.** [Retained arguments](#retained-arguments); `Sync` dropped |
| 11 | ethrex: the raw `tokio::spawn` writer (`server.rs:1542`) and framework listeners/timers are not covered; "no helpers" is wrong | **Accepted, confirmed in the code.** The client mapping now migrates the writer into an owned helper with a pending-frame policy and states that until then `max_children` bounds slots, not sockets |
| 12 | Monitoring fixes peer-table removal but not `teardown()`'s mempool and announcement cleanup when `stopped()` is skipped | **Accepted, confirmed** (`teardown()` clears in-flight transaction hashes). The mapping assigns each resource an owner that releases it on the `Down`, guarded by the connection's identity |
| 13 | The map-plus-monitor example only works for Temporary children | **Accepted.** Labelled Temporary-only, with the restartable pattern ([rank 12](#interaction-with-rank-12-scoped-registries)) |
| 14 (P2) | Elixir allows `max_children = 0`; pinned groups have no per-actor capacity; OTP's monitors are only for shutdown; termination by slot | **Accepted.** `Some(0)` allowed; pinned groups keep affinity only; links text corrected; `terminate_slot` added |

### agy

| # | Finding | Resolution |
|---|---|---|
| 1 (P0) | One stuck child (even retiring) fails the whole supervisor; a root ethrex supervisor would be bricked. The ethrex text also misstated self-exit timing (one interval, no kill) | **Accepted.** [Stuck children](#stuck-children): a retiring stuck child fails only itself; the supervisor fails only for a stuck child it would restart (V3) or during shutdown. `StuckIsolated`, control `StuckFailsAll`. The mapping's timing statement is corrected and recommends nesting the connection supervisors under the p2p supervisor anyway |
| 2 | No `RestartFollowsPolicy`; first-start rules unchecked | **Accepted.** `RestartFollowsPolicy`, `FirstStartNeverRetried`, `FailedAfterCleanup`, `ChargedRestarts` (covers "uncharged"), with controls. agy's `FailedAfterCleanup` required `alive = {}`; it is `removed` or `stuck`, because of Codex 4 |
| 3 | A constructing replacement can publish after `TerminateChild` or `Close` undetected; `lateOk` only covered first starts and was set by the guarded action | **Accepted in part.** History variables are computed outside the actions (`GhostUpdate`) for commits, publications and generations; `NoPublishAfterStop`, control `PublishAfterClose`. The `TerminateChild` half no longer arises: with the serialised loop, `terminate_child` cannot run while a replacement is being constructed (it waits behind it), which `SerialConstruction` and the loop's ownership enforce |
| 4 | Budget charged after backoff in the model; refund unchecked | **Accepted**, as Codex 6 |
| 5 | `terminate_child` target resolution: contradictions during replacement construction and `Starting`; no way to terminate from `which_children` | **Accepted.** Last-published key; serialisation answers the `Starting` case (a terminate queued behind a start finds the child); `terminate_slot`; `DynamicChildInfo` reports the key and readiness |
| 6 | The nested adapter's 5 s wait collides with the inner 2 × `shutdown_timeout`; redundant constructor arguments | **Accepted.** `DynamicSupervisorSpec::new(name, config, spec)`; default wait 2 × child `shutdown_timeout` + 1 s. The same collision in static nested supervisors goes to the follow-up PR |
| 7 | Stuck first-start cleanup must release `start_child`; a pre-shutdown failure must not be reported as shutdown; in-flight and duplicate terminates | **Accepted.** As Codex 4; `cause` recorded when the attempt ends (`FailureCauseKept`, control `LateFailedOverwrite`); terminate resolution modelled (`TermOkMeansRemoved`, `TerminateAnswered`, control `TermReplyEarly`) |
| 8 | ethrex `Handler<Down>` lacks the monitor check on the entry; awaiting `start_child` stalls accepts; `terminate_child` also skips `stopped()` | **Accepted.** The guard is in both sketches; the handshake moves out of `started()` (required now that starts are serialised); the `stopped()` note covers `terminate_child` |
| 9 (P2) | Jitter seed per slot; drop `Sync`; unconditional `Clone`, slot eviction; structured `FailedToStart` | **Accepted**, all four |

### Open questions for round 2

1. **Holding the loop during `terminate_child`** (OTP blocks the supervisor
   up to 2 × `shutdown_timeout`): faithful, but a slow-to-stop child delays
   every start meanwhile. Keep, or let the loop continue while the
   termination completes?
2. **Stuck children that would restart** fail the supervisor (V3). An
   alternative is to retire such a slot instead (no restart, no failure).
   Failing matches the static supervisor and escalates; retiring would
   silently weaken Permanent.
3. **The one-step fan-out abstraction** in the model: acceptable with trace
   validation, or should the model interleave signals with child steps?
