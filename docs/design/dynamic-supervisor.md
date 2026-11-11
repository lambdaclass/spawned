# Dynamic supervisor: design contract

**Status: design in review, not implemented.** ROADMAP rank 12a. Draft PR
[#245](https://github.com/lambdaclass/spawned/pull/245).

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

Where this design keeps an existing Spawned behaviour that OTP does not have
(backoff, concurrent restarts), it says so and why; those are listed
separately from new deviations.

## Contents

- [Sources](#sources)
- [Goals and non-goals](#goals-and-non-goals)
- [API](#api)
- [Semantics](#semantics)
- [Erlang alignment](#erlang-alignment)
- [Non-guarantees](#non-guarantees)
- [Semantics contract: what this adds and touches](#semantics-contract-what-this-adds-and-touches)
- [Interaction with rank 12 (scoped registries)](#interaction-with-rank-12-scoped-registries)
- [What the model establishes](#what-the-model-establishes)
- [Alternatives considered](#alternatives-considered)
- [Open questions](#open-questions)

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
| `start_child` | `gen_server:call(Sup, …, infinity)`; the child's `init` runs inside the supervisor, so starts are serialised with each other and with `'EXIT'` handling ([L652](https://github.com/erlang/otp/blob/186c0b94da565e4cd234685307b3c3ea41c4af1c/lib/stdlib/src/supervisor.erl#L652), [L1077-1092](https://github.com/erlang/otp/blob/186c0b94da565e4cd234685307b3c3ea41c4af1c/lib/stdlib/src/supervisor.erl#L1077-L1092)) | same; "will block the `DynamicSupervisor` until the child initializes" ([L359](https://github.com/elixir-lang/elixir/blob/b57b70bd96e2e76d66cb81eba6b39c172fa8e8e4/lib/elixir/lib/dynamic_supervisor.ex#L359)) |
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
2. OTP's restart types, supervisor-wide restart budget and escalation.
3. Terminate one child without restarting it.
4. Optional `max_children` that rejects at once, counting restarting
   children.
5. Stop every child concurrently on shutdown, bounded by the per-child
   shutdown timeout and the kill, not by the number of children.
6. Placement on one `ExecutionGroup`; ownership by an `ActorSystem`;
   nesting under a static supervisor.

Non-goals (v1):

- Heterogeneous children (Elixir's any-spec `start_child`). See
  [Alternatives](#alternatives-considered) and open question 2.
- A keyed registry owned by the supervisor (rank 12; see
  [Interaction with rank 12](#interaction-with-rank-12-scoped-registries)).
- Supervisor-type dynamic children (a template that builds a nested
  supervisor per call). Possible later through the same `SupervisorSpec`
  adapter the static supervisor uses.
- `ignore` from a factory (the static supervisor does not have it either,
  S1).
- `Task.Supervisor` (ranks 11 and 15), `PartitionSupervisor` (sharding),
  significant children and `auto_shutdown` (OTP rejects them for
  `simple_one_for_one`).
- Draining children on shutdown (rank 13), bounded mailboxes (rank 17).

## API

### Template

```rust
/// One template for every child: placement, factory, restart type and limits.
/// The factory gets the per-call arguments (OTP's ExtraArgs).
pub struct DynamicChildSpec<A: Actor, Args> { /* name, group, factory, restart, timeouts */ }

impl<A: Actor, Args: Clone + Send + Sync + 'static> DynamicChildSpec<A, Args> {
    /// Permanent, as OTP's default. Restartable children keep their
    /// arguments for the next generation, hence `Clone`.
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
    /// they need not be `Clone`: a `TcpStream` or a semaphore permit works.
    pub fn temporary<F, U, E>(name: impl Into<String>, group: &ExecutionGroup, factory: F) -> Self;
}

impl<A: Actor, Args> DynamicChildSpec<A, Args> {
    pub fn startup_timeout(self, limit: Duration) -> Self;   // default 5 s
    pub fn shutdown_timeout(self, limit: Duration) -> Self;  // default 5 s
}
```

Two constructors instead of one is a Rust-forced shape (R): a restartable
child must be rebuilt from its arguments, so they must be cloneable, while a
temporary one must accept move-only values. `RestartMode::Temporary` passed
to `restart()` is still allowed (it simply never clones). The restart type is
per template, as in `simple_one_for_one`.

### Starting the supervisor

```rust
pub struct DynamicSupervisorConfig {
    pub max_restarts: usize,          // default 5
    pub restart_window: Duration,     // default 30 s
    pub max_children: Option<usize>,  // default None (Elixir :infinity)
    pub backoff: Backoff,             // default as SupervisorConfig
}

// Root, owned by the system (shutdown unit in admission order, S2):
let peers: DynamicSupervisor<PeerConnection, PeerArgs> =
    system.dynamic_supervisor(config, spec)?.start().await?;

// Nested under a static supervisor, as `add_supervisor` (V8):
let slot: SupervisedDynamicSupervisor<PeerConnection, PeerArgs> =
    builder.add_dynamic_supervisor(DynamicSupervisorSpec::new("peers", &group, config, || spec()))?;
let peers = slot.current()?;   // re-resolve after the parent restarts it
```

`start()` returns once the supervisor is admitted; it has no children to
start, so it cannot fail with `FailedToStartChild`. Configuration errors
(`max_children = Some(0)`, a zero window, `backoff.maximum < initial`, a
zero timeout, a foreign group) are `SupervisorError::Configuration` or
`System(ForeignGroup)` at construction, as for the static supervisor. The
`max_children <= 65536` ceiling of `SupervisorConfig` does not apply: the
static ceiling bounds a declaration list, while the dynamic count is the
user's explicit choice.

### Operations

```rust
impl<A: Actor, Args> DynamicSupervisor<A, Args> {
    /// OTP start_child(Sup, ExtraArgs). Resolves when the first generation
    /// is ready (started() returned) or failed.
    pub async fn start_child(&self, args: Args) -> Result<Started<A>, StartChildError>;
    /// OTP terminate_child(Sup, Pid). Resolves after the child finished.
    pub async fn terminate_child(&self, id: ActorId) -> Result<(), TerminateChildError>;
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

/// Like `SupervisedChild<A>` for a dynamic slot.
impl<A: Actor> DynamicChild<A> {
    pub fn id(&self) -> DynamicChildId;             // stable for the slot
    pub fn current(&self) -> Result<ActorRef<A>, ChildUnavailable>;
    pub async fn wait_for_generation(&self, after: u64) -> Result<ActorRef<A>, ChildUnavailable>;
    pub fn snapshot(&self) -> ChildSnapshot;
    /// terminate_child for whatever generation is current, including a
    /// restart in backoff.
    pub async fn terminate(&self) -> Result<(), TerminateChildError>;
}

#[non_exhaustive]
pub enum StartChildError {
    /// max_children reached (Elixir {:error, :max_children}). Nothing queued.
    MaxChildren,
    /// The supervisor is shutting down or has stopped (OTP: the caller's
    /// call exits with the supervisor's reason, or noproc).
    Stopped,
    /// The first generation failed: factory error or panic, panic or stop
    /// in started(), startup_timeout. The child is not kept and nothing is
    /// charged to the budget (OTP {error, Reason}).
    FailedToStart { cause: String },
    /// Placement: the group is closed or at capacity.
    System(SystemError),
}

#[non_exhaustive]
pub enum TerminateChildError {
    /// Not a current child of this supervisor (Elixir {:error, :not_found}).
    NotFound,
    /// The supervisor is shutting down; it stops the child itself.
    Stopped,
    /// The child's cleanup missed its deadlines; the supervisor failed (V3).
    CleanupStuck,
}

pub struct DynamicChildInfo {
    pub child: DynamicChildId,
    pub actor: Option<ActorId>,   // None while restarting (OTP `restarting`)
    pub state: ChildState,
    pub generation: u64,
}

pub struct ChildCounts {
    pub specs: usize,        // 1, as OTP simple_one_for_one
    pub active: usize,       // children with a live, ready generation
    pub supervisors: usize,  // 0 in v1 (no supervisor-type children)
    pub workers: usize,      // every child, restarting ones included
}
```

`ChildState` gains no variant: a dynamic child is `Starting`, `Running`,
`Backoff` (restart pending), `Stopping`, then gone from `which_children`.
`which_children` is a snapshot in no particular order, as OTP's.

### Restart types and defaults

| | OTP | This design |
|---|---|---|
| Default restart type | `permanent` | `Permanent` (`DynamicChildSpec::new`); `temporary()` for one-shot children such as connections |
| Restart table | `do_restart/3` | unchanged V1 table: Permanent always; Transient on an abnormal reason (`Panic`, `Kill`, `Killed`, `NoProc`) or a cleanup failure (exit-signals design); Temporary never |
| A replacement failing before readiness | `try_again_restart`, retried | retried unless Temporary (V1), each attempt charged |
| Explicit `terminate_child` | never restarted | never restarted, any type |

### Restart budget

- **Scope:** one rolling window per supervisor, shared by all its children,
  as OTP's intensity and period.
- **What counts:** every restart attempt: the factory call of every
  generation after a child's first, including retries of a replacement that
  failed before readiness (OTP calls `add_restart` before each restart and
  each `try_again_restart`). A dynamic supervisor has no collateral
  restarts, so v0.7's "every factory call" and rank 5f's "recovery rounds"
  give the same count here.
- **What does not count:** a failed `start_child` (OTP), a Temporary child's
  exit, an exit after `terminate_child`, exits during shutdown.
- **Defaults:** `max_restarts = 5`, `restart_window = 30 s`, the static
  supervisor's defaults. OTP uses 1 in 5 s and Elixir 3 in 5 s. Spawned's
  is looser because per-child exponential backoff (V6) already spreads
  restarts: a crash-looping child still uses up 5 grants in well under a
  second (backoff bounds 10, 20, 40, 80, 160 ms), so the looser window does
  not let a loop run long, while a supervisor of many children tolerates
  more unrelated crashes. Open question 3.
- **Exhaustion:** the supervisor fails (`failure()` records
  "restart budget exhausted" with the child and its last reason), closes
  admission, stops every child as in a shutdown and escalates: a nested
  dynamic supervisor exits abnormally and its parent applies its own policy
  (V8); a root supervisor reports it in `ShutdownReport::supervisor_failures`.
  OTP exits `shutdown` here; see [alignment](#erlang-alignment) row 15.
- **Reservation:** as V5, capacity is reserved before backoff and refunded if
  the backoff is cancelled (by `terminate_child` or shutdown). An exhausted
  window fails at once without waiting for the backoff.

### `max_children`

- `None` by default (Elixir `:infinity`). `Some(n)` caps the number of
  children the supervisor holds: starting, running, cleaning up, in backoff
  or constructing a replacement. A slot is freed only when its child is
  removed: its last generation finished cleanup (milestone 7 of the exit
  phases) and no restart follows.
- Checked only by `start_child`, atomically with admission. A full
  supervisor returns `Err(MaxChildren)` at once; nothing waits or queues.
- Restarts are never re-checked (Elixir) and never need to be: the slot they
  use was never freed. So a new start cannot take the place of a child
  that is restarting, and a restart cannot fail for want of a slot.
- Spawned frees the slot after cleanup, not at the exit signal (OTP removes
  the child when it processes the `'EXIT'`). B: the cap then bounds what is
  really held (state, sockets, helpers), not only live processes.

### Placement and worker budgets

- Every child runs on the template's `ExecutionGroup`; factories run there
  too, as for the static supervisor. The group must belong to the
  supervisor's `ActorSystem` (`ForeignGroup` otherwise).
- Async groups: children share the group's workers; `max_children` is the
  bound on how many there are. A dynamic supervisor does not change worker
  budgets; the supervisor's own bookkeeping runs on the system's control
  runtime, outside every group budget (as today).
- `thread_group(n)` (one thread per live actor, `Capacity` beyond `n`) and
  pinned groups: a child's slot holds its group lease from admission until
  removal, across restarts, so a restart is never refused for capacity that
  a newer `start_child` took. A start that finds the group full returns
  `Err(System(Capacity))` and takes no slot. Implementation note: this needs
  a lease that outlives one actor instance; today a lease is per live scope.
- A closed group (system shutdown) makes `start_child` return
  `Err(System(Closed))` and fails restarts, which fails the supervisor (as
  static supervisors treat group closure).

### Nesting and the system

- Root: `system.dynamic_supervisor(config, spec)` is a shutdown unit in
  admission order, stopped in its turn by `ActorSystem::shutdown` (S2); its
  children stop concurrently in that turn.
- Under a static supervisor: `add_dynamic_supervisor` uses the nested
  adapter of `add_supervisor` (V8). The parent sees one child; it stops it
  in reverse declaration order like any child; a restart by the parent
  creates a new, empty dynamic supervisor (OTP: a restarted
  `simple_one_for_one` supervisor starts with no children). Old
  `DynamicSupervisor` handles keep pointing at the dead instance and return
  `Stopped`; `SupervisedDynamicSupervisor::current()` resolves the new one.
- A dynamic supervisor's children are actors in v1. A child may itself start
  a static supervisor in its factory only as unmanaged work, which is not
  supported; nested supervisor templates are a follow-up.

## Semantics

### `start_child`

1. **Admission** is one atomic step on the supervisor's gate: if the
   supervisor is closed (shutdown, failure or exhaustion began), `Stopped`;
   else if `max_children` is reached, `MaxChildren`; else the group lease is
   reserved (`System(..)` if it cannot be) and a slot is created in
   `Starting`, counted from this moment.
2. **Construction** runs the factory on the group and then `started()`,
   bounded by `startup_timeout`, as a static child's first generation.
   Concurrent `start_child` calls construct concurrently (see
   [alignment](#erlang-alignment) row 2).
3. **Commit.** Readiness commits under the gate only while it is open. Then
   `start_child` returns `Ok(Started)` and the child is supervised.
4. **First-start failure.** Factory error or panic, panic or stop in
   `started()`, or `startup_timeout`: the generation is cleaned up, the slot
   removed, `Err(FailedToStart { cause })` returned after that cleanup (S2's
   "after rollback" rule, for one child). Never retried, never charged to the
   budget, whatever the restart type (OTP returns `{error, Reason}`).
5. **Cancellation.** Dropping the `start_child` future does not cancel the
   start (as R4: giving up never cancels). The child, once ready, is
   supervised like any other and visible in `which_children`. OTP behaves the
   same when the caller of `start_child` dies.

### Races

| Race | Outcome |
|---|---|
| `start_child` vs shutdown, admission first | the attempt is cancelled at shutdown: if `started()` had not begun, the state is dropped without hooks (L2); otherwise the generation gets the parent exit signal like a running child. `start_child` returns `Stopped`. It never returns `Ok` after shutdown began (model: `NoStartAfterShutdown`) |
| `start_child` vs shutdown, shutdown first | `Stopped` at admission; nothing is constructed |
| `start_child` vs a sibling's crash | the crashed child keeps its slot through cleanup, backoff and the replacement, so at the cap the new start gets `MaxChildren` (model: `MaxChildrenBound`, control `UncountedRestart`) |
| `start_child` vs a sibling's removal | the slot frees only after the removed child's cleanup; a start before that sees it counted |
| `start_child` vs budget exhaustion | exhaustion closes the gate; as shutdown |
| `terminate_child` vs a crash of that child | whichever is first under the slot lock wins. Crash first: if a restart is pending, `terminate_child` cancels it (refunding the budget) and removes the child; if the replacement is constructing, it is cancelled. Terminate first: the crash reason is recorded, no restart. Either way the child is removed and never restarted (model: `TerminatedNeverRestarted`) |
| `terminate_child` vs shutdown | after shutdown began, `Stopped`: the supervisor is already stopping every child |
| `terminate_child(id)` with an old generation's id | `NotFound` once that generation is not the slot's current one (a newer generation started); during backoff the last generation's id still names the slot. `DynamicChild::terminate()` targets the slot whatever its generation |

### Restart identity

- Every generation has a new `ActorId`, mailbox and state (M4). The slot's
  `DynamicChildId` does not change; `DynamicChild::current()` returns the
  latest ready generation, `wait_for_generation(n)` waits for a newer one
  (V4). Old `ActorRef`s stay bound to their instance and get `NoProc` (v0.8).
- `which_children` shows the current `ActorId`, or `None` while restarting
  (OTP's `restarting`).
- Callers that look children up by a domain key (a peer id) keep their own
  map from key to `DynamicChild` until rank 12 provides keyed registration;
  see [rank 12](#interaction-with-rank-12-scoped-registries).

### Terminating one child

`terminate_child` is OTP's `shutdown/2` for one child, under the exit-signal
design's E3 row for a supervisor stopping a child:

1. Mark the slot terminated (no restart from now on), cancel a pending
   backoff or replacement.
2. Send the parent exit signal `Shutdown`: a non-trapping child begins exit
   after its current handler and skips `stopped()` (E2, decision 2); a
   trapping child handles the messages queued before the signal, then runs
   `stopped()` (E3).
3. If the child has not reached its death point `shutdown_timeout` after the
   signal, kill it (`Killed`, E1); if it misses a second `shutdown_timeout`
   the cleanup is stuck: the supervisor fails (V3) and `terminate_child`
   returns `CleanupStuck`.
4. Resolve after the child's cleanup finished and the slot was removed.

### Shutdown of all children

When the supervisor closes (`stop`, `shutdown(deadline)`, system shutdown,
its parent stopping it, or a failure):

1. Admission closes: `start_child` returns `Stopped`; pending restarts and
   constructions are cancelled (budget refunded); starts in construction are
   cancelled as in [Races](#races).
2. **Every live child gets the parent exit signal at once**, not in reverse
   order: dynamic children have no declared order (OTP: "not defined") and
   sequential stops would make shutdown take up to N × `shutdown_timeout`.
   Model: `ConcurrentShutdown`, control `Sequential`.
3. Each child then follows the E3 deadlines from that one moment: kill after
   `shutdown_timeout`, stuck after a second one. As every child shares the
   template's timeout, this is OTP's single timer. Shutdown therefore takes
   at most 2 × `shutdown_timeout` plus cleanup, whatever N is.
4. **Done** means every slot was removed: every generation reached
   publication and its owned cleanup finished (V9 "done"). Model:
   `NoChildOutlivesSupervisor`, `ClosedLeadsToDone`.
5. `shutdown(deadline)` keeps the static meaning: `deadline` is the report
   deadline; at it the report is returned with `complete = false` if
   children remain, and they stay owned by the system (S3).

### In-flight requests and deferred replies of a dying child

Nothing is specific to dynamic supervision; the child dies as any actor
(exit-signals design, phases of an exit, with rank 11):

- At the child's death point D every queued request and every open
  `ReplyTo` token is answered `ActorStopped(final reason)`: `Shutdown` after
  a clean stop, `Killed` after the kill, the crash reason after a crash. A
  request or registration after D gets `NoProc`.
- A non-trapping child stopped by its supervisor skips `stopped()`, so
  deferred tokens it stored are answered at D, not by `stopped()` code.
  Cleanup that must always run belongs in `Drop` or needs `trap_exit` (the
  exit-signals migration note).
- Pipe jobs of the child are cancelled at exit begin and joined after
  publication (rank 11 step 4), within the cleanup deadline; the slot is
  freed only after that join.
- A replacement never receives the old generation's queue, tokens or jobs
  (V4; rank 11 `ActorId` fencing).

### Links and monitors

- The supervisor is the parent of every child generation (E3), so its
  `Shutdown` is a parent exit signal. It is not an actor: it observes
  children through internal completion state, not links or a mailbox (V9),
  so a child's backlog never delays its restart decision and a child cannot
  unlink itself from supervision (OTP guards against such "naughty" children
  with monitors; here there is nothing to unlink).
- Children can link to and monitor each other and outside actors as usual;
  supervision does not add or remove those edges. A link between two
  children propagates a crash under X3 and both are then handled by the
  restart table independently (one for one).
- Outside actors that track children should monitor them
  (`ctx.monitor(&started.actor.child_handle())`) rather than rely on the
  child's `stopped()`: the `Down` arrives exactly once with the final reason,
  also after a kill, a skipped `stopped()` or a runtime drop. This is what
  replaces ethrex's self-reported removal ([client mapping](dynamic-supervisor-client-mapping.md)).

### Budget exhaustion takes everything down

Exhaustion is a supervisor failure: it closes admission and stops every
child exactly as a shutdown, then escalates (V8). Nothing restarts after the
failure (model: `ExhaustionCloses`, `NoGenerationAfterClose`, control
`NoEscalate`). A Temporary child never consumes budget, so a supervisor of
Temporary children (ethrex's case) fails only on a stuck cleanup or a closed
group.

## Erlang alignment

| # | Rule | OTP / Elixir | This design | Reason for any deviation |
|---|---|---|---|---|
| 1 | `start_child` is synchronous and returns the child | `{ok, Pid}` after `init` | `Ok(Started { actor, child })` after `started()` | aligned; `child` is an addition (row 9) |
| 2 | Starts are serialised in the supervisor | the supervisor runs each child's `init` and blocks meanwhile | admission is serialised (atomic count and gate); construction runs concurrently, bounded by `startup_timeout` | kept Spawned behaviour, not R/T/B: the supervisor is not a process, and static supervisors already restart children concurrently (v0.7). Observable difference is timing only: a slow `started()` does not delay other starts or restart decisions. Open question 1 |
| 3 | Template plus per-call args | `ExtraArgs` appended (Elixir: fixed `extra_arguments` prepended) | typed `Args` passed to the factory | T: one typed argument instead of a term list |
| 4 | `ignore` | `{ok, undefined}`, nothing stored | not supported | as the static supervisor (S1); a factory cannot return it |
| 5 | Start failure | `{error, Reason}`, not counted, not retried | `FailedToStart { cause }`, not counted, not retried, returned after cleanup | aligned (S2 ordering) |
| 6 | Default restart type | `permanent` | `Permanent` | aligned |
| 7 | Temporary children keep no args | `mapsets` | `temporary()` moves args once, no `Clone` | aligned; R for the separate constructor |
| 8 | `terminate_child` | by pid; unknown dead pid `ok` (OTP) or `not_found` (Elixir) | by `ActorId`; `NotFound` for anything not current | Elixir's answer: OTP's `ok` for a dead unknown pid is an artefact of `find_child` |
| 9 | Restart identity | new pid, found through names | new `ActorId`; stable `DynamicChildId` and `DynamicChild::current()` | addition, consistent with `SupervisedChild` (references are instance-bound, an accepted decision); no OTP behaviour changes |
| 10 | `restart_child`, `delete_child` | `{error, simple_one_for_one}` | not provided | aligned (absent at compile time: T) |
| 11 | `which_children` | pid or `restarting`, unordered | `Option<ActorId>`, unordered, with state and generation | aligned plus fields |
| 12 | `count_children` | `specs` 1, `active` excludes restarting | same | aligned with OTP (Elixir's `specs` differs) |
| 13 | `max_children` | Elixir: default infinity, start only, counts restarting | same; frees after cleanup | aligned with Elixir; B for freeing after cleanup |
| 14 | Restart budget | intensity/period, every restart attempt counts, start failures do not | same rule; defaults 5 in 30 s | defaults: kept Spawned behaviour (backoff, V6); open question 3 |
| 15 | Exhaustion | terminate children, supervisor exits `shutdown` | terminate children concurrently, fail, escalate; the nested adapter exits abnormally (as static nested supervisors today) | existing static-supervisor difference, not introduced here: a `Transient` OTP supervisor that exhausts its budget is not restarted by its parent, a Spawned one is. Open question 5 |
| 16 | Shutdown order | parallel, undefined order, one timer, then `kill` | parallel, per-child `shutdown_timeout` from one moment, then `kill`, then stuck | aligned; the stuck stage is R (Rust cannot kill a thread; exit-signals design) |
| 17 | Shutdown signal | `exit(Child, shutdown)`; trapping child runs `terminate` | parent exit signal: trapping child runs `stopped()` after its backlog; non-trapping skips `stopped()` | aligned (E2, E3) |
| 18 | `start_child` during shutdown | call queued, caller exits with the supervisor's reason or `noproc` | `Err(Stopped)` | R: Rust has no caller exit; a typed error is the counterpart |
| 19 | Restarted dynamic supervisor | empty | empty | aligned |
| 20 | Backoff | none: restart at once | per-child backoff (V6) | kept Spawned behaviour; budget refunded if cancelled |
| 21 | Shutdown admission | a stopping supervisor's children may start anything | the whole tree closes admission at once | B (exit-signals design) |
| 22 | Cleanup before a slot frees or a replacement starts | the process is gone at `'EXIT'` | after owned cleanup (V3) | R: destructors and owned helpers outlive the last hook |
| 23 | Restarted-child calls in flight | `'EXIT'`/monitor tells callers | `ActorStopped(reason)` at D, `NoProc` after (rank 11, v0.8) | aligned |
| 24 | Heterogeneous specs | Elixir: any spec per call | one typed template | T: `start_child` returns `ActorRef<A>`; open question 2 |

## Non-guarantees

- **No order** among dynamic children, at start or shutdown, and none in
  `which_children`.
- **No queueing** at `max_children`: a full supervisor rejects. Waiting for
  a slot is rank 17's admission.
- **No exactly-once start under cancellation:** a dropped `start_child`
  future may still produce a supervised child.
- **No state or queue carried across a restart** (V4); nothing replays
  messages sent to an old generation.
- **No transparent reference:** `Started::actor` is the first generation
  only.
- **No preemption:** a child stuck in a handler or `stopped()` is killed
  cooperatively; a child whose cleanup never ends fails the supervisor and
  keeps the shutdown report incomplete (S3), as today.
- **No fairness between `start_child` callers** beyond the atomic admission
  step; under contention for the last slot any one may win.
- **`which_children`/`count_children` are snapshots**; they can be stale by
  the time they are read (OTP's "likely to be very accurate").
- **No bounded memory per child** (rank 18a) or per mailbox (rank 17).

## Semantics contract: what this adds and touches

New guarantees (section 6 of [semantics.md](semantics.md), ids proposed):

| Id | Guarantee | Checked by (planned) |
|---|---|---|
| V10 | **Dynamic admission.** `start_child` is admitted atomically with the gate and the count; it never returns `Ok` after shutdown began; a full supervisor rejects with `MaxChildren`; a first-start failure returns `FailedToStart` after cleanup, uncharged and unretried. | `DynamicSupervisor!NoStartAfterShutdown`, `!MaxChildrenBound`, `!OkMeansReady`, `!StartAnswered` (liveness); tests |
| V11 | **Cap counts restarting children.** A child holds its slot from admission until removal, through cleanup, backoff and replacement. | `DynamicSupervisor!MaxChildrenBound` |
| V12 | **Terminated means removed.** After `terminate_child` a child is never restarted, whatever its type; a pending restart is cancelled and refunded. | `DynamicSupervisor!TerminatedNeverRestarted`, `!TerminateCompletes` |
| V13 | **Concurrent shutdown.** Every live child gets its stop signal at once; the supervisor is done only after every child finished. | `DynamicSupervisor!ConcurrentShutdown`, `!NoChildOutlivesSupervisor`, `!ClosedLeadsToDone` |

Touched:

- **V1** (restart table): applies per child; the Temporary rule also means no
  retained arguments. `DynamicSupervisor!TemporaryNeverRestarted`.
- **V3, V4** (one live generation, publication): per slot.
  `DynamicSupervisor!AtMostOneGeneration`, `!PublishedIsLive`.
- **V5** (budget): supervisor-wide, start failures excluded; exhaustion
  stops all children concurrently instead of in reverse order.
  `DynamicSupervisor!BudgetBounded`, `!ExhaustionCloses`.
- **V8** (escalation): a dynamic supervisor is a nestable subtree.
- **V9** (shutdown): concurrent instead of reverse declaration order for
  dynamic children; "done" unchanged. `!NoGenerationAfterClose`.
- **S1, S2** (admission, ordered shutdown): the dynamic supervisor is a
  shutdown unit; its own admission closes with the tree.
- **§10** gains the non-guarantees above; **§11** gains rows 2, 14, 15, 20
  of the alignment table.

No guarantee in sections 1-5 changes.

## Interaction with rank 12 (scoped registries)

**Decision: registry-owned lookup is deferred to rank 12.** Rank 12a ships
`start_child` returning a handle and nothing keyed. Reasons:

- The two are independent: a dynamic supervisor is useful without keys (a
  pool of workers, one actor per accepted socket counted by `max_children`),
  and a keyed registry is useful without supervision (rank 12's own client
  evidence is a peer map whose entries need generation guards).
- What a key means on conflict is domain policy. ethrex keeps the first
  connection and rejects a duplicate ("Peer already connected, don't
  replace it"); another user may want replace-and-stop-old. That belongs in
  the registry's design, not in the supervisor's.
- Folding keys into 12a would make the dynamic supervisor depend on rank 12's
  scoping (system-scoped instead of the process-global registry) before it
  is designed.

Until rank 12, the pattern is the application map plus a monitor:

```rust
// In the owner (a peer table actor):
let started = peers.start_child(args).await?;
let mref = ctx.monitor(&started.actor.child_handle());
self.by_key.insert(key, Entry { child: started.child, monitor: mref });
// Handler<Down>: remove the entry only if it still holds that monitor.
```

The `Down`'s `MonitorRef` (or the `ActorId`, never reused, M4) is the
generation guard that ethrex's `ConnectionId` hand-builds, and the `Down` is
delivered exactly once whatever ends the child (X1), unlike a message sent
from `stopped()`.

**How they compose once rank 12 lands:**

- A keyed start is `start_child` plus a generation-owned registration made
  by the child (in `started()`, as `ctx.register_name` already does, S5) or
  by the supervisor on readiness. The registration is released when that
  generation's cleanup finishes, so an old cleanup can never remove a
  replacement (S5, the ethrex #7435 bug).
- For restartable children, each generation re-registers under the same key
  with a new generation; lookups between the crash and the replacement's
  readiness find nothing (instance-bound references, V4).
- Duplicate policy (reject or replace) is the registry's; `max_children` and
  the budget stay the supervisor's.
- The rank 12 model (`an old entry's cleanup never removes its replacement`)
  composes with `DynamicSupervisor.tla` through the slot's generations.

## What the model establishes

[`DynamicSupervisor.tla`](../../verification/tla/DynamicSupervisor.tla)
models one dynamic supervisor and N `start_child` calls, each made at most
once at any moment, so starts race crashes, restarts, `terminate_child`,
`max_children`, budget exhaustion and shutdown. Restart types are per slot so
one instance covers all three. Time is abstract: the window is a counter
released at most `MaxExpire` times, which also bounds the generations (no
state constraint); the kill escalation is the nondeterministic reason
(`Shutdown` or `Killed`) of a stopped child; a cleanup can get stuck
(`CleanupStuck`, not fair), which fails the supervisor.

Invariants:

| Invariant | Meaning |
|---|---|
| `AtMostOneGeneration` | a slot never has two live generations |
| `PublishedIsLive` | `current()` returns only the live, latest generation |
| `NoChildOutlivesSupervisor` | done means every child finished and was removed |
| `MaxChildrenBound` | children held (restarting and cleaning up included) never exceed `max_children` |
| `TemporaryNeverRestarted` | a Temporary child has one generation |
| `TerminatedNeverRestarted` | no generation starts after `terminate_child` |
| `NoStartAfterShutdown` | no `start_child` returns `Ok` after shutdown began |
| `NoGenerationAfterClose` | no generation (start or restart) begins after admission closed |
| `ExhaustionCloses` | budget exhaustion fails and closes the supervisor |
| `BudgetBounded` | grants in the window stay within `max_restarts` |
| `ConcurrentShutdown` | once stopping, every running child has its stop signal |
| `OkMeansReady` | `Ok` only for a child that became ready |

Action property `NeverReopens`. Liveness (fair construction, stop, cleanup
and supervisor steps; crashes and stuck cleanup not fair):
`ClosedLeadsToDone`, `StartAnswered`, `TerminateCompletes`.

TLC results (TLC 2.19, fede, 4 workers, `nice -n 19`):

| Configuration | Instance | Distinct states | Depth | Time |
|---|---|---|---|---|
| `MCDynamicSupervisor` (every PR; safety, action property, liveness) | 3 slots (Permanent, Transient, Temporary), `max_children` 2, budget 1, one window release, exits `Normal`/`Panic`, start failures factory/`Panic` | RESULTS_FAST | | |
| `MCDynamicSupervisorDeep` (weekly; safety, action property) | 4 slots (P, T, Temp, P), `max_children` 3, budget 2, two releases, one reason per class (the decisions depend only on normal versus abnormal; with every reason the instance passed 12 GB of TLC state without finishing) | RESULTS_DEEP | | |

Negative controls (`negative_checks.py`; each checks `TypeOK` and only the
named property, on `MCDynamicSupervisor`):

| `Fault` | Mutation | Fails | Result |
|---|---|---|---|
| `IgnoreMax` | admission ignores the cap | `MaxChildrenBound` | counterexample |
| `UncountedRestart` | a crashed child frees its slot until it restarts (counts only starting and running children) | `MaxChildrenBound` | counterexample |
| `RestartTemporary` | Temporary children follow the Permanent rule | `TemporaryNeverRestarted` | counterexample |
| `RestartTerminated` | `terminate_child` does not cancel a restart | `TerminatedNeverRestarted` | counterexample |
| `AcceptDuringShutdown` | `start_child` admitted and committed after close | `NoStartAfterShutdown` | counterexample |
| `RestartAfterClose` | a pending restart proceeds after close | `NoGenerationAfterClose` | counterexample |
| `NoEscalate` | budget exhaustion drops only that child | `ExhaustionCloses` | counterexample |
| `EarlyDone` | done before every child finished | `NoChildOutlivesSupervisor` | counterexample |
| `Sequential` | children stopped one at a time (static order) | `ConcurrentShutdown` | counterexample |
| `NoAbort` | a start racing shutdown is not cancelled | `StartAnswered` (liveness) | counterexample |
| `StuckBackoff` | shutdown does not cancel a pending restart | `ClosedLeadsToDone` (liveness) | counterexample |

All eleven produce their counterexample in under 5 s each.

Not modelled: handler bodies, mailboxes and requests (the child's own death
is `Lifecycle.tla`'s; replies are rank 11's models); real time (backoff
delays, the window's clock); group capacity and leases; nesting (a failed
dynamic supervisor is one abnormal child of `NestedSupervision.tla`'s
parent). Trace validation of real runs against this model, as
`TraceSupervision.tla` does for OneForOne, is part of the implementation
(verification rule). Apalache inductive invariants are follow-up work: the
model needs type annotations and an `IndInv` with helper lemmas like
`SupervisionInd.tla`.

## Alternatives considered

1. **Elixir-style heterogeneous children as the primary API**
   (`start_child(ChildSpec<B>)` on an untyped supervisor). Rejected for v1:
   every call site would carry its own spec and restart type, which the one
   client does not need, and a typed template gives `ActorRef<A>` directly.
   It can be added later as a separate untyped handle without changing this
   contract (the model already allows per-slot restart types). Open
   question 2.
2. **Serialise starts like OTP.** Rejected (open question 1): one slow
   handshake in `started()` would block every other start and every restart
   decision, and static supervisors already restart concurrently. The OTP
   idiom for this is `handle_continue`, which Spawned users get by keeping
   `started()` short and doing the handshake in a handler.
3. **Make `max_children` wait instead of reject.** Rejected: Elixir rejects,
   and waiting is rank 17's admission with its own deadlines and credits.
4. **Free the slot at the exit signal, as OTP removes the child at
   `'EXIT'`.** Rejected: owned cleanup (helpers, destructors) still holds
   resources, and the cap is meant to bound them (B).
5. **Stop children in reverse start order, as the static supervisor.**
   Rejected: OTP does not order them, and N sequential stops multiply the
   shutdown time (control `Sequential`).
6. **Terminate by slot only (no `ActorId` variant).** Rejected: OTP and
   Elixir terminate by pid, and a stale id must not hit a newer generation
   silently; the slot variant (`DynamicChild::terminate`) is offered too.
7. **Return only the `ActorRef` from `start_child`.** Rejected: restartable
   children need a way to find the next generation without a registry
   (rank 12); the `DynamicChild` handle is that way, as `SupervisedChild` is
   for static children.
8. **Put keyed registration in 12a.** Rejected; see
   [rank 12](#interaction-with-rank-12-scoped-registries).
9. **Add `terminate_child` to static supervisors in the same change.** The
   ROADMAP research note asks for it (today an individually stopped
   Permanent child restarts). It is the same mechanism (mark removed, parent
   exit, kill, remove) but changes the static API and `Supervision.tla`;
   proposed as a follow-up, open question 6.

## Open questions

1. **Concurrent construction** (alignment row 2). Keep concurrent starts
   (proposed), or serialise factory plus `started()` per supervisor as OTP
   does?
2. **Heterogeneous children.** Is the typed template enough for v1, or
   should an untyped `start_child(ChildSpec<B>)` ship with it?
3. **Default budget and cap.** Keep the static defaults (5 per 30 s,
   `max_children = None`), or use Elixir's 3 per 5 s? Should the dynamic
   supervisor default to a finite `max_children` (B), given that every
   other Spawned bound is opt-in today?
4. **Group leases across restarts.** Is holding a thread-group lease from
   admission to removal acceptable, or should `max_children` simply be
   required to fit the group's capacity?
5. **Exit reason of an exhausted supervisor.** OTP exits `shutdown` (so a
   Transient parent does not restart it); Spawned's nested adapter exits
   with a panic reason. Align both static and dynamic supervisors with OTP
   (a separate change), or keep it?
6. **`terminate_child` on static supervisors.** Add it in the same release
   (ROADMAP research note), or later?
7. **Rejected arguments.** `MaxChildren`, `Stopped` and `System` errors drop
   the arguments (ethrex's `TcpStream` closes without an RLPx Disconnect, as
   today). Should these errors hand the arguments back (`Err((error, args))`),
   or should a `try_reserve()` permit precede `start_child`?
