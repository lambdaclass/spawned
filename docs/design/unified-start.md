# Unified start API (0.9): design contract

Status: design in review, not implemented. Target release 0.9. The
implementation follows after two independent reviews. It is designed against
the 0.8 API as #240 ships it (exit signals, `try_link`, `NoProc`, the 0.8
API-review cleanups: `send_request`, `#[non_exhaustive]` builders).

- Model: [`verification/tla/Start.tla`](../../verification/tla/Start.tla)
  (configurations `MCStart`, `MCStartContinue`, `MCStartLive`, `MCStartDeep`).
- Client mapping: [unified-start-client-mapping.md](unified-start-client-mapping.md).
- Inputs: the 0.8 API review (Start cluster, footgun 5.11, rank 19); the OTP,
  Elixir and Gleam gap report (G1, G2, G7, G8, G14, G15, section 3.1).
- OTP sources: `proc_lib.erl`, `gen.erl` and `gen_server.erl` at OTP-27.1. A
  line reference like `gen.erl:191` points into those files.

## Summary

| # | Decision | OTP |
|---|---|---|
| 1 | `start`, `start_link` and `start_monitor` return after the child's `started()`: `Ok` when it is ready, otherwise the error, after the child's death point | `gen_server:start/start_link/start_monitor` are synchronous on `init/1` |
| 2 | `spawn` starts without waiting (fire and forget); `ActorRef::ready()` waits later | `proc_lib:spawn` |
| 3 | `started()` may fail: it returns `()`, `Result<(), E>` or `Init<Self>` (ready, continue, error, ignore, stop) | `init/1` returns `{ok,S}`, `{ok,S,{continue,C}}`, `{error,R}`, `{stop,R}`, `ignore` |
| 4 | `Init::continue_with(msg)`: a turn that runs before any mailbox message | `handle_continue` |
| 5 | One `StartOptions<A>`: placement, typed name, init timeout, label, stack size, per-instance bounds; reserved slots for 0.10 | `{local,Name}`, `{timeout,T}`, `spawn_opt`, `set_label` |
| 6 | Linked and monitored children inherit the parent's placement (its group or backend). Plain `start`/`spawn` never inherit; `placement_of(&ctx)` opts in | none (one node, one scheduler pool); see [Placement](#4-placement-and-inheritance) |
| 7 | Stage 1 runs eagerly in the caller: admission, link or monitor, name, dispatch. A refused start leaves no trace | `spawn_opt([link, monitor])` is atomic; `gen:init_it` registers the name before `init` |
| 8 | A waiting `start_link` consumes the failed child's `Exit` (trapping parent) and `start_monitor` its `Down` | `proc_lib:sync_start` + `flush_EXIT`; `gen:monitor_return` |
| 9 | The init timeout unlinks or demonitors first, then kills | `kill_flush_EXIT` |
| 10 | Removed: `start_on`, `start_with_backend`, `ExecutionGroup::start`, `start_linked`; `ChildSpec::{startup_timeout, max_outstanding, max_native_jobs}` move into `StartOptions` | |

## 1. The problem

The 0.8 start surface:

| Call | Placement | Link | Waits for `started()` | Fallible |
|---|---|---|---|---|
| `a.start()` | unmanaged `Async` | no | no | no |
| `a.start_with_backend(b)` | unmanaged `b` | no | no | no |
| `a.start_on(&g)` | group `g` | no | no (`ready()` waits) | admission only |
| `g.start(a)` | group `g` (duplicate of `start_on`) | no | no | admission only |
| `a.start_linked(&ctx)` | **always unmanaged `Async`** | yes, before `started()` | no | no |
| `ChildSpec::new(id, &g, f)` | group `g` | supervisor parent | yes (`startup_timeout`) | yes |
| `TestSystem::start`, `Mock::start{,_on}` | test group / unmanaged | no | no | panics |

The resulting problems:

- **Footgun (API review 5.11).** A child that a managed actor starts with
  `start_linked` is outside the parent's group. `spawn_helper` then returns
  `UnmanagedActor`, ordered shutdown never sees the child, and group
  teardown can abort it without `stopped()` (semantics S6).
- **No atomic monitor at start.** `ctx.monitor` after `start()` races the
  child's death, and since 0.8 a child that already died reports `NoProc`
  instead of its real reason.
- **Startup failure is invisible outside supervision.** `started()` cannot
  fail, and `start()` returns an `ActorRef` even when `started()` will
  panic. The client sweep found the ethrex L2 sequencer starting actors one
  by one and only logging startup failures.
- **No continuation.** A self-send from `started()` queues behind anything
  that arrived during `started()`. The dynamic supervisor design (#245)
  moves ethrex's RLPx handshake into "the first message", and a peer message
  or a `terminate_child` can overtake it.
- **Linking at start covers only one placement:** `start_linked` is
  Async-only and `start_on` has no linked form.

## 2. API

### 2.1 tasks (`spawned_concurrency::tasks`)

```rust
pub trait ActorStart: Actor {
    /// proc_lib:spawn. Unmanaged Async, no wait, no parent.
    fn spawn(self) -> ActorRef<Self>;
    /// gen_server:start. Unmanaged Async; resolves after started().
    fn start(self) -> Starting<Self, ActorRef<Self>>;
    /// gen_server:start_link. The parent's placement; linked before started().
    fn start_link<P: Actor>(self, parent: &Context<P>) -> Starting<Self, ActorRef<Self>>;
    /// gen_server:start_monitor. The parent's placement; monitored before started().
    fn start_monitor<P: Actor + Handler<Down>>(self, parent: &Context<P>)
        -> Starting<Self, (ActorRef<Self>, MonitorRef)>;
    /// The same four verbs with options (spawn_opt and the gen_server Opts list).
    fn with_options(self, opts: impl Into<StartOptions<Self>>) -> Starter<Self>;
}

#[must_use = "call spawn, start, start_link or start_monitor"]
pub struct Starter<A: Actor> { /* actor, options */ }
impl<A: Actor> Starter<A> {
    pub fn spawn(self) -> Result<ActorRef<A>, StartError>;
    pub fn start(self) -> Starting<A, ActorRef<A>>;
    pub fn start_link<P: Actor>(self, parent: &Context<P>) -> Starting<A, ActorRef<A>>;
    pub fn start_monitor<P: Actor + Handler<Down>>(self, parent: &Context<P>)
        -> Starting<A, (ActorRef<A>, MonitorRef)>;
}

/// The child is already started (stage 1 ran in the call); awaiting it
/// waits for started(). 'static + Send: it owns what it needs.
#[must_use = "the actor runs; await this to learn whether it started"]
pub struct Starting<A: Actor, T> { /* ... */ }
impl<A: Actor, T> IntoFuture for Starting<A, T> {
    type Output = Result<T, StartError>;
}
impl<A: Actor, T> Starting<A, T> {
    /// The child's reference without waiting: Err only for a stage-1 refusal.
    /// Dropping the Starting abandons the wait (section 5.5).
    pub fn actor_ref(&self) -> Result<ActorRef<A>, &StartError>;
}

impl<A: Actor> ActorRef<A> {
    /// The ack for any placement (0.8: managed only, returning SystemError).
    pub async fn ready(&self) -> Result<(), StartError>;
}
```

### 2.2 threads (`spawned_concurrency::threads`)

The four verbs and `with_options` have the same names and meanings. The
waiting forms block the calling thread instead of returning a future:

```rust
fn spawn(self) -> ActorRef<Self>;
fn start(self) -> Result<ActorRef<Self>, StartError>;
fn start_link<P: Actor>(self, parent: &Context<P>) -> Result<ActorRef<Self>, StartError>;
fn start_monitor<P: Actor + Handler<Down>>(self, parent: &Context<P>)
    -> Result<(ActorRef<Self>, MonitorRef), StartError>;
fn with_options(self, opts: impl Into<StartOptions<Self>>) -> Starter<Self>;
// ActorRef::ready(&self) -> Result<(), StartError>  (blocking)
```

The only difference between the modes is `.await` against blocking, as for
requests. threads mode has one placement (a thread per actor), so its
`StartOptions` has no `group`, `backend` or `placement_of`. These are
tasks-only, like `ActorSystem` today (API review rank 27). A threads-mode
`start_link` from a threads parent inherits "a thread", which is the only
placement there is.

### 2.3 Fallible `started()` and `Init`

```rust
// tasks
fn started(&mut self, ctx: &Context<Self>)
    -> impl Future<Output = impl Into<Init<Self>> + Send> + Send { async {} }
// threads
fn started(&mut self, ctx: &Context<Self>) -> impl Into<Init<Self>> {}

#[must_use]
pub struct Init<A: Actor> { /* private */ }
impl<A: Actor> Init<A> {
    pub fn ready() -> Self;                           // {ok, S}
    pub fn continue_with<M: Message>(msg: M) -> Self  // {ok, S, {continue, C}}
        where A: Handler<M>;
    pub fn error(reason: impl Display) -> Self;       // {error, R}: exit Normal
    pub fn stop(reason: ExitReason) -> Self;          // {stop, R}: exit R
    pub fn ignore() -> Self;                          // ignore: exit Normal
}
impl<A: Actor> From<()> for Init<A>;                            // ready
impl<A: Actor, E: Display> From<Result<(), E>> for Init<A>;      // Err -> error
impl<A: Actor, E: Display> From<Result<Init<A>, E>> for Init<A>; // Err -> error
```

The signature was prototyped with rustc 1.99 (scratch crate). All of these
compile unchanged:

- the existing `async fn started(&mut self, ctx: &Context<Self>) {}`;
- the default body;
- `-> Result<(), io::Error>` and `-> Result<Init<Self>, String>`;
- the threads form.

The `#[started]` macro hook passes the method's return type through. No
existing actor changes. Return-position `impl Trait` in traits is stable
since Rust 1.75, below the workspace's 1.93.

| `started()` outcome | Child | `stopped()` | Waiting starter gets | Link notice / `Down` reason |
|---|---|---|---|---|
| `()`, `Ok(())`, `Init::ready()` | runs | later, as usual | `Ok` | none |
| `Init::continue_with(m)` | runs; `m` is handled first | later | `Ok` (before `m` runs, as OTP's `init_ack`) | none |
| `Err(e)`, `Init::error(e)` | exits `Normal` | no | `InitFailed(e)` | `Normal` |
| `Init::ignore()` | exits `Normal` | no | `Ignored` | `Normal` |
| `Init::stop(r)` | exits `r` | no | `InitExited(r)` | `r` |
| panic | exits `Panic("panic in started(): ..")` | no | `InitExited(Panic(..))` | `Panic` |
| ready, but `ctx.stop*` called in `started()` | exits that reason | yes (L1, unchanged) | `InitExited(r)` | `r` |
| ready, but a held signal ends it (kill, non-`Normal` parent exit, link notice) | exits per X3 | per X3 | `InitExited(r)` | `r` |
| still running at the init timeout | killed | no | `InitTimeout` | none (edge removed first) |

The ack is `Ok` exactly when `started()` returned a ready `Init` and no exit
was pending when it returned. That is also the point where the managed
readiness barrier commits (L2). Typed init data, as Gleam's `returning` and
`proc_lib:init_ack(Ret)`, is not included; see [alternatives](#15-alternatives-considered).

### 2.4 `StartOptions<A>`

```rust
#[derive(Clone)]
#[non_exhaustive]
pub struct StartOptions<A: Actor> { /* private fields */ }
impl<A: Actor> Default for StartOptions<A> { .. }
impl<A: Actor> StartOptions<A> {
    pub fn new() -> Self;
    // placement (tasks only)
    pub fn group(self, group: &ExecutionGroup) -> Self;
    pub fn backend(self, backend: Backend) -> Self;
    pub fn placement_of<P: Actor>(self, ctx: &Context<P>) -> Self;
    // naming and diagnostics
    pub fn name(self, name: impl Into<Name<A>>) -> Self;
    pub fn label(self, label: impl Into<Cow<'static, str>>) -> Self;
    // start behaviour
    pub fn init_timeout(self, timeout: Duration) -> Self;
    // per-instance resources
    pub fn stack_size(self, bytes: usize) -> Self;
    pub fn max_outstanding(self, max: usize) -> Self;
    pub fn max_native_jobs(self, max: usize) -> Self;
}
impl<A: Actor> From<&ExecutionGroup> for StartOptions<A>;   // .group(g)
impl<A: Actor> From<Backend> for StartOptions<A>;           // .backend(b)
```

| Field | Default | Meaning | OTP |
|---|---|---|---|
| placement | `start_link`/`start_monitor`: the parent's; `start`/`spawn`: unmanaged `Async` | [section 4](#4-placement-and-inheritance). The last of `group`/`backend`/`placement_of` wins | none |
| `name` | none | registered for this instance at stage 1, released at its D ([section 6](#6-registered-names)) | `{local, Name}` |
| `label` | none | shown in tracing spans, panic and stall diagnostics, `ShutdownReport` actor details and introspection | `proc_lib:set_label/1` (OTP 27) |
| `init_timeout` | none (wait forever); `ChildSpec` keeps its 5 s default | [section 7](#7-init-timeout) | `{timeout, T}`, default `infinity` (`gen.erl:716`) |
| `stack_size` | runtime default | honoured where the actor owns a thread: threads mode, `Backend::Thread`. Elsewhere `InvalidOptions`; group stacks are rank 8c | none (BEAM stacks grow) |
| `max_outstanding`, `max_native_jobs` | `Actor::MAX_OUTSTANDING`, `MAX_NATIVE_JOBS` | per-instance overrides; were on `ChildSpec` only | none |

**Reserved for 0.10.** These are planned as additive builder methods, with no
new start API:

- `mailbox(Unbounded | Bounded(n))` (rank 17);
- `max_memory` (rank 18a, OTP `max_heap_size`);
- a message-priority opt-in (OTP 28 priority messages, gap G11).

**Not copied from `spawn_opt`:**

- `fullsweep_after`, `min_heap_size` and `message_queue_data`: there is no
  per-actor heap.
- `priority`: a cooperative runtime cannot honour per-actor CPU priority.
  Groups are the bulkhead.
- `hibernate_after` and `debug`.

`Name<A>` is a typed name: `Name::new("peer_table")`, plus
`From<&'static str>` and `From<String>`. `name.whereis() -> Option<ActorRef<A>>`
cannot mistype a lookup (G14). It still keys the global registry until rank 12
scopes it.

### 2.5 `StartError`

```rust
#[derive(Debug, Clone, thiserror::Error)]
#[non_exhaustive]
pub enum StartError {
    Closed,                       // group, system or the parent's scope no longer admits
    Capacity,                     // thread_group full
    AlreadyStarted { name: String, existing: Option<ChildHandle> },
    ParentExited,                 // start_link/start_monitor through a Context whose actor's exit began
    InvalidOptions(&'static str), // e.g. stack_size on a pooled placement, backend in a ChildSpec
    InitTimeout,
    InitFailed(String),           // started() returned an error
    InitExited(ExitReason),       // the child exited before it was ready
    Ignored,                      // started() returned Init::ignore()
    System(SystemError),          // runtime construction and other placement failures
}
```

The `Init*` names follow Gleam's `actor.StartError`. `Ignored` is an error
variant because Rust has no third return shape for OTP's bare `ignore`.
Supervisors treat it as "nothing started" (section 9).
`SystemError::StartupFailed` is removed: `ready()` now returns
`StartError::InitExited`.

## 3. Two stages

**Stage 1 runs in the caller, synchronously, inside the call** (`spawn`,
`start*` or `Starter::*`, before any `.await`). Its steps, in order:

1. **Admit** to the placement: explicit, else inherited, else unmanaged. A
   group admits while it is open and has room. Otherwise the start returns
   `Closed` or `Capacity`.
2. **Link or monitor** (`start_link`/`start_monitor`). The edge is recorded
   in one transaction with the parent's lifecycle state. A parent whose exit
   began, or which is past its death point, refuses with `ParentExited`.
3. **Register** the name, atomically, or refuse with `AlreadyStarted`.
4. **Dispatch** the child to its executor.

**A refusal at any step leaves no trace:**

- the admission slot, the edge and the name are released;
- the state is dropped without either hook running;
- no `Exit` or `Down` is ever delivered, and no actor id is published.

This is L2's rule for a closed admission, extended to every refusal. The
implementation may reorder the internal locks, as long as the result is
indistinguishable from this order. The model checks interleavings of parent
death, group closure and a competing registration between every two steps
(`AbortInvisible`).

**Stage 2 runs on the child.** `started()` runs with signals held (phase 0 of
the exit-signals design), and they are resolved when it returns. The waiting
forms resolve then, as in the table in 2.3.

Because stage 1 is eager, `x.start();` without `.await` still starts the
actor. A forgotten `.await` therefore degrades to `spawn` (plus the
`#[must_use]` warning) and never to "the actor silently never started".
This matters for the migration: a 0.8 statement `x.start();` keeps working.

## 4. Placement and inheritance

**Rule.** The placement is the first that applies:

1. an explicit placement (`group`, `backend`, `placement_of`);
2. for `start_link` and `start_monitor`, the parent's placement;
3. unmanaged `Backend::Async` (threads: a thread).

The parent's placement is fixed when the parent was admitted:

| Parent | Child of `start_link`/`start_monitor` |
|---|---|
| managed, async group `g` | admitted to `g` |
| managed, thread group `g` | admitted to `g` (any free slot; `Capacity` if none) |
| managed, pinned group `g`, shard `i` | `g`, shard `i` (as `Scope::group()` keeps a pinned selection) |
| supervised child on `g` | `g` |
| unmanaged, backend `b` | unmanaged, backend `b` |
| threads mode | a thread |

**No fallback.** If the inherited group is closed (system shutdown began, S1)
or full, the start fails with `Closed` or `Capacity`. Silently escaping the
group is the 0.8 bug this design removes.

**Ownership of an inherited child.**

- **`start_link` child of a managed parent.** It is admitted under the
  parent's scope, as nested supervisors are (admission parent), not as a new
  system root. Its parent for E3 is the linking actor. System shutdown reaches
  it through that parent: the parent's turn in the reverse admission order
  (S2) ends when the parent and its `start_link` descendants have finished,
  or at the grace deadline. The parent's exit reaches them as a parent exit
  (E3). If the parent is supervised, its generation counts as finished (V3)
  only when its `start_link` descendants have also finished. A descendant
  still alive one cleanup interval (the row for the parent in the E3 deadline
  table) after the parent's D is killed; one still alive a second interval
  later makes the slot stuck. Without this, a trapping child would get the
  system's `Shutdown` as a non-parent signal, which it can ignore.
- **`start_monitor` child of a managed parent.** It is a standalone system
  root admitted after its starter, as `start_on` is today; its E3 parent is
  the system. The reverse admission order stops it before its starter.
- **Unmanaged parents.** Inheritance copies the backend only. Unmanaged
  children stay unowned (S6).

**Why only `start_link` and `start_monitor`.** OTP has one node and one
scheduler pool, so every process is "in the parent's placement". The closest
Spawned analogue of the node is the parent's group, and a linked child
outside its parent's group is the footgun. Plain `start` and `spawn` take no
`Context`. Inheriting for them would need an ambient "current actor"
(task-local or thread-local). That would cost something on every turn, would
not follow work into `tokio::spawn` or helper tasks, and would silently move
the 20 `.start()` calls in ethrex's actor code into groups. The explicit form
is `with_options(StartOptions::new().placement_of(ctx))`. OTP's
`gen_server:start` and `start_monitor` also give the child no parent
(`gen.erl:137-142`, parent `self`). Only `start_link` makes the caller the
parent.

## 5. `start_link` and `start_monitor`

### 5.1 Atomicity

- The edge (link or monitor) is recorded before the child's `started()`
  begins, in every mode and placement (model `LinkBeforeStarted`). OTP sets
  it atomically with the spawn: `spawn_opt(.., [link, monitor])`.
- The child never runs without its edge. The edge never exists for a child
  that a stage-1 refusal aborted.
- No edge survives the parent's death point (`NoEdgeToDeadParent`).

### 5.2 Parent state

- **The parent is running** (the normal case: the call comes from its turn).
  The edge is recorded, and the parent's turn waits for the ack (M1: no other
  message of the parent runs meanwhile, as OTP's caller blocks in
  `sync_start`).
- **The parent's exit began, or it is past D.** This is possible only through
  a cloned `Context` used from a helper or job. The start returns
  `ParentExited`, and no child exists. 0.8 instead spawned a doomed child
  that received a `NoProc` parent exit (otp-exit-signals E3, "a parent
  already past D"). That bullet is replaced. OTP has no counterpart (a dead
  process cannot call); the closest rule is X2, where a link from an exiting
  caller is inert.
- **The parent dies after the edge is recorded, during the child's stage 1
  or `started()`.** Its D sends the child a parent exit. The exit is held
  until `started()` returns and then applied by E3: a non-`Normal` reason ends
  the child, and the ack reports `InitExited(reason)`; `Normal` is ignored by
  a non-trapping child. For `start_monitor` the monitor is cancelled at the
  parent's D, and the child continues (it has no parent). Model:
  `ParentExitReachesChild` (liveness), control `DropHeld`.

### 5.3 Startup failure

Every failure is returned after the child's D, as OTP's `sync_start` awaits
the `'DOWN'` (`proc_lib.erl:434-449`, `FailureAfterDeath`). By then the
child's name is free, so the start can be retried at once
(`NameReleasedAtDeath`), as `gen_server` unregisters before notifying
(`gen_server.erl:2203-2210`).

- **`start_link`, trapping parent.** The child's `Exit` is consumed by the
  waiting call and never reaches `exit_received`, as `flush_EXIT`
  (`proc_lib.erl:606`).
- **`start_link`, non-trapping parent.** A non-`Normal` reason is applied as
  a link notice. The call returns the error to the handler, and the parent
  then exits with that reason after its current handler (X3; cooperative,
  L3). In OTP the link kills the caller before `start_link` returns (the
  `proc_lib:start_link` docs). A `Normal` exit (`Init::error`, `ignore`) is
  ignored.
- **`start_monitor`.** The `Down` is consumed, as `gen:monitor_return`
  (`gen.erl:172-182`); the call returns the error. Model: `FailureFlushed`.

### 5.4 Ok

`Ok` means `started()` returned a ready `Init` (`AckMeansStarted`). After
`Ok`, the edge behaves as any `ctx.link`/`ctx.monitor` edge (X1, X3, X5).

### 5.5 Abandoning the wait

Dropping the `Starting` (tasks) before it resolves abandons only the wait. The
child keeps running with its edge and its name. A notice already diverted to
the waiting call is handed to the parent as if it had never been diverted. The
handover is linearised with the child's D, so the notice is delivered exactly
once (`AbandonedNoticeDelivered`, control `AbandonDropsNotice`). An abandoned
`start` is `spawn`. `Starting` is `'static + Send`, so a handler may also
hand it to a job and await it outside its turn. The consumption then happens
wherever it is awaited.

**Mechanism (no cost on send or dispatch).** While a waiting call exists, the
edge carries an "ack pending" mark. The child's death notice for a marked edge
goes to the call's slot instead of the parent's mailbox. The ack (`Ok` or
abandon) clears the mark under the edge's lock. Unmarked edges, which are
every edge after startup, take the 0.8 path unchanged.

## 6. Registered names

- **When.** At stage 1, before `started()`, so the child can be found while
  it initialises (messages queue; a continuation still runs first). OTP
  registers in `gen:init_it` before `init/1` (`gen.erl:194-201`).
- **Clash.** The start is refused with
  `AlreadyStarted { name, existing }`. `started()` does not run, and nothing
  is linked or delivered. `existing` is the holder's `ChildHandle` when an
  actor holds the name, or `None` when a value from the untyped `register`
  does. OTP returns `{error, {already_started, Pid}}` and its loser exits
  `normal` (`gen.erl:198-200`). A `normal` exit reaches no non-trapping
  caller, and the trapping caller's `'EXIT'` is consumed, so nothing is
  observable either.
- **Uniqueness.** Two concurrent starts with one name: exactly one is
  admitted (`NameUnique`, control `DoubleRegister`).
- **Release.** At the holder's D, before any link notice, `Down`,
  `wait_exit`, ack or `ShutdownReport` entry can observe the death
  (`NameReleasedAtDeath`, control `LateUnregister`). This applies to every
  placement, unmanaged ones included. 0.8's `ctx.register_name` is
  managed-only and released at cleanup. A refused start never held the name.
- **Supervised children.** Each generation registers the spec's name, and
  the replacement registers only after the previous generation's D (V3).
  `name.whereis()` therefore keeps addressing the current generation. This
  is Gleam's `named_subject` behaviour, without a proxy.
- **Exception.** An init timeout whose child is stuck inside `started()`
  keeps the name until the child's D (section 7).
- `ctx.register_name` remains for names chosen after start. The registry
  keeps its global scope until rank 12.

## 7. Init timeout

`init_timeout(d)` bounds stage 2. It is measured from dispatch until
`started()` returns, as OTP's timer starts after the spawn.

At expiry, if a starter is still waiting:

1. the link or monitor edge is removed (`unlink`/`demonitor` fences);
2. a kill is sent (`kill_flush_EXIT` unlinks before `exit(Pid, kill)`,
   `proc_lib.erl:610-615`);
3. the call returns `InitTimeout` after the child's D, or after a bounded
   wait for it: the placement's cleanup deadline (5 s unmanaged, otherwise
   the group's or spec's).

Model: `TimeoutContained`, control `KillBeforeUnlink`.

Rust cannot preempt `started()` (L3), so a child stuck in blocking code
reaches D only when `started()` returns. Until then it holds its name and its
admission slot, and it is reported as unfinished if the system shuts down.

**Without a waiting starter** (`spawn`, or an abandoned `start*`), the
timeout is enforced by a framework timer armed at dispatch, only when the
option is set. Expiry kills the child, and `ready()` reports `InitTimeout`.
An edge of an abandoned `start_link` stays, so the `Killed` notice reaches
the parent like any linked death (trappable, X4).

**Default.** None, as OTP's `infinity`. `ChildSpec` keeps its 5 s startup
default (V7, a Spawned bound). The gap report suggested 5 s "as
`gen_server:start`", but OTP's default is `infinity` (`gen.erl:716-722`). A
5 s default would also kill client actors whose `started()` does slow I/O
today (open question 3).

## 8. Continuation (`handle_continue`)

`Init::continue_with(msg)` runs `Handler<M>::handle(msg)` as the actor's
first turn after `started()`, before any mailbox message, including messages
that arrived during `started()` (`ContinueFirst`, control `ContinueQueued`).

- The ack and managed readiness commit before the continuation runs, as
  OTP's `init_ack` precedes `handle_continue` (`gen_server.erl:2197-2201`).
- A stop or exit observed before the continuation begins discards it, as D4
  discards a queued message. No caller waits on it; its `PendingReply` is
  detached.
- A panic in the continuation is an ordinary handler panic after readiness.
  Under a supervisor it is a crash of a ready child, not a startup failure.
- **Cost.** One branch after `started()`; the dispatch loop is unchanged.
- **Not included:** continuing from ordinary handlers (OTP's
  `{noreply, S, {continue, C}}`). That needs a check after every turn, which
  is a cost on the dispatch path (open question 4).

## 9. Supervisors

The OTP child spec separates `start` (the MFA and its options) from
`restart`, `shutdown`, `type` and `significant`. `ChildSpec` mirrors that:

```rust
ChildSpec::new(id, &group, |fctx| async { Ok(MyActor::new(..)) })
    .options(StartOptions::new()
        .name("rpc")
        .init_timeout(Duration::from_secs(10))
        .label("http")
        .max_native_jobs(8))
    .restart(RestartMode::Transient)
    .shutdown_timeout(Duration::from_secs(5))
```

- **Placement.** The positional group stays: a supervised child is always
  managed, and the group must belong to the supervisor's system
  (`ForeignGroup`). A `group` in `options` replaces it. A `backend` is
  `InvalidOptions` at `add()`.
- **Moved into `StartOptions`.** `startup_timeout` becomes `init_timeout`
  (default 5 s when unset, V7 unchanged). `max_outstanding` and
  `max_native_jobs` also move. `restart` and `shutdown_timeout` stay; there
  is room for a future `significant(bool)` (G15), since the builder has
  private fields.
- **Each generation is a `start_link` with the supervisor as parent.** The
  same stage-1 and ack rules apply. `Init::error`, `stop` or a panic is a
  failed start: the first start aborts the tree (V7), and a replacement is
  retried (V1). **`Init::ignore`** follows OTP: the spec stays and no
  generation runs, in a new `ChildState::Ignored`. A Temporary child that
  ignores is removed from the snapshot, as OTP deletes it. `ignore` does not
  fail the initial start and is not a restart.
- **Name clashes.** A clash at a restart is a failed restart attempt, which
  is retried with backoff and charged to the budget (V5). OTP also fails the
  restart on `already_started`.
- **Continuations.** A continuation of a supervised child runs after
  readiness.
- **Dynamic supervisor (#245).** `DynamicChildSpec` takes the same
  `StartOptions<A>`. `start_child` returns `Ignored` as "nothing started"
  (gap report 3.2.3). The RLPx handshake becomes a continuation, so no peer
  message or `terminate_child` can overtake it (3.2.1).
- **Not derived.** `#[actor]` does not derive a child spec; see
  [alternatives](#15-alternatives-considered).

## 10. Test kit (`spawned-test`)

| 0.8 | 0.9 |
|---|---|
| `Mock::start()` | `Mock::spawn()` (immediate; a mock's `started()` is trivial) |
| `Mock::start_on(&g)` | `Mock::spawn_with(opts)` (`impl Into<StartOptions<Mock>>`, so `&g` works) |
| none | `Mock::on_started(\|ctx\| Init)` and `Mock::start_delay(d)` (virtual time under the simulator): drive every `Init` outcome and the init timeout from tests |
| `TestSystem::start(a)` | `TestSystem::spawn(a)` (immediate, panics on refusal) and `TestSystem::start(a).await` (waits, panics with the `StartError`) |
| `TestSystem::start_mock(m)` | `TestSystem::spawn_mock(m)` |
| none | `TestSystem::options::<A>()`: `StartOptions` with the test group set |

`start_link` and `start_monitor` are exercised from tests through `in_actor`.
Its closure gets the parent's `Context`, and because `Starting` is `'static`
it can be returned and awaited outside the turn.

## 11. Erlang/OTP alignment

Categories as in semantics section 11: R (Rust forces it), T (Rust types add
strictness), B (a bound Erlang lacks).

| OTP | Spawned 0.9 | Verdict |
|---|---|---|
| `gen_server:start/3,4`: synchronous on `init/1` | `start()`, `with_options(o).start()` | aligned |
| `gen_server:start_link`: link set atomically, caller is the parent | `start_link(&ctx)` | aligned; the parent is passed explicitly (R: no `self()`) |
| `gen_server:start_monitor` (OTP 23) → `{ok, {Pid, Mon}}` | `start_monitor(&ctx)` → `(ActorRef, MonitorRef)` | aligned |
| `proc_lib:spawn/spawn_opt`: no init ack | `spawn()`, `with_options(o).spawn()` | aligned |
| `{local, Name}`; `{error, {already_started, Pid}}` | `name(Name<A>)`; `AlreadyStarted { name, existing: Option<ChildHandle> }` | aligned; typed name (T); `existing` is `None` when a non-actor value holds the name (T: the registry stores typed values) |
| name unregistered before the starter is notified | released at D, before any notice | aligned |
| `{timeout, T}`, default `infinity` | `init_timeout`, default none | aligned; the wait for the killed child's death is bounded (R: no preemption) |
| `kill_flush_EXIT` on timeout | unlink/demonitor, then kill | aligned |
| init `{ok, S}` / `{ok, S, {continue, C}}` / `{error, R}` / `{stop, R}` / `ignore` | `Init::ready` / `continue_with` / `error` / `stop` / `ignore` | aligned; `ignore` is `Err(Ignored)` (T) |
| init crash → `{error, R}`, exit `R` | panic → `InitExited(Panic)` | aligned |
| `handle_continue` from `init` | `Init::continue_with` | aligned |
| `{noreply, S, {continue, C}}` from callbacks | not in 0.9 | gap: dispatch-path cost (open question 4) |
| failed `start_link`, trapping caller: `'EXIT'` consumed | consumed | aligned |
| failed `start_link`, non-trapping caller: killed by the link before returning | returns the error, then exits after the current handler | R: cooperative signals (L3) |
| failed `start_monitor`: `'DOWN'` consumed (`gen:monitor_return`) | consumed | aligned |
| start from a dead process | `ParentExited` | R: a cloned `Context` outlives its actor |
| `spawn_opt` `link`/`monitor` | the verbs | aligned |
| `spawn_opt` `max_heap_size` | reserved (rank 18a) | planned |
| `spawn_opt` `priority`, `fullsweep_after`, `min_heap_size`, `message_queue_data`; `hibernate_after` | none | not applicable (no per-process heap or priority scheduler) |
| `proc_lib:set_label` (OTP 27) | `label` at start | aligned (set once, at start) |
| one node, one scheduler pool: no placement | groups and backends; linked/monitored children inherit | R: no VM; groups are the scheduler analogue |
| `process_flag(trap_exit, true)` in `init` | `ctx.trap_exit(true)` in `started()` | aligned; no start option is needed (signals are held during `started()`, X3) |

## 12. Migration from 0.8

| 0.8 | 0.9 | Behaviour |
|---|---|---|
| `a.start()` | `a.spawn()` | same |
| | or `a.start().await?` | waits for `started()`; reports init errors |
| `a.start_on(&g)?` | `a.with_options(&g).spawn()?` | same |
| | or `a.with_options(&g).start().await?` | waits |
| `a.start_with_backend(b)` | `a.with_options(b).spawn()?` | same (now a `Result`; it fails only on options) |
| `g.start(a)?` | `a.with_options(&g).spawn()?` | same |
| `a.start_linked(&ctx)` | `a.start_link(&ctx).await?` | **changes:** inherits the parent's placement, waits for `started()`, refuses an exiting parent |
| `ctx.monitor(&a.start().child_handle())` | `a.start_monitor(&ctx).await?` | atomic; no `NoProc` race |
| `ctx.register_name(n)` in `started()` | `with_options(StartOptions::new().name(n))` | atomic with start; released at D; all placements |
| `actor.ready().await` → `Result<(), SystemError>` (managed only) | `Result<(), StartError>`, every placement | error type |
| `SystemError::StartupFailed(r)` | `StartError::InitExited(r)` | |
| `ChildSpec::startup_timeout(d)` | `.options(StartOptions::new().init_timeout(d))` | same |
| `ChildSpec::{max_outstanding, max_native_jobs}` | `StartOptions::{max_outstanding, max_native_jobs}` | same; now also for unsupervised actors |
| `async fn started(..) {}` | unchanged | same; may now return `Result<(), E>` or `Init<Self>` |
| self-send from `started()` to finish setup | `Init::continue_with(msg)` | runs before any other message |
| `Mock::start()` / `Mock::start_on(&g)` | `Mock::spawn()` / `Mock::spawn_with(&g)` | same |
| `TestSystem::start(a)` / `start_mock(m)` | `TestSystem::spawn(a)` / `spawn_mock(m)` | same |

In tasks mode a 0.8 statement `x.start();` still compiles and starts the
actor (with a `#[must_use]` warning, section 3). `let r = x.start();` used as
an `ActorRef` does not compile, which flags every site to migrate. The
removed functions are compile errors (trybuild cases in the test plan).

## 13. Client mapping

Every start call on the clients' `spawned-v0.8` branches is mapped in
[unified-start-client-mapping.md](unified-start-client-mapping.md): ethrex
`c1163b7`, ethlambda `85990aa`.

- **Sites that change:** 51 (ethrex 37, ethlambda 14). All are mechanical,
  and the behaviour-preserving rewrite keeps the current behaviour at every
  one.
- **Inheritance changes behaviour at no site:** neither client calls
  `start_linked`, and plain starts never inherit.
- **Two handler sites** start per-connection actors from inside an actor:
  ethrex's RLPx initiator (via `PeerConnection::spawn_as_initiator`) and the
  L2 proof coordinator. They stay unmanaged. Moving them to `start_link` or
  `placement_of` is a client decision with a behaviour change: group
  ownership, ordered shutdown and capacity.
- **Optional upgrade at 14 async sites** (8 in the L2 sequencer and the
  based block fetcher, 4 in the prover, discovery, the monitor):
  `start().await?` surfaces init failures, which today are only logged.
- **Six client functions return `SystemError` from `start_on`.** Their error
  type becomes `StartError`, or they map it.
- **Supervisor specs** (3 `ChildSpec::new` sites): unchanged. They set no
  `startup_timeout` or bounds.

## 14. Semantics contract changes

New or changed guarantees, with ids for `semantics.md`. They are written into
the contract by the implementation PR, as with otp-exit-signals.

- **L1 (changed).** `stopped()` runs only after `started()` returned a ready
  `Init`. A failed init (error, ignore, stop, panic) skips it.
- **L2 (changed).** Readiness exists for every placement. `ready()` and the
  waiting starts resolve at the same barrier: `started()` returned ready with
  no exit pending (and, for managed actors, admission still open).
- **L8 (new), Start.** Two stages as in section 3. `spawn` returns after
  stage 1. `start`, `start_link` and `start_monitor` return after `started()`:
  `Ok` only if ready, otherwise the error after the child's D. A stage-1
  refusal leaves no trace.
- **L9 (new), Init outcomes.** The table in 2.3.
- **L10 (new), Continuation first.** No mailbox message is dispatched before
  the continuation. A stop before it discards it.
- **X5 (changed).** "`start_linked` registers the link before the child's
  `started()`" becomes "`start_link` and `start_monitor` register their edge
  before the child's `started()`, on every placement. A start through an
  exiting or dead parent is refused (`ParentExited`)."
- **X3 (changed).** The parent row reads "`start_link` caller" for
  "`start_linked` caller".
- **X8 (new), Start-time edges and failure.** On a failure while the starter
  waits: a trapping parent's `Exit` and the `Down` are consumed, and a
  non-trapping parent applies a non-`Normal` notice after its current handler.
  An abandoned wait delivers the notice exactly once. The init timeout
  removes the edge before the kill.
- **S6 (changed).** `spawn` and `start` are unmanaged unless given a
  placement. `start_link` and `start_monitor` take the parent's.
- **S7 (new), Inheritance.** Section 4: the rule, no fallback, `Closed` and
  `Capacity`.
- **S8 (new), Linked descendants are owned by the parent's scope.**
  Shutdown order, V3 extension and kill escalation, as in section 4.
- **S9 (new), Start-time names.** Atomic, unique and released at D before any
  observation, on every placement; `AlreadyStarted` on a clash.
- **T5 (new), Init timeout.** Section 7.
- **V1/V7 (changed).** `Init::ignore` leaves the spec without a generation
  (`Ignored`) and is not a failure. A name clash at a restart is a failed
  attempt.
- **Section 11 (changed).** New rows:
  - start synchronous on init: aligned in 0.9;
  - fallible init and continue: aligned;
  - failed `start_link` with a non-trapping caller: R;
  - continue from handlers: gap.

**Non-guarantees** (added to section 10):

- The init timeout cannot interrupt `started()`. The killed child holds its
  name and slot until `started()` returns.
- Inheritance does not follow work outside the start call: `tokio::spawn`,
  helpers and plain `start`/`spawn` from a handler stay unmanaged.
- The continuation exists only after `started()`.
- An abandoned start does not consume the child's failure notice.
- Names are process-global until rank 12.

## 15. The model

[`Start.tla`](../../verification/tla/Start.tla) models:

- one parent (managed in group `G` or unmanaged, trapping or not, which may
  exit `Normal` or `Panic`);
- children started with `spawn`, `start`, `start_link` or `start_monitor`,
  with an explicit placement (none, `G`, unmanaged, `placement_of`) and
  possibly one shared name;
- `G`, which may close and admits at most `Cap` live actors.

Stage 1 is four separate steps, so parent death, group closure and a
competing registration interleave between them. `started()` is one step with
every `Init` outcome. Signals are held until it returns. Others may send
messages, which a continuation must precede. A waiting starter can time out
or abandon the wait.

**Invariants:**

| Invariant | Meaning |
|---|---|
| `InheritedPlacement` | explicit placement wins; link/monitor take the parent's; otherwise unmanaged |
| `LinkBeforeStarted` | the link or monitor exists before `started()` begins |
| `NameUnique` | at most one live holder of a name |
| `NameReleasedAtDeath` | no holder past D or aborted |
| `AbortInvisible` | a refused start: no edge, name, slot, hook or notice |
| `AckMeansStarted` | `Ok` from a waiting start means `started()` returned |
| `FailureAfterDeath` | the init error is returned after the child's D |
| `FailureFlushed` | a failed or timed-out `start_monitor` delivers no `Down`; `start_link` no `Exit` to a trapping parent |
| `TimeoutContained` | an init-timeout kill never reaches the parent |
| `NoEdgeToDeadParent` | no edge survives the parent's D |
| `AdmittedOnlyOpen`, `CapacityRespected` | group admission |
| `NoticeOnce` | at most one `Exit` and one `Down` per child |
| `ContinueFirst` | no message before the continuation |
| `AbandonedNoticeDelivered` | an abandoned wait hands a diverted notice to the parent |

Liveness (`MCStartLive`, weak fairness on the child's steps and the parent's
D): `ParentExitReachesChild`. A child linked to a parent that dies with a
non-`Normal` reason, also during its `started()`, dies (or was refused).

**Results** (TLC 2.19, fede, 4 workers, `nice -n 19` on a loaded host):

| Configuration | Run | Distinct states | Depth | Time |
|---|---|---|---|---|
| `MCStart` | per PR, safety | 2,896,748 | 24 | 23 s |
| `MCStartContinue` | per PR, safety | 17,720 | 16 | 1 s |
| `MCStartLive` | per PR, safety + liveness | 424,172 | 23 | 20 s |
| `MCStartDeep` | weekly (every explicit placement, `spawn`, messages) | see below | | |

MCStartDeep is reported in the PR description once its run completes.

**Negative controls.** `negative_checks.py` runs `named_fault` with
`TypeOK` and only the named invariant, and `named_temporal` for the
liveness control. Each control below produced its counterexample (1-7 s
each, one at a time):

| Fault | Reintroduces | Fails |
|---|---|---|
| `NoInherit` | 0.8: a linked/monitored child of a managed parent is unmanaged | `InheritedPlacement` |
| `LinkAfterStarted` | the link is set once `started()` runs | `LinkBeforeStarted` |
| `DoubleRegister` | check-then-insert registration | `NameUnique` |
| `LateUnregister` | the name outlives D | `NameReleasedAtDeath` |
| `AbortLeaksLink` | a refused start keeps its edge | `AbortInvisible` |
| `EarlyFailAck` | the error returns before the child's D | `FailureAfterDeath` |
| `AckBeforeStarted` | `Ok` before `started()` returned | `AckMeansStarted` |
| `NoMonitorFlush` | a failed `start_monitor` also delivers `Down` | `FailureFlushed` |
| `NoLinkFlush` | a failed `start_link` also delivers `Exit` | `FailureFlushed` |
| `KillBeforeUnlink` | the timeout kills while linked | `TimeoutContained` |
| `LinkCheckLate` | the link ignores the parent's state | `NoEdgeToDeadParent` |
| `AdmitClosed` | a closed group admits | `AdmittedOnlyOpen` |
| `IgnoreCap` | a full group admits | `CapacityRespected` |
| `AbandonDropsNotice` | an abandoned wait loses the diverted notice | `AbandonedNoticeDelivered` |
| `ContinueQueued` (`MCStartContinue`) | the continuation is queued behind earlier messages | `ContinueFirst` |
| `DropHeld` (`MCStartLive`, liveness) | a parent exit held during `started()` is dropped | `ParentExitReachesChild` |

**Not modelled:**

- the child trapping exits (E3's trapping-child rules are in `Lifecycle.tla`);
- more than one parent;
- the system's ordered shutdown of linked descendants (S8). That extends
  `Coordinator.tla` and `NestedSupervision.tla` in the implementation PR;
- time;
- the init-timeout timer without a waiting starter.

## 16. Test plan

The implementation is held to this plan. Every guarantee in section 14 has a
check at each layer that applies, and each check must fail when the
guarantee breaks.

### 16.1 Per-guarantee matrix

Columns: TLA+ invariant (and trace validation); integration tests (both
modes); simulator (`#[sim_test]`, PCT and bounded DFS); fuzzer oracle;
trybuild.

| Guarantee | TLA+ / trace | Integration (`concurrency/tests/start_contract.rs`) | Sim (`start_sim.rs`) | Fuzz oracle | trybuild |
|---|---|---|---|---|---|
| L8 two stages, ack | `AckMeansStarted`, `FailureAfterDeath` | `{spawn,start,start_link,start_monitor}_returns_{immediately,after_started}`; `start_without_await_still_starts` | `ack_races_child_death` | ack `Ok` ⇒ hook log shows `started()` returned | `start_result_is_not_an_actor_ref` |
| L8 refusal invisible | `AbortInvisible` | `refused_start_runs_no_hook_and_leaves_no_edge_or_name` (closed, capacity, clash, parent exited) | `refusal_races_parent_death` | aborted ids never appear in `Exit`/`Down`/registry | |
| L9 Init outcomes | (model abstracts the outcomes to their exits) | one test per row of the 2.3 table, each mode and placement, for `start`, `start_link`, `start_monitor`, `spawn`+`ready()` | | `started()` outcomes generated; oracle checks the reported `StartError` against the outcome | `started_may_return_unit_result_or_init` (ui-pass); `continue_with_requires_handler` |
| L1 `stopped()` skipped on failed init | | `failed_init_skips_stopped` (every outcome) | | existing L5 oracle extended | |
| L2 readiness for every placement | `AckMeansStarted` | `ready_resolves_for_unmanaged_and_threads` | | | |
| L10 continuation first | `ContinueFirst` | `continuation_runs_before_messages_sent_during_started`; `stop_before_continuation_discards_it` | `continuation_races_peer_send` | hook-log order | |
| X5 edge before `started()` | `LinkBeforeStarted` | `start_link_child_panicking_in_started_notifies_parent` on every placement; same for `start_monitor` | `child_dies_in_started_racing_link` | link/monitor atomicity: a child failing in `started()` always produces the parent's notice or the consumed error | |
| X5 parent exited | `NoEdgeToDeadParent` | `start_link_through_dead_context_is_refused` | `parent_stops_during_child_started` | | |
| X8 failure flush | `FailureFlushed` | `failed_start_link_consumes_exit_of_trapping_parent`; `failed_start_link_kills_nontrapping_parent_after_handler`; `failed_start_monitor_consumes_down` | `failure_races_ack` | no `Exit`/`Down` for a start that returned an init error | |
| X8 abandoned wait | `AbandonedNoticeDelivered` | `abandoned_start_monitor_still_delivers_down`; `abandoned_start_link_delivers_exit` | `abandon_races_child_death` | | |
| T5 init timeout | `TimeoutContained` | `init_timeout_returns_after_death_and_spares_parent` (each kind); `spawn_init_timeout_kills_and_ready_reports_it`; `stuck_started_holds_name_until_return` | `timeout_races_started_return` | timeout options generated; parent never ends `Killed` from a timed-out child | |
| S7 inheritance | `InheritedPlacement` | `link_and_monitor_inherit_{async_group,thread_group,pinned_shard,backend_async,backend_blocking,backend_thread}`; `plain_start_never_inherits`; `placement_of_opts_in` | | placement oracle: the child's `execution_group()` name (or backend) matches the rule | `threads_options_have_no_group_or_backend` |
| S7 closed or full group | `AdmittedOnlyOpen`, `CapacityRespected` | `inherited_start_into_closing_group_fails_closed`; `inherited_start_into_full_thread_group_fails_capacity` | `group_shutdown_during_start` | existing S1 oracle | |
| S8 linked descendants owned | (Coordinator/Nested extension in the impl PR) | `system_shutdown_stops_linked_children_with_their_parent`; `supervised_parent_generation_waits_for_linked_children`; `unlinked_descendant_is_killed_after_one_interval` | `parent_shutdown_racing_child_start` | existing S1-S4 and V3 oracles see linked children | |
| S9 names | `NameUnique`, `NameReleasedAtDeath` | `name_clash_returns_already_started_with_holder`; `failed_named_start_can_be_retried_at_once`; `name_released_before_down_and_exit`; `typed_name_whereis` | `name_registration_race` | at most one live holder; no name of a dead actor after its `Down` | `name_type_must_match_actor` |
| V1/V7 ignore, clash at restart | (Supervision model: `Ignored` state in the impl PR) | `supervised_ignore_keeps_spec_without_generation`; `temporary_ignore_is_removed`; `restart_name_clash_is_a_failed_attempt` | | V1 oracle treats `Ignored` | |
| API removal | | | | | `start_on_removed`, `start_with_backend_removed`, `start_linked_removed`, `execution_group_start_removed`, `start_options_struct_literal` (private/`non_exhaustive`), `start_monitor_requires_down_handler` |

**Placements.** Every integration test runs in:

- tasks: `Backend::Async`, `Blocking`, `Thread`, `async_group`,
  `thread_group` (and `pinned_group` on Linux);
- threads mode, where the API exists there (all except the placement rows).

They use a per-placement macro, as in `semantics_gaps`. "Start from outside
any actor" is the plain `spawn`/`start` rows run from the test body.

### 16.2 TLA+ and trace validation

- **Model.** `Start.tla`, per PR: `MCStart`, `MCStartContinue`,
  `MCStartLive` (the CI leg `start`). Weekly: `MCStartDeep`. The 16 negative
  controls are in `negative_checks.py` (`START_CONTROLS`,
  `START_TEMPORAL_CONTROLS`).
- **Trace schema `start-v1`.** The `lifecycle-trace` feature records these
  events from the real code:
  - `SBegin(c, kind, explicit, named)`, `SAdmit(c, place | refusal)`,
    `SLink(c)`, `SMonitor(c)`, `SRegister(c | already)`, `SDispatch(c)`;
  - `SStartedBegin(c)`, `SStartedEnd(c, outcome)`, `SAck(c, result)`;
  - `STimeout(c)`, `SAbandon(c)`, `SDie(c, reason)`;
  - `PDie(reason)`, `GClose`, `SSend(c)`, `SHandle(c)`, `SContinue(c)`.

  `TraceStart.tla` replays them as a behaviour of `Start.tla` (`trace_check.py`
  gains the schema). A new `concurrency/tests/start_trace.rs` records every
  integration scenario of 16.1 on Async, async groups and thread groups.
- **Trace negative controls** (`negative_checks.py`, `require_rejected`):
  - move `SLink` after `SStartedBegin`;
  - give two children the same `SRegister`;
  - change an `SAdmit` place to unmanaged under a managed parent;
  - deliver an `Exit` after a failed `SAck`.
- **Trace coverage.** `trace_coverage.py` adds a `StartCoverage` projection;
  the baseline requires every `Start.tla` action to be reached by a recorded
  trace.

### 16.3 Simulator

`concurrency/tests/start_sim.rs`, each case under PCT (depths 1-3) and
bounded DFS, with seeds and tapes:

| Race | Check |
|---|---|
| parent stops during the child's `started()` | the child ends with the parent's reason, or `Normal` ignored |
| link or monitor racing the child's death in `started()` | exactly one of: consumed error, or delivered notice (abandoned) |
| two starts with one name | exactly one `Ok`, one `AlreadyStarted` |
| group shutdown during a start | either admitted and owned by shutdown, or `Closed`; never unowned |
| init timeout racing `started()` return | `Ok` xor `InitTimeout`; the parent is never hit |
| abandon racing the child's death | the notice is delivered exactly once |
| continuation racing a peer's send during `started()` | the continuation always runs first |

### 16.4 Scenario fuzzer (`fuzz/`)

- **Generator.** `SpawnHow` becomes
  `Start { verb: Spawn | Start | Link | Monitor, placement: Default | Group(g) | Unmanaged | SameAsParent, name: Option<u8>, init_timeout: Option<u8>, abandon: bool }`.
  Worker hooks gain `started()` outcomes: ready, continue, error, ignore,
  stop with a reason, panic, sleep.
- **New oracles:**
  - placement: the inheritance rule;
  - link/monitor atomicity and failure flush;
  - names unique and released before any death observation;
  - ack `Ok` ⇒ `started()` returned;
  - continuation first.
- **Extended oracles:** the existing M1/L1/L5 and X1/X3/X5 oracles learn
  that a consumed notice is expected.

### 16.5 Planted bugs

Added to `verification/planted/patches/` (the #247 matrix). Each must be
caught by at least two layers.

| Patch | Bug | Must be caught by |
|---|---|---|
| `NN-start-link-ignores-parent-group` | `start_link` admits the child unmanaged when the parent is managed (the 0.8 behaviour) | `link_and_monitor_inherit_*`, fuzz placement oracle, trace `SAdmit` check (`InheritedPlacement`) |
| `NN-start-link-after-dispatch` | the edge is recorded after the child is dispatched | `start_sim::child_dies_in_started_racing_link` (deterministic; the integration test `start_link_child_panicking_in_started_notifies_parent` catches it only on some schedules), trace `LinkBeforeStarted` |
| `NN-start-name-check-then-insert` | the name is checked under a read lock and inserted under a separate write lock | `start_sim::name_registration_race`, fuzz name oracle |
| `NN-start-monitor-down-lost-on-abandon` | a diverted `Down` is dropped when the `Starting` is dropped | `abandoned_start_monitor_still_delivers_down`, `start_sim::abandon_races_child_death` |
| `NN-start-name-released-at-publication` | the name is released at publication instead of D | `failed_named_start_can_be_retried_at_once`, `name_released_before_down_and_exit`, trace `NameReleasedAtDeath` |

### 16.6 Coverage matrix

Each new or changed guarantee in section 14 gets:

- a "Checked by" line in `semantics.md` naming the checks above;
- a row in `docs/TESTING.md` "Semantics coverage" (modes, placements,
  covered or partial).

New rows: L8, L9, L10, X8, S7, S8, S9, T5. Changed rows: L1, L2, X3, X5, S6,
V1, V7. A guarantee is merged as covered only when each clause has a failing
check under its planted bug or model control.

### 16.7 Client checks

The client-compat gate (`.github/workflows/client-compat.yml`) runs on the
implementation PR, against client branches that carry the mapped rewrite
(`spawned-v0.9`, created from `spawned-v0.8`):

- **ethrex:** check, clippy `-D warnings`, `ethrex-p2p` tests, the `p2p::`
  and `rpc::` groups, and the lib tests.
- **ethlambda:** clippy and `cargo test --workspace --profile release-fast`.

Clippy `-D warnings` also turns any leftover un-awaited `start()` into an
error.

## 17. Alternatives considered

| Alternative | Why not |
|---|---|
| Keep `start()` immediate and add `ready()` (no waiting starts) | Startup failure stays invisible by default (G1). The init timeout and failure flush have no starter to act for, so a timed-out linked child would kill a non-trapping parent with `Killed`, unlike OTP's `kill_flush_EXIT` |
| Waiting starts with a lazy stage 1 (nothing happens until awaited) | A forgotten `.await` would silently never start the actor (a warning only), across 51 migrated client sites. The eager stage 1 makes that mistake equal `spawn` |
| Ambient inheritance for plain `start`/`spawn` (task- or thread-local current actor) | A cost per turn; it does not follow `tokio::spawn` or helpers; it silently moves 20 ethrex call sites into groups; OTP's plain `start` has no parent either |
| Fall back to unmanaged when the inherited group is closed or full | It re-creates the footgun at the worst moment (shutdown) |
| One `start(opts)` taking options always (`start(StartOptions::default())`) | Noise at the common call; the `with_options` builder keeps the zero-option verbs short and gives one options path |
| Six or eight `_with` methods (`start_with`, `start_link_with`, ..) | Doubles the surface; `with_options(o).verb()` is one entry point |
| Typed init data (`started() -> Init<R>`, Gleam's `returning`) | The starter needs the type `R` statically, which needs an associated type on `Actor`, and stable Rust has no associated-type defaults, so every manual `impl Actor` would break. The main use (handing back a sub-handle) is covered by the typed `ActorRef<A>` and `Recipient`s derived from it. `Starting` is a struct, so a later `data()` accessor stays additive |
| Typed init error (`InitFailed(E)`) | Same associated-type problem; `Display` text matches supervisors' factory errors and Gleam's `InitFailed(String)` |
| `trap_exit` as a start option | No window to close: signals are held during `started()` and resolved with the flag read when it returns (X3). One way to set it stays |
| `#[actor]` derives a child spec (Elixir `child_spec/1`, Gleam `supervised()`) | A spec needs a factory for fresh state per generation, which the type alone cannot provide. With `ChildSpec::new(id, &g, f).options(o)` the derive would save nothing. `ChildSpec` keeps room for `significant` |
| `spawn_link` / `spawn_monitor` (no waiting) | OTP's `gen_server` has none; slow initialisation belongs in a continuation, so a waiting `start_link` returns fast |
| Continuation from every handler | A check after every turn is a cost on the dispatch path; post-init is the case clients need (#245) |
| `init_timeout` default 5 s | OTP's default is `infinity`; it would kill client actors whose `started()` does slow I/O; `ChildSpec` keeps its 5 s bound |
| Keep `start_on` as sugar for `with_options(g).spawn()` | Two spellings again (`ExecutionGroup::start` was the same mistake) |

## 18. Gap-report input: accepted and rejected

| Suggestion | Decision |
|---|---|
| Fallible init on every start path, `InitTimeout`/`InitFailed`/`InitExited`, non-waiting `spawn` | accepted (sections 2.3, 2.5, 3) |
| `Init` carrying data for the starter | rejected for 0.9 (section 17), room kept |
| `Init` continuation, `Init::ignore` | accepted (sections 8, 9) |
| `start_monitor`; link and monitor on every backend and group | accepted |
| Options: placement, label, typed name, init timeout, stack size; reserve mailbox, memory, priority | accepted; default timeout `infinity`, not 5 s (section 7) |
| `trap_exit` at start | rejected (no window; section 17) |
| `spawn_opt` GC and priority knobs | not copied (section 2.4) |
| `#[actor]` derives the child spec; room for `significant` | derive rejected (section 17); room kept |
| Builder shape `A::new(..).spawn_opts(opts).start()` | adopted as `with_options(opts).start()` |

## 19. Open questions for the reviewers

1. **Should a waiting `start_link` from a parent's turn block that turn?**
   This is OTP, and the continuation keeps it short. A `started()` that
   requests its parent deadlocks until the request timeout, as in OTP.
2. **S8 kill escalation for `start_link` descendants:** one cleanup interval
   after the parent's D, then stuck. Is tying a supervised generation's end
   to its linked descendants (V3) acceptable for ethrex-style per-connection
   children?
3. **`init_timeout` default:** none (OTP) or 5 s (bound)?
4. **Continuation from ordinary handlers** for 0.9, at a one-branch dispatch
   cost, or later?
5. **`AlreadyStarted.existing: Option<ChildHandle>`** against a typed
   `Option<ActorRef<A>>`. The typed form makes `StartError` generic.
6. **Moving `ChildSpec::{startup_timeout, max_*}` into `StartOptions`:**
   neither client uses them, but the dynamic supervisor (#245) should adopt
   the same shape in its review.
7. **`Starting::actor_ref()`** exposes the reference before readiness. Is it
   needed beyond `spawn`?
