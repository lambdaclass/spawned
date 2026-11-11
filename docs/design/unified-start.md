# Unified start API (0.9): design contract

Status: design in review, revision 2, not implemented. Target release 0.9.
The implementation follows after two independent reviews. It is designed
against the 0.8 API as #240 ships it: exit signals, `try_link`, `NoProc`, and
the 0.8 API-review cleanups (`send_request`, `#[non_exhaustive]` builders).

Revision 2 answers both round-one reviews (Codex and agy, both "approve after
the listed changes"). What changed and why is in
[Review resolutions](#20-review-resolutions).

- **Model:** [`verification/tla/Start.tla`](../../verification/tla/Start.tla),
  with configurations `MCStart`, `MCStartTimeout`, `MCStartContinue`,
  `MCStartScope`, `MCStartLive` and `MCStartDeep`.
- **Client mapping:** [unified-start-client-mapping.md](unified-start-client-mapping.md).
- **Agreed shapes with other designs:**
  - the dynamic supervisor (#245, revision 3) and introspection v1 (#256);
  - see [section 18](#18-cross-design-agreement).
- **Inputs:**
  - the 0.8 API review: the Start cluster, footgun 5.11 and rank 19;
  - the OTP, Elixir and Gleam gap report: G1, G2, G7, G8, G14, G15 and
    section 3.1.
- **OTP sources:** `proc_lib.erl`, `gen.erl` and `gen_server.erl` at OTP-27.1.
  A line reference such as `gen.erl:191` points into those files.

## Summary

| # | Decision | OTP |
|---|---|---|
| 1 | `start`, `start_link` and `start_monitor` wait for the child's `started()`. They answer `Started` when it is ready, `Ignored` for `Init::ignore()`, or the error. An error is returned only after the child's publication: it is dead, its state is dropped and its name is free | `gen_server:start*` are synchronous on `init/1`; `sync_start` awaits the `'DOWN'` |
| 2 | `spawn` does not wait; `ActorRef::ready()` returns the same committed result later | `proc_lib:spawn` |
| 3 | `started()` may fail. It returns `()`, `Result<(), E>` or `Init<Self>`: ready, continue, error, stop or ignore | `init/1` returns `{ok,S}`, `{ok,S,{continue,C}}`, `{error,R}` (OTP 26 `init_fail/3`), `{stop,R}` or `ignore` |
| 4 | Each start has one **startup cell** with one terminal commitment: ready, failed, ignored or timed out. It is committed exactly once, by the child at the end of `started()` or by the init timer. Readiness is final and independent of the waiter | `init_ack` is sent before the loop starts; the timer and the ack race in the caller's `receive` |
| 5 | `Init::continue_with(msg)`: a turn that runs before any mailbox message. It is not a barrier against termination | `handle_continue` |
| 6 | One `StartOptions<A>`, typed by the actor: placement, typed name, init timeout (default infinity), label, stack size, per-instance bounds, and reserved slots for 0.10 | `{local,Name}`, `{timeout,T}`, `spawn_opt`, `set_label` |
| 7 | Linked and monitored children inherit the parent's placement. A linked child in the parent's system is owned by the parent's scope (S8) | none (one node); see [section 4](#4-placement-and-inheritance) |
| 8 | Stage 1 is one commit under a fixed lock order: admission, the parent's state, the edge and the name, all or nothing. A refused start is never observable | `spawn_opt([link, monitor])` is atomic |
| 9 | While the starter waits, a failed start's `Exit` (trapping parent) or `Down` is consumed by the error. A non-trapping parent still exits on a non-`Normal` init failure. An init timeout never reaches the parent | `sync_start`, `flush_EXIT`, `kill_flush_EXIT`, `gen:monitor_return` |
| 10 | Supervised children keep `startup_timeout`, the deadline for the whole attempt (factory and `started()`); infinity is allowed. `ignore` keeps the spec, as OTP | `supervisor.erl` |
| 11 | Removed: `start_on`, `start_with_backend`, `ExecutionGroup::start`, `start_linked` | |

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

- **Footgun (API review 5.11).** A linked child of a managed actor is outside
  the parent's group, ordered shutdown and helpers.
- **No atomic monitor at start.** `ctx.monitor` after `start()` races the
  child's death, and a child that already died reports `NoProc`.
- **Startup failure is invisible outside supervision.** `started()` cannot
  fail.
- **No continuation.** A self-send from `started()` queues behind anything
  that arrived during `started()`.
- **Linking at start covers only one placement:** `start_linked` is
  Async-only, and `start_on` has no linked form.

## 2. API

### 2.1 tasks (`spawned_concurrency::tasks`)

```rust
pub trait ActorStart: Actor {
    fn spawn(self) -> ActorRef<Self>;                                   // proc_lib:spawn
    fn start(self) -> Starting<Self, ActorRef<Self>>;                   // gen_server:start
    fn start_link<P: Actor>(self, parent: &Context<P>) -> Starting<Self, ActorRef<Self>>;
    fn start_monitor<P: Actor + Handler<Down>>(self, parent: &Context<P>)
        -> Starting<Self, (ActorRef<Self>, MonitorRef)>;
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

/// The child already exists (stage 1 ran in the call). Awaiting it reads the
/// startup cell; dropping it abandons the wait (section 5.4). 'static + Send.
#[must_use = "the actor runs; await this to learn whether it started"]
pub struct Starting<A: Actor, T> { /* ... */ }
impl<A: Actor, T> IntoFuture for Starting<A, T> {
    type Output = Result<StartOutcome<T>, StartError>;
}
impl<A: Actor, T> Starting<A, T> {
    /// The unready instance, without waiting or consuming the result (Err
    /// only for a stage-1 refusal). For wiring during initialisation and for
    /// keeping control of a stuck initializer.
    pub fn actor_ref(&self) -> Result<ActorRef<A>, &StartError>;
}

/// OTP {ok, Pid} | ignore. Shared with the dynamic supervisor (section 18).
#[must_use]
pub enum StartOutcome<T> { Started(T), Ignored }
impl<T> StartOutcome<T> {
    pub fn started(self) -> Option<T>;
    pub fn is_ignored(&self) -> bool;
}

impl<A: Actor> ActorRef<A> {
    /// The committed startup result of this instance, for every placement.
    pub async fn ready(&self) -> Result<StartOutcome<()>, StartError>;
}
```

### 2.2 threads (`spawned_concurrency::threads`)

The verbs and `with_options` have the same names. The waiting forms block
and return `Result<StartOutcome<T>, StartError>`. `ready()` blocks. threads
mode has one placement (a thread), so its `StartOptions` has no `group`,
`backend` or `placement_of`; these are tasks-only, like `ActorSystem`. The
only difference between the modes is `.await` against blocking, as for
requests.

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
    pub fn ready() -> Self;                                   // {ok, S}
    pub fn continue_with<M: Message>(msg: M) -> Self where A: Handler<M>;  // {ok, S, {continue, C}}
    pub fn error(cause: impl Display) -> Self;                // {error, R}: exit Normal (OTP 26 init_fail/3)
    pub fn stop(reason: ExitReason) -> Self;                  // {stop, R}: exit R
    pub fn ignore() -> Self;                                  // ignore: exit Normal
}
impl<A: Actor> From<()> for Init<A>;                              // ready
impl<A: Actor, E: Display> From<Result<(), E>> for Init<A>;       // Err -> error
impl<A: Actor, E: Display> From<Result<Init<A>, E>> for Init<A>; // Err -> error
```

The signature was prototyped with rustc 1.99. These all compile, and existing
actors change nothing:

- `async fn started(..) {}` and the default body;
- `-> Result<(), io::Error>`, `-> Result<Init<Self>, String>`, and the
  threads form.

The `#[started]` macro moves the method into `impl Actor` with its signature
intact. `Init::continue_with` is generic over `M` with `A: Handler<M>`. It
needs no associated type: an associated-type default (`type Continue = ()`)
is still unstable on stable Rust. The continuation is boxed only when one is
returned.

`Init::stop(ExitReason::Normal)` ends the child `Normal` and answers
`InitExited { reason: Normal }`. Use `Init::error` for a diagnostic and
`Init::ignore` for a deliberate opt-out. `Init::stop` is for a reason that
links should see, as `{stop, R}`.

**Resolution when `started()` returns.** Signals received during `started()`
are held (0.8 phase 0, otp-exit-signals E2/E3). They are resolved with the
trap flag that `started()` leaves, in this precedence. The first matching row
decides both the starter's result and the final exit reason.

| # | Condition | Exit reason | `stopped()` | Startup cell |
|---|---|---|---|---|
| 1 | the init timer committed `timedout` (section 6) | `Killed` | no | `timedout` → `InitTimeout` |
| 2 | a kill arrived (`kill()`, `exit(Kill)`, the S8 scope deadline) | `Killed` | no | failed → `InitExited { Killed }` |
| 3 | the child does not trap, and a held non-`Normal` signal (parent exit, link notice, exit signal) arrived; the first in arrival order wins | that reason | no | failed → `InitExited { r }` |
| 4 | `started()` panicked | `Panic("panic in started(): ..")` | no | failed → `InitExited { Panic }` |
| 5 | `ctx.stop*(r)` was called in `started()` | `r` | yes (a self-exit, L1) | failed → `InitExited { r }` |
| 6 | `Init::stop(r)` | `r` | no | failed → `InitExited { r }` |
| 7 | `Err(e)` / `Init::error(e)` | `Normal` | no | failed → `InitFailed { cause: e }` |
| 8 | `Init::ignore()` | `Normal` | no | ignored → `Ok(Ignored)` |
| 9 | `Init::ready()` / `continue_with(m)`, but a managed scope's admission closed | `Shutdown` | no | failed → `InitExited { Shutdown }` |
| 10 | `Init::ready()` / `continue_with(m)` | runs | later | **ready** → `Ok(Started(..))` |

- **Held signals in rows 9-10.** A held `Normal` signal to a non-trapping
  child is dropped (E2). For a child that traps, every held non-kill signal is
  queued as a mailbox entry in arrival order. That includes a parent exit,
  even a `Normal` one (E3: it ends the child when handled). The child
  commits ready, runs its continuation first, then handles the queued entries.
  This is OTP's behaviour for an `init/1` that sets `trap_exit` and returns
  `{continue, C}`.
- **A superseded diagnostic.** When a higher row wins over a returned
  `Init::error(e)`, `e` is logged at debug level with the actor id. The
  result is the winning row's.
- **Cooperative deviation.** OTP applies a signal when it arrives, so a
  signal before `process_flag(trap_exit, true)` kills a non-trapping process
  immediately. Spawned reads the flag when `started()` returns. This is
  0.8's documented Rust deviation (phase 0, R: no preemption). Section 11
  labels it as such.

### 2.4 `StartOptions<A>`

```rust
#[non_exhaustive]
pub struct StartOptions<A: Actor> { /* private; PhantomData<fn() -> A> */ }
impl<A: Actor> Clone for StartOptions<A>;    // no A: Clone bound
impl<A: Actor> Default for StartOptions<A>;
impl<A: Actor> StartOptions<A> {
    pub fn new() -> Self;
    pub fn group(self, group: &ExecutionGroup) -> Self;          // tasks
    pub fn backend(self, backend: Backend) -> Self;              // tasks
    pub fn placement_of<P: Actor>(self, ctx: &Context<P>) -> Self; // tasks
    pub fn name(self, name: impl Into<Name<A>>) -> Self;
    pub fn label(self, label: impl Into<Arc<str>>) -> Self;     // initial set_label
    pub fn init_timeout(self, timeout: impl Into<Timeout>) -> Self;
    pub fn stack_size(self, bytes: usize) -> Self;
    pub fn max_outstanding(self, max: usize) -> Self;
    pub fn max_native_jobs(self, max: usize) -> Self;
}
pub enum Timeout { After(Duration), Infinity }        // From<Duration>
impl<A: Actor> From<&ExecutionGroup> for StartOptions<A>;
impl<A: Actor> From<Backend> for StartOptions<A>;
```

| Field | Default | Meaning | OTP |
|---|---|---|---|
| placement | `start_link`/`start_monitor`: the parent's; otherwise unmanaged `Async` | section 4; the last of `group`/`backend`/`placement_of` wins | none |
| `name` | none | committed with the start, released at publication (section 7) | `{local, Name}` |
| `label` | none | the initial value of introspection's `ctx.set_label` (#256): tracing, diagnostics, `ActorInfo.label` | `set_label/1` (OTP 27) |
| `init_timeout` | `Infinity` | section 6 | `{timeout, T}`, default `infinity` (`gen.erl:716-722`) |
| `stack_size` | runtime default | only where the actor owns a thread (threads mode, `Backend::Thread`) | none |
| `max_outstanding`, `max_native_jobs` | the `Actor` constants | per instance | none |

**Validation.** It happens at stage 1, before anything is visible, and
returns `InvalidOptions`:

- a zero `init_timeout` (OTP's `{timeout, 0}` would always fail; use a real
  bound);
- a zero `max_outstanding`, `max_native_jobs` or `stack_size`;
- a `stack_size` on a pooled placement;
- a `backend` in a `ChildSpec`.

These rules are the same in both modes.

**Typing.** `StartOptions<A>` is typed by the actor, and `name` takes a
`Name<A>`. Passing a `Name<B>` to an `A` start is a compile error (trybuild
`name_type_must_match_actor`). `Name<A>` and `StartOptions<A>` are `Clone`
and `Send` without bounds on `A`. `Name<A>` keys the global registry until
rank 12 scopes it.

**Reserved for 0.10:** `mailbox(..)` (rank 17), `max_memory` (rank 18a), and
a message-priority opt-in (G11).

**Not copied from `spawn_opt`:** `fullsweep_after`, `min_heap_size`,
`message_queue_data`, `priority`, `hibernate_after` and `debug`. None has a
counterpart in a cooperative runtime without per-actor heaps.

### 2.5 `StartError`

```rust
#[derive(Debug, Clone, thiserror::Error)]
#[non_exhaustive]
pub enum StartError {
    // stage 1: nothing exists
    Closed,                       // group, system or the parent's scope no longer admits
    Capacity,                     // thread_group full
    AlreadyStarted { name: String, existing: Option<ChildHandle> },
    ParentExited,                 // start_link/start_monitor through a Context whose exit began
    InvalidOptions(&'static str),
    System(SystemError),          // executor or runtime construction failed
    // stage 2: the child existed and is gone (or still cleaning up: `pending`)
    InitFailed { cause: String, pending: Option<ChildHandle> },
    InitExited { reason: ExitReason, pending: Option<ChildHandle> },
    InitTimeout { pending: Option<ChildHandle> },
}
```

- **`pending`.** It is `None` when the child was published before the
  answer: dead, state dropped, notices sent, name and slot free. It is
  `Some(handle)` when the bounded wait for publication expired first
  (section 6). The handle is instance-bound: `wait_exit` reaches the real
  end, and the name and slot stay held until then.
- **`ignore`** is the `Ok(StartOutcome::Ignored)` outcome, not an error, as
  OTP returns `ignore` beside `{ok, Pid}` (Codex 9).
- **Removed:** `SystemError::StartupFailed`. `ready()` returns these
  variants.

## 3. Stage 1: one commit

Stage 1 runs inside the call, before any `.await`, in two parts.

1. **Prepare (invisible).** Validate the options; build the actor's shared
   state, mailbox, startup cell and edges in private memory; and acquire the
   executor resources. For `Backend::Thread` and threads mode, spawn the
   thread parked on a start latch. For an async placement, reserve the task
   slot. A failure returns `System` or `InvalidOptions`, and the state is
   dropped outside every framework lock. Nothing was visible.
2. **Commit (atomic).** Take, in this order:
   1. the target group's admission lock;
   2. the parent's exit-request lock, the one an exit begin takes;
   3. the parent's link or monitor table lock;
   4. the registry lock.

   Under all of them, check in any order:
   - the group is open and below capacity (`Closed`, `Capacity`);
   - the parent's exit has not begun (`ParentExited`);
   - the name is free (`AlreadyStarted`).

   On success, apply everything before releasing:
   - the admission slot;
   - the parent's edge, created with the startup cell's mark;
   - the name;
   - the actor's entry in introspection's actor table.

   Release in reverse order, then open the start latch, which cannot fail.
   On a refusal nothing was applied: release the locks, drop the prepared
   state and stop the parked thread.

**Guarantees.** A refused start is never observable: no `whereis` can return
it, no `Capacity` refusal is caused by it, and no edge, notice or hook
exists. Every admission becomes visible at one instant. This is the
`spawn_opt([link, monitor])` atomicity. Model: `AbortInvisible`, with a
`Lookup` observer; control `SplitCommit`, which registers the name before the
rest of the commit.

**Lock-order obligations for the implementation.** The order above is global.
No 0.8 path takes a registry lock and then a link-table, exit-request or
group lock, and the implementation keeps it so. A Loom test covers a commit
racing the parent's exit begin and a concurrent commit of the same name.

**Bare `spawn()`** is infallible in its signature. An executor construction
failure in the prepare step panics with the `SystemError`, as
`std::thread::spawn` does. `with_options(..).spawn()` returns it instead.

**Forgotten `.await`.** Stage 1 is eager, so `x.start();` without `.await`
still starts the actor, and the dropped `Starting` abandons the wait
(section 5.4). In the migration a 0.8 statement `x.start();` therefore keeps
working. It also warns under `#[must_use]`, which the clients' clippy
`-D warnings` gate turns into an error.

## 4. Placement and inheritance

**Rule.** The placement is the first of:

1. an explicit placement;
2. for `start_link` and `start_monitor`, the parent's placement;
3. unmanaged `Backend::Async` (threads mode: a thread).

| Parent | Child of `start_link`/`start_monitor` |
|---|---|
| managed, async group `g` | `g` |
| managed, thread group `g` | `g` (any free slot; `Capacity` if none) |
| managed, pinned group `g`, shard `i` | `g`, shard `i` |
| supervised child on `g` | `g` |
| unmanaged, backend `b` | unmanaged, backend `b` |
| threads mode | a thread |

**No fallback.** A closed or full inherited group fails the start (`Closed`,
`Capacity`).

**Only `start_link` and `start_monitor` inherit.** Plain `start`/`spawn` take
no `Context`. Inheriting for them would need an ambient current actor, which
costs something on every turn. It would also not follow work into helpers,
and would silently move 20 ethrex call sites into groups.
`placement_of(ctx)` is the explicit form. OTP's `gen_server:start` and
`start_monitor` also give the child no parent (`gen.erl:137-142`).

### 4.1 S8: linked children owned by the parent's scope

Ownership is decided separately from inheritance, by this rule:

- **Owned.** A `start_link` child is owned by its parent's scope when the
  parent is managed and the child is admitted to a group of the parent's
  `ActorSystem`. That covers an inherited group, an explicit group of the same
  system, and a pinned shard.
- **Not owned.** A `start_monitor` child, a child placed unmanaged, and a
  child in another system. It is linked only, as in OTP, and is owned by its
  own placement: the other system's shutdown, or nobody (S6).

**What ownership means.**

- **System shutdown.** It reaches an owned child through its parent. The
  parent's turn in the reverse admission order (S2) ends when the parent and
  its owned descendants are published, or at the grace deadline.
- **Supervised parents.** The generation counts as finished (V3) only when
  its owned descendants are published too. A replacement never overlaps
  them.
- **Unlink.** Ownership persists after `unlink`. That is not OTP link
  semantics; it is scope ownership, justified by Rust resource lifetimes. A
  descendant may hold resources (sockets, files, a native slot) that the
  parent's replacement or the system's shutdown report must see released, as
  for 0.8's registered helpers.

**Deadlines.** One interval, the parent's row in the E3 deadline table,
covers the whole scope:

- `shutdown_timeout` from the parent's signal when the supervisor or system
  stops it;
- `shutdown_timeout` from the exit begin when the parent exits on its own
  (5 s unmanaged default).

At its expiry the scope deadline sends a kill to every still-alive member,
the parent and each owned descendant, linked or not. Each member is killed
directly; nothing waits for the parent's D. One more interval later, the
scope is stuck, with the existing V3/E3 consequences.

A parent that never reaches D does not delay the descendants' kill. System
grace expiry likewise kills every remaining actor of every scope directly
(S2, S3). A descendant still linked normally ends earlier, by its parent
exit at the parent's D (E3).

Model: `MCStartScope`, with invariant `GenDoneMeansOwnedDone` (control
`EarlyGenDone`) and liveness `OwnedScopeFinishes` (control `NoScopeKill`: the
deadline spares unlinked descendants).

## 5. The startup cell and `start_link`/`start_monitor`

### 5.1 The cell

Each start that has a waiter (`start`, `start_link`, `start_monitor`), and
each start with an init timer, gets one cell. Its two parts:

```text
commitment: Pending -> Ready | Failed(cause, reason) | Ignored | TimedOut   (exactly once)
waiter:     Waiting -> Consumed | Abandoned                                  (exactly once)
```

The cell has one lock. The edges created at the commit carry the cell's mark.

**Ready.** The child commits it at the end of `started()` under the cell
lock, when row 10 of 2.3 applies and (for managed actors) admission is still
open. The same critical section:

1. commits `Ready`;
2. disarms the init timer;
3. clears the start marks on the edges;
4. publishes the result to the waiter and to `ready()`.

Only then does the child run its continuation or dispatch. Readiness is
final: a later death of the child is an ordinary death with an ordinary
`Exit`/`Down`, whether or not the waiter has polled yet (agy P0-1, Codex 1).
OTP sends `init_ack` before entering the loop (`gen_server.erl:2185-2201`).
Model: `ReadyIsFinal` (control `DivertAfterReady`), `ResultMatchesCommit`,
`ReadyOnlyWhileOpen` (control `ReadyAfterClose`).

**Failed / Ignored.** The child commits it at the end of `started()` (rows
2-9), or at a kill before `started()` began.

**TimedOut.** The framework-owned init timer commits it (section 6).

Exactly one of these wins the `Pending` state. In particular, the timer can
win only while the cell is `Pending`, and the child's readiness commit can
win only while the timer has not committed (`TimeoutNeverKillsReady`, control
`TimerIgnoresCommit`; agy P0-2, Codex 2).

### 5.2 Edges, death and publication

The child's death has three steps, as in otp-exit-signals:

1. **D.** The edges close and their notices are taken.
2. **Destruction.** Queued messages and the state `A` are dropped, outside
   framework locks.
3. **Publication**, in this order:
   1. release the name;
   2. release the admission slot;
   3. dispose of each taken notice through the cell;
   4. publish `wait_exit`;
   5. answer the waiter.

The waiter answers only from publication (agy P0-3, Codex 5), so a retry never
overlaps the old instance's `Drop`. This is OTP's completed-failure boundary,
since `sync_start` awaits the `'DOWN'`. Model: `FailureAfterPublication`
(control `EarlyFailAck`), `NoOverlapOnReuse` (control `UnregisterAtDeath`),
`NameFreeBeforeObservation` (control `LateUnregister`).

**Disposition of a taken notice** under the cell lock at publication:

| Cell | Waiter | Trapping parent (`Exit`) | Non-trapping parent | Monitor (`Down`) |
|---|---|---|---|---|
| `Ready` (any waiter) | any | delivered | applied if non-`Normal` (X3) | delivered |
| `Failed`/`Ignored` | `Waiting` | diverted to the cell, consumed by the error | **applied if non-`Normal`**: the parent exits after its current handler (OTP: the link kills the caller) | diverted, consumed |
| `Failed`/`Ignored` | `Consumed` (bounded wait expired) | dropped | applied if non-`Normal` | dropped |
| `TimedOut` | `Waiting` / `Consumed` | dropped | **not applied** | dropped |
| any non-`Ready` | `Abandoned` | delivered | applied if non-`Normal` | delivered |

The timeout rows are OTP's `kill_flush_EXIT`, which unlinks before the kill.
Spawned keeps the edge and suppresses its notice instead, which is the same
observably, and the waiter decides at publication. Model: `FailureFlushed`
(controls `NoLinkFlush`, `NoMonitorFlush`), `TimeoutContained` (control
`KillBeforeUnlink`), `NoticeOnce` (control `DoubleNotice`).

### 5.3 Parent state

- **Running**, the normal case: the edge is committed, and the parent's turn
  waits. This is OTP and needs no extra protection, but the deadlock is
  OTP's too. If the child's `started()` sends a request to its parent, it
  waits until its request timeout (5 s by default, R3), because the
  parent's turn is blocked in `start_link`. OTP documents the same
  `gen_server:call`-from-`init` deadlock. Slow initialisation belongs in a
  continuation; a waiter that must not block moves the `Starting` to a job
  explicitly.
- **Exit began or past D** (only through a cloned `Context`): the commit
  refuses with `ParentExited`. The check is under the parent's exit-request
  lock, so a start from `stopped()` is refused too (agy P1-9). Model:
  `NoEdgeToDeadParent` (control `LinkCheckLate`).
- **Dies after the commit:** its D sends each linked child a parent exit,
  which resolves under section 2.3. Model: `ParentExitReachesChild`
  (liveness; control `DropHeld`).

### 5.4 Abandoning the wait

Dropping the `Starting` sets the waiter to `Abandoned` under the cell lock. An
abandoned wait never consumes anything:

- **Already diverted.** A notice already diverted to the cell is handed to
  the parent as an ordinary notice, exactly once (`AbandonedNoticeDelivered`,
  which checks the kind and the reason; control `AbandonDropsNotice`).
- **Later.** A later notice is ordinary (table above).
- **Timer.** The init timer is unaffected (section 6).

An abandoned `start` is `spawn`.

### 5.5 Answering

| Committed | Waiter answers |
|---|---|
| `Ready` | at once: `Ok(Started(..))` |
| `Ignored` | after publication: `Ok(Ignored)` |
| `Failed` | after publication: `InitFailed`/`InitExited { pending: None }` |
| `TimedOut` | after publication: `InitTimeout { pending: None }` |
| any failure, publication not reached by the bounded wait | at the bound: `pending: Some(handle)` |

The bounded wait is the placement's cleanup deadline: 5 s unmanaged, or the
group's or spec's otherwise. `ready()` reads the same cell and answers the
same value, at any time and any number of times (agy P1-6). Model:
`StartAnswered` (liveness; control `NoAnswerIgnore`).

## 6. Init timeout

`init_timeout(d)` arms a framework-owned timer at the commit, only when the
option is set. The waiter does not own it, so it runs whether or not anyone
polls, and abandonment does not change it (Codex 2).

At expiry, under the cell lock:

- **Cell `Pending`:** commit `TimedOut`, then send a kill. The start edges
  stay. Their notice is disposed of at publication by the waiter: suppressed
  while waiting or consumed, ordinary if abandoned (5.2).
- **Already committed:** nothing happens (`TimeoutNeverKillsReady`).

**Stuck policy (explicit, agy P1-5).** The kill is applied cooperatively,
like every kill (L3). It does not interrupt a running `started()`:

- a `started()` that never returns keeps the actor, its name, its admission
  slot and its state;
- the waiter answers `InitTimeout { pending: Some(handle) }` at the bounded
  wait;
- the system reports the actor as unfinished;
- under a supervisor the slot follows V3's stuck rule.

There is no thread abort and no dropping of the `started()` future at an
`.await`. Aborting a turn at an await is the 0.10 design introspection lists
("turn abort at an await"). It would cover `started()` and handlers alike,
with its own model, rather than adding a special case here.

**Default.** `Infinity`, as OTP. `with_options(..)` accepts
`Timeout::Infinity` explicitly.

## 7. Registered names

- **When.** At the stage-1 commit, before `started()`, so the child can be
  found while it initialises. Messages queue, and a continuation still runs
  first. OTP registers in `gen:init_it` before `init/1`
  (`gen.erl:194-201`).
- **Clash.** `AlreadyStarted { name, existing }`, decided in the commit, so
  nothing is created. `existing` is the holder's `ChildHandle`, or `None` for
  a non-actor value. OTP returns `{already_started, Pid}`, and its loser exits
  `normal` unobservably.
- **Release.** At publication, after destruction and before any notice,
  `wait_exit` or answer (5.2). The name is held from the commit until then.
  A new holder therefore never overlaps the old one's `Drop`, which is
  stricter than OTP's unregistration at exit; Rust has destructors. A child
  stuck in `started()` or in `Drop` keeps its name until publication, and
  the waiter is told so through `pending`.
- **Supervised children.** Each generation registers the name, and the
  replacement waits for the old one's publication (V3).
- **Kept from 0.8.** `ctx.register_name` remains for names chosen after
  start. The registry stays global until rank 12.

## 8. Continuation (`handle_continue`)

`Init::continue_with(msg)` runs `Handler<M>::handle(msg)` as the first turn
after the readiness commit, before any mailbox entry. Mailbox entries include
messages that arrived during `started()` and signals queued for a trapping
child (`ContinueFirst`, control `ContinueQueued`).

**What it does not guarantee** (Codex 8): that it runs at all, or runs to
completion, before termination.

- Readiness is committed before it starts, so the waiter and the supervisor
  already see the child ready.
- Anything that ends a ready child can end it before or during the
  continuation: a kill, an untrapped exit signal, a `terminate_child`, or
  supervisor shutdown (non-trapping: applied when the continuation's turn
  ends; trapping: queued behind it).
- A stop observed before the continuation begins discards it, as D4 discards
  queued messages.

OTP gives the same guarantee: `handle_continue` precedes `receive`, but
fatal signals still terminate the process. Work that must finish before the
start succeeds belongs in `started()`.

- **Under supervision.** A panic in the continuation is a crash of a ready
  child (restart policy), not a start failure.
- **Cost.** One branch after `started()`; the dispatch loop is unchanged.
- **Not in 0.9:** continuing from ordinary handlers. That needs a branch per
  turn; maintainer decision 4.

## 9. Supervisors

The OTP child spec separates `start` (MFA and options) from `restart`,
`shutdown`, `type` and `significant`. `ChildSpec` mirrors it:

```rust
ChildSpec::new(id, &group, |fctx| async { Ok(MyActor::new(..)) })   // or Ok(ChildStart::Ignore)
    .options(StartOptions::new().name("rpc").label("http").max_native_jobs(8))
    .startup_timeout(Duration::from_secs(10))        // whole attempt; Timeout::Infinity allowed
    .restart(RestartMode::Transient)
    .shutdown_timeout(Duration::from_secs(5))
```

- **Factory result.** The factory returns
  `Result<impl Into<ChildStart<A>>, E>` with
  `ChildStart::Actor(a) | ChildStart::Ignore`. `From<A> for ChildStart<A>`
  keeps existing factories compiling. This is the dynamic supervisor's
  shape (section 18). A factory `Ignore` and an `Init::ignore()` are the same
  outcome.
- **`startup_timeout` stays on `ChildSpec`** (agy P1-7, Codex 3). It bounds
  the whole attempt: factory, stage 1 and `started()`. The default is 5 s
  (V7), and `Timeout::Infinity` is allowed. The actor's init timer gets the
  remaining budget at the commit, or the smaller of that and
  `options.init_timeout`. Expiry during the factory cancels it (V7, unchanged);
  expiry after the commit is the cell's `TimedOut`. The dynamic supervisor
  bounds construction and `started()` together the same way.
- **Placement.** The positional group stays: a supervised child is always
  managed, in the supervisor's system. A `group` in `options` replaces it;
  a `backend` is `InvalidOptions` at `add()`.
- **Each generation is a `start_link` with the supervisor as parent**, with
  the cell rules of section 5.
- **`ignore` (static supervisors, OTP semantics; agy P1-7, Codex 9).**
  - **The spec stays**, with no generation: `ChildState::Ignored`,
    `ChildInfo { actor: None, ready: false, state: Ignored }`.
  - **Counts.** It counts in `ChildCounts.specs` and `workers`, not `active`,
    as OTP's `count_children`.
  - **Initial start.** An ignore completes the child's first start: the
    initial-start barrier advances, the tree's `start()` succeeds, and V7 sees
    no failure. The ignored instance was already published before the
    supervisor observes it (the answer follows publication), so its cleanup
    is complete.
  - **OneForOne.** Nothing restarts an ignored child: it has no process that
    could exit. This is OTP's `undefined` pid.
  - **OneForAll / RestForOne.** A later round that covers the ignored child
    starts it again with the rest of the cohort, as OTP's `start_children`
    after `terminate_children` does. The round's readiness barrier treats
    `Ignored` as done for that child.
  - **Temporary.** An ignored Temporary child is removed, as OTP deletes it.
  - **Budget.** An ignored replacement keeps the restart attempt already
    stamped (V5) and adds nothing.
  - **Waiting.** `SupervisedChild::wait_ready()` and `current()` return
    `ChildUnavailable(ChildState::Ignored)` at once rather than waiting.
  - **Model and traces.** `Supervision.tla` and `Cohorts.tla` gain the
    `Ignored` state and the cohort rule, and traces gain an `Ignore(c, g)`
    event between `Attempt` and the round's barrier, with `trace_check.py`
    updated. This is in the implementation PR, with its own controls:
    `IgnoreRetried` (OneForOne restarts an ignored child) and
    `IgnoreSkippedInCohort` (a round leaves an ignored Permanent child
    stopped).
- **Errors.** `SupervisorError::FailedToStartChild { child, cause: ChildStartError }`,
  where:

  ```rust
  #[non_exhaustive]
  pub enum ChildStartError {
      Factory(String),                              // error or panic in the factory
      Start(StartError),                            // stage 1 or stage 2 (InitFailed, InitExited, InitTimeout)
      StartupTimeout { pending: Option<ChildHandle> },  // the whole-attempt deadline
  }
  ```

  The dynamic supervisor's `FailedToStart` carries the same type (section
  18).
- **Continuations.** A supervised child's continuation runs after
  readiness, and the supervisor does not wait for it (section 8).
- **`#[actor]` does not derive a child spec** (section 17); `ChildSpec`
  keeps room for `significant`.

## 10. Test kit (`spawned-test`)

| 0.8 | 0.9 |
|---|---|
| `Mock::start()` | `Mock::spawn()` |
| `Mock::start_on(&g)` | `Mock::spawn_with(opts)` |
| none | `Mock::on_started(\|ctx\| Init)`, `Mock::start_delay(d)` (virtual time under the simulator), `Mock::drop_flag(Arc<AtomicBool>)` (set by the mock's `Drop`; checks publication order) |
| `TestSystem::start(a)` | `TestSystem::spawn(a)`; `TestSystem::start(a).await` (waits, panics with the error) |
| `TestSystem::start_mock(m)` | `TestSystem::spawn_mock(m)` |
| none | `TestSystem::options::<A>()` |

`start_link`/`start_monitor` are exercised through `in_actor`. `Starting` is
`'static`, so it can be returned from the turn and awaited outside it.

## 11. Erlang/OTP alignment

Categories: R (Rust forces it), T (Rust types add strictness), B (a bound
Erlang lacks).

| OTP | Spawned 0.9 | Verdict |
|---|---|---|
| `gen_server:start/start_link/start_monitor`, synchronous on `init/1` | `start`, `start_link`, `start_monitor` | aligned; the parent is explicit (R: no `self()`) |
| `proc_lib:spawn/spawn_opt` | `spawn`, `with_options(o).spawn()` | aligned |
| `{ok, Pid}` / `ignore` / `{error, R}` | `Ok(Started)` / `Ok(Ignored)` / `Err` | aligned |
| `init_ack` before the loop; the ack is final | readiness commits before the continuation and is final | aligned |
| `{timeout, T}` default `infinity`; on expiry `kill_flush_EXIT`, then `{error, timeout}` after `'DOWN'` | `init_timeout` default `Infinity`; the notice is suppressed instead of unlinked; `InitTimeout` after publication | aligned observably; the wait for publication is bounded (R) |
| a kill ends `init` at once | applied when `started()` returns (L3) | R: no preemption; abort at an await is the 0.10 design |
| a signal before `process_flag(trap_exit, true)` acts at once | held, resolved with the flag at return | R: 0.8 phase 0 (cooperative) |
| `init` returns `{stop, R}` / crashes / `{error, R}` (OTP 26 `init_fail/3`) / `ignore` | `Init::stop` / panic / `Init::error` / `Init::ignore` | aligned |
| failure returned after the process is gone (`await_DOWN`) | after publication: state dropped, name free | aligned; stricter (destructors, R) |
| `{local, Name}`, `{already_started, Pid}`, name freed before the starter is notified | `name`, `AlreadyStarted { existing }`, released at publication before notices | aligned; typed (T) |
| failed `start_link`, trapping caller: `'EXIT'` consumed | consumed | aligned |
| failed `start_link`, non-trapping caller: killed by the link | the error is returned, then the caller exits after its handler | R: cooperative |
| failed `start_monitor`: `'DOWN'` consumed (`gen:monitor_return`) | consumed | aligned |
| `handle_continue` from `init` | `Init::continue_with` | aligned |
| `{noreply, S, {continue, C}}` | not in 0.9 | gap (dispatch cost) |
| `gen_server:call` to the parent from `init` deadlocks until timeout | same (request timeout) | aligned; documented |
| supervisor child `ignore`: spec kept, `undefined` pid, restarted in a later `one_for_all`/`rest_for_one` round, deleted if temporary | same | aligned |
| `set_label` (OTP 27) | `label` option, `ctx.set_label` | aligned |
| one node, no placement | inheritance for link/monitor; S8 ownership | R: no VM; ownership is a Rust resource-lifetime addition |

## 12. Migration from 0.8

| 0.8 | 0.9 | Behaviour |
|---|---|---|
| `a.start()` | `a.spawn()` | same |
| | or `a.start().await?` | waits; reports init errors |
| `a.start_on(&g)?` | `a.with_options(&g).spawn()?` | same |
| `a.start_with_backend(b)` | `a.with_options(b).spawn()?` | same; a `Result` (options or executor failure) |
| `g.start(a)?` | `a.with_options(&g).spawn()?` | same |
| `a.start_linked(&ctx)` | `a.start_link(&ctx).await?` | **changes:** inherits placement, waits, refuses an exiting parent |
| `ctx.monitor(&a.start().child_handle())` | `a.start_monitor(&ctx).await?` | atomic |
| `ctx.register_name(n)` in `started()` | `StartOptions::name(n)` | atomic; released at publication |
| `ready().await -> Result<(), SystemError>` (managed only) | `-> Result<StartOutcome<()>, StartError>`, every placement | type |
| `SystemError::StartupFailed(r)` | `StartError::InitExited { reason: r, .. }` | |
| `ChildSpec::startup_timeout(d)` | unchanged (whole attempt) | same |
| `ChildSpec::{max_outstanding, max_native_jobs}` | `.options(StartOptions::new().max_*(..))` | same |
| factory `Ok(actor)` | unchanged (`From<A> for ChildStart<A>`); `Ok(ChildStart::Ignore)` is new | same |
| `async fn started(..) {}` | unchanged | same |
| `Mock::start()` / `start_on` | `Mock::spawn()` / `spawn_with` | same |
| `TestSystem::start` / `start_mock` | `TestSystem::spawn` / `spawn_mock` | same |

## 13. Client mapping

See [unified-start-client-mapping.md](unified-start-client-mapping.md):
ethrex `c1163b7`, ethlambda `85990aa`.

- **Sites that change:** 51 (ethrex 37, ethlambda 14). All are mechanical,
  and none changes behaviour.
- **Inheritance changes behaviour at no site.**
- **Optional upgrades:** 14 async sites may use `start().await?`.
- **Error types:** six client functions change their error type from
  `SystemError` to `StartError`.
- **Supervisor specs:** the 3 `ChildSpec::new` sites are unchanged
  (`startup_timeout` stays).

Revision 2 corrects the mapping on two points:
- the RLPx handshake continuation precedes peer messages, but a
  `terminate_child` can still end the connection before it completes;
- bare `spawn()` panics on executor construction failure.

## 14. Semantics contract changes

The implementation PR writes these into `semantics.md`, with the "Checked by"
lines from section 16.

- **L1 (changed).** `stopped()` runs after a ready commit, or after a
  `ctx.stop*` called in `started()`; never after a failed or ignored init.
- **L2 (changed).** Readiness exists for every placement. It is the startup
  cell's `Ready`: committed by the child at the end of `started()`, while
  admission is open, before any continuation or dispatch. It is final.
  `ready()` and the waiting starts read the same committed value.
- **L8 (new), Start.** Stage 1 is one commit (section 3), and a refusal is
  never observable. The waiting starts answer per 5.5. Errors come after
  publication or carry `pending`.
- **L9 (new), Init resolution.** The precedence table in 2.3, including
  trapping children.
- **L10 (new), Continuation.** It precedes every mailbox entry. It is not a
  barrier against termination.
- **X5 (changed).** `start_link`/`start_monitor` commit their edge before the
  child's `started()`, on every placement. A start through an exiting or
  dead parent is refused.
- **X3 (changed).** The parent row reads "`start_link` caller".
- **X8 (new), Startup notices.** The disposition table in 5.2. The init
  timeout never reaches the parent unless the wait was abandoned. An
  abandoned wait delivers a diverted notice exactly once.
- **S6 (changed).** `spawn`/`start` are unmanaged unless given a placement.
- **S7 (new), Inheritance.** Section 4, with no fallback.
- **S8 (new), Scope ownership.** Section 4.1.
- **S9 (new), Start-time names.** Committed atomically, unique, held until
  publication, released before any notice.
- **T5 (new), Init timeout.** A framework-owned timer that loses to a ready
  commit, with the stuck policy.
- **V1/V5/V7 (changed).** `ignore` keeps the spec, as OTP. `startup_timeout`
  bounds the whole attempt. A name clash at a restart is a failed attempt.
- **Section 11 (changed).** New rows as in section 11 above.

**Non-guarantees.**

- Neither the init timeout nor a kill interrupts `started()`.
- A continuation may not run at all.
- Inheritance does not follow helpers or plain starts.
- An abandoned wait consumes nothing.
- Names are process-global until rank 12.
- The commit's global lock order is a cost at start time, never on send or
  dispatch.

## 15. The model

[`Start.tla`](../../verification/tla/Start.tla), revision 2:

- **Parent.** One parent P: managed in group `G` or unmanaged, trapping or
  not, able to exit.
- **Children.** Started with `spawn`, `start`, `start_link` or
  `start_monitor`, with a default or explicit placement and possibly the
  shared name.
- **Stage 1.** Prepare failure, then the atomic `Commit`; a `Lookup`
  observer resolves the name at any time.
- **End of `started()`.** One step with every `Init` outcome, a trap flag,
  held parent exits and kills, resolved by the 2.3 precedence.
- **The startup cell.** The commitment and the waiter state, with `AckOk`,
  `AckFail` (after publication, or at the bounded wait with `pend`),
  `Abandon`, and the init `Timeout`.
- **Death.** `Die` (D) and `Publish`, with destruction between them; group
  capacity is released at publication.
- **After readiness.** Ready children may exit, be killed or unlink; a
  continuation and messages from others; a trapping child's queued parent
  exit.
- **S8.** Owned children, the scope deadline, and the generation end.
- **Notices.** Counted per child (`exitN`, `downN`) with their reason, so a
  duplicate delivery is visible.

**Safety invariants:**

| Invariant | Meaning |
|---|---|
| `InheritedPlacement` | explicit wins; link/monitor take the parent's; otherwise unmanaged |
| `LinkBeforeStarted` | the edge was really created (at the commit) before `started()` began |
| `NameUnique` | at most one holder |
| `NameHeldUntilPublished` | a holder is between its commit and its publication |
| `NoOverlapOnReuse` | no holder while another named child is between D and publication |
| `NameFreeBeforeObservation` | no notice or final error while the child still holds the name |
| `AbortInvisible` | a refused start: no edge, name, lookup, slot, hook or notice |
| `ResultMatchesCommit` | the answer is the committed outcome |
| `ReadyIsFinal` | a ready commit is never answered as a start error |
| `TimeoutNeverKillsReady` | the init timer never kills a ready child |
| `ReadyOnlyWhileOpen` | managed readiness commits only while admission is open |
| `FailureAfterPublication` | an error without `pending` comes after publication |
| `FailureFlushed` | a failed start delivers no `Down`, and no `Exit` to a trapping parent |
| `TimeoutContained` | an init timeout reaches P only after an abandoned wait |
| `NoEdgeToDeadParent` | no edge outlives P's D |
| `AdmittedOnlyOpen`, `CapacityRespected` | admission; slots held until publication |
| `NoticeOnce` | at most one `Exit` and one `Down` per child (counters) |
| `AbandonedNoticeDelivered` | an abandoned wait hands the diverted notice to P with its kind and reason |
| `ContinueFirst` | no mailbox entry before the continuation |
| `GenDoneMeansOwnedDone` | P's generation ends only after its owned children |

**Liveness** (weak fairness on the child's own steps, publication, P's D and
the scope deadline; strong fairness on the answer):

| Property | Meaning |
|---|---|
| `ParentExitReachesChild` | a non-`Normal` parent exit ends a linked child, also when held during `started()` |
| `StartAnswered` | every waiting start is answered or abandoned |
| `OwnedScopeFinishes` | a managed parent's generation ends after it dies |

**Configurations and results** (TLC 2.19, fede, 4 workers, `nice -n 19`,
run one at a time):

| Configuration | Run | Instance | Distinct states | Depth | Time |
|---|---|---|---|---|---|
| `MCStart` | per PR | two named children, link/monitor, managed parent, Cap 3 (two children and P at once), closure, abandonment, retry | RESULT_MCStart |
| `MCStartTimeout` | per PR | one child of each waiting variant, managed or unmanaged parent, the init timer | RESULT_MCStartTimeout |
| `MCStartContinue` | per PR | one child of every variant with every explicit placement, two messages, trapping | RESULT_MCStartContinue |
| `MCStartScope` | per PR | managed parent, two linked children (owned or not), trapping, unlink, scope deadline; liveness `OwnedScopeFinishes` | RESULT_MCStartScope |
| `MCStartLive` | per PR | one child of each waiting variant, trapping; liveness `ParentExitReachesChild`, `StartAnswered` | RESULT_MCStartLive |
| `MCStartDeep` | weekly | two named children, every variant and explicit placement, trapping, unlink, scope, timer | RESULT_MCStartDeep |

**Negative controls** (`negative_checks.py`, each with `TypeOK` and only the
named property, run one at a time):

CONTROLS_TABLE

**Not modelled:**

- the lock implementation, which is covered by the Loom test in section 16;
- more than one parent;
- time (the bounded wait is a nondeterministic choice);
- destruction as a separate step (it lies between `Die` and `Publish`);
- the supervisor's `ignore` and `startup_timeout` rules, which change
  `Supervision.tla` and `Cohorts.tla` in the implementation PR (section 9).

## 16. Test plan

The implementation is held to this plan. Every guarantee in section 14 has a
check at each layer that applies, and each check must fail when the
guarantee breaks.

### 16.1 Per-guarantee matrix

| Guarantee | TLA+ / trace | Integration (`concurrency/tests/start_contract.rs`) | Sim (`start_sim.rs`, PCT + DFS) | Fuzz oracle | trybuild |
|---|---|---|---|---|---|
| L8 commit, refusal invisible | `AbortInvisible` | `refused_start_runs_no_hook_and_leaves_no_edge_or_name` (closed, capacity, clash, parent exited, prepare failure); `refused_named_start_is_never_found_by_whereis` | `commit_races_whereis_and_competing_admission` | aborted ids never in `Exit`/`Down`/registry/`Capacity` causes | `start_result_is_not_an_actor_ref` |
| L8/L2 readiness final (P0-1) | `ReadyIsFinal`, `ResultMatchesCommit` | `ready_then_immediate_crash_with_unpolled_waiter_returns_started_and_delivers_down` (link and monitor); `continuation_panic_before_waiter_resumes_is_a_crash_not_a_start_error` | `ready_commit_races_post_ready_death` | ack `Started` ⇒ hook log shows ready; post-ready deaths deliver notices | |
| L2 readiness while open | `ReadyOnlyWhileOpen` | `group_closure_immediately_before_ready_commit_fails_the_start` | `group_shutdown_during_start` | existing S1 oracle | |
| L9 resolution | (abstracted outcomes) | one test per row of 2.3, each mode and placement, for every waiting verb and `spawn` + `ready()`; `error_with_held_abnormal_parent_exit_answers_init_exited` | `held_signals_resolve_by_precedence` | generated outcomes; the result and the exit reason match the table | `started_may_return_unit_result_or_init` (ui-pass); `continue_with_requires_handler` |
| L9 trapping child (agy P1-4) | `ContinueFirst`, `ParentExitReachesChild` | `trapping_child_with_normal_parent_exit_in_started_commits_ready_runs_continuation_then_ends`; the same for an abnormal parent exit | `trapping_child_parent_exit_during_started` | | |
| L10 continuation | `ContinueFirst` | `continuation_runs_before_messages_sent_during_started`; `stop_before_continuation_discards_it`; `terminate_child_can_end_a_child_before_its_continuation` | `continuation_races_peer_send` | hook-log order | |
| X5 edge before `started()` | `LinkBeforeStarted` | `start_link_child_panicking_in_started_notifies_parent` (every placement), the same for `start_monitor` | `child_dies_in_started_racing_commit` | link/monitor atomicity | |
| X5 parent exited | `NoEdgeToDeadParent` | `start_link_through_dead_context_is_refused`; `start_link_from_stopped_is_refused` | `parent_stops_during_child_started` | | |
| X8 disposition | `FailureFlushed`, `NoticeOnce`, `TimeoutContained` | `failed_start_link_consumes_exit_of_trapping_parent`; `failed_start_link_kills_nontrapping_parent_after_handler`; `failed_start_monitor_consumes_down`; `init_timeout_never_reaches_the_parent` | `failure_races_ack` | no notice for a start that returned an error; at most one per child | |
| X8 abandonment | `AbandonedNoticeDelivered` | `abandoned_start_monitor_still_delivers_down`; `abandoned_start_link_delivers_exit`; `abandoned_start_with_timeout_delivers_killed` | `abandon_races_publication` | | |
| L8 failure after publication (P0-3) | `FailureAfterPublication`, `NoOverlapOnReuse` | `state_drop_finishes_before_failure_answer_and_name_reuse` (`Mock::drop_flag`); `hung_drop_answers_with_pending_handle_at_the_bound` | `retry_never_overlaps_old_drop` | no two live states of one name | |
| T5 timer vs readiness (P0-2) | `TimeoutNeverKillsReady`, `ResultMatchesCommit` | `init_timeout_racing_ready_agrees_with_ready()` (failpoint at the commit); `spawn_init_timeout_kills_and_ready_reports_it`; `stuck_started_keeps_name_and_reports_pending` | `timeout_races_ready_commit` | timeout options generated; a ready child never ends `Killed` by its timer | |
| `ready()` parity (agy P1-6) | `ResultMatchesCommit` | `ready_returns_the_same_result_as_the_waiter` for `Started`, `Ignored`, `InitFailed`, `InitExited`, `InitTimeout`, via `spawn` and `Starting::actor_ref()` | | | |
| S7 inheritance | `InheritedPlacement` | `link_and_monitor_inherit_{async_group,thread_group,pinned_shard,backend_async,backend_blocking,backend_thread}`; `plain_start_never_inherits`; `placement_of_opts_in` | | placement oracle | `threads_options_have_no_group_or_backend` |
| S7 closed/full | `AdmittedOnlyOpen`, `CapacityRespected` | `inherited_start_into_closing_group_fails_closed`; `..._full_thread_group_fails_capacity`; `capacity_released_only_at_publication` | `group_shutdown_during_start` | | |
| S8 ownership | `GenDoneMeansOwnedDone`, `OwnedScopeFinishes` | `system_shutdown_stops_owned_children_with_their_parent`; `supervised_generation_waits_for_owned_children`; `unlinked_owned_child_is_killed_at_the_scope_deadline`; `stuck_parent_does_not_delay_descendant_kill`; `foreign_system_and_unmanaged_children_are_not_owned` | `parent_shutdown_racing_child_start` | S1-S4/V3 oracles see owned children | |
| S9 names | `NameUnique`, `NameHeldUntilPublished`, `NameFreeBeforeObservation` | `name_clash_returns_already_started_with_holder` (the holder kept alive across both calls); `failed_named_start_can_be_retried_at_once`; `name_released_before_down_and_exit`; `typed_name_whereis`; `unregister_then_reregister_keeps_ownership`; `cross_type_name_clash_reports_none_holder` | `name_registration_race` | one live holder | `name_type_must_match_actor` |
| V1/V5/V7 supervisors | `Supervision`/`Cohorts` `Ignored` (impl PR) | `factory_ignore_and_init_ignore_keep_the_spec`; `ignored_child_restarted_in_one_for_all_round`; `ignored_child_restarted_in_rest_for_one_round`; `temporary_ignore_is_removed`; `wait_ready_on_ignored_returns_unavailable`; `startup_timeout_bounds_a_hung_factory`; `startup_timeout_infinity`; `restart_name_clash_is_a_failed_attempt` (traced, `trace_check.py`) | | V1 oracle treats `Ignored` | |
| Options | | `zero_timeout_and_zero_bounds_are_invalid_options` (both modes); `stack_size_on_pooled_placement_is_invalid`; `spawn_panics_on_executor_failure` (failpoint) | | | `start_options_struct_literal`; `start_options_clone_without_actor_clone` (ui-pass) |
| API removal | | | | | `start_on_removed`, `start_with_backend_removed`, `start_linked_removed`, `execution_group_start_removed`, `start_monitor_requires_down_handler` |
| No cost on send/dispatch | | | | | benchmark gate: the default send, request and dispatch benchmarks within noise of 0.8 (`benchmarks/`, the gate introspection #256 uses); `future_layout` sizes unchanged for the run loop |
| Lock order | | Loom: `commit_races_parent_exit_begin`, `two_commits_one_name`, `ready_commit_races_timer` (the cell's CAS) | | | |

**Placements.** Every integration test runs in:

- tasks: `Async`, `Blocking`, `Thread`, `async_group`, `thread_group`, and
  `pinned_group` on Linux;
- threads mode, except the placement rows.

They use a per-placement macro, as `semantics_gaps` does.

### 16.2 Trace validation (Codex 11, agy P1-9)

`lifecycle-trace` records the events below. A new
`concurrency/tests/start_trace.rs` records every scenario in 16.1 on `Async`,
`async_group` and `thread_group`. `TraceStart.tla` replays each trace as a
behaviour of `Start.tla`, and `trace_check.py` gains schema `start-v1`.

Every event names the code point at which it is recorded, which is its
linearization point.

| Event | Fields | Model action | Linearization point (code) |
|---|---|---|---|
| `SConfig` | parent placement, parent trap flag, Cap | `Init` | trace start |
| `SBegin` | c, kind, explicit, named | `Begin` | entry of the start call |
| `SPrepareFail` | c, error | `PrepareFail` | the prepare step's error return |
| `SCommit` | c, place, edge kind, named, owned | `Commit` | inside the commit section, all locks held |
| `SRefuse` | c, reason | `Commit` (refusal) | inside the commit section |
| `SLookup` | c | `Lookup` | `whereis` under the registry read lock (test probes only) |
| `SStartedBegin` | c | `StartedBegin` | before the first poll of `started()` |
| `SResolve` | c, outcome, trap flag, held signals, committed cell | `StartedEnd` / `KilledEarly` | under the cell lock at the end of `started()` |
| `STimer` | c, won | `Timeout` | under the cell lock in the timer |
| `SContinue` / `SHandle` | c | `RunContinue` / `Handle` | the start of the turn |
| `SExit` | c, reason | `Exit` (via `ReadyExit`, `KillReady`, `ApplyParentExit`, `HandleQueuedExit`) | exit begin (0.8 phase 2) |
| `SUnlink` | c | `ChildUnlink` | the unlink fence |
| `SDie` | c, taken link note, taken monitor note | `Die` | D (0.8 phase 4) |
| `SPublish` | c, name released, slot released, disposition per note (delivered, diverted, applied, dropped) | `Publish` | publication (0.8 phase 6), under the cell lock |
| `SAnswer` | c, result, pending | `AckOk` / `AckFail` | under the cell lock when the waiter takes the result |
| `SAbandon` | c, handed-on notice | `Abandon` | `Drop` of `Starting`, under the cell lock |
| `PExitBegin` | reason | `PExit` | the parent's exit begin |
| `PDie` | per child: queued or held parent exit | `PDie` | the parent's D |
| `PNotice` | c, kind, reason | (part of `Publish`/`Abandon`) | enqueue into the parent's mailbox, or the signal applied to it |
| `GClose`, `GScopeKill`, `GGenDone` | | `GClose`, `ScopeDeadline`, `GenDone` | group close; the scope deadline; the generation-end commit |

- **Complete consumption.** Replay must consume every event, and it may not
  invent observable events: every notice the parent sees has its own
  `PNotice` and must match the disposition recorded in `SPublish`.
- **Fault-only actions** (`RegisterFirst`, `CommitRest`, `LateLink`,
  `LateRelease`) are excluded from positive coverage. `trace_coverage.py`
  requires every other action to be reached.
- **Trace negative controls** (`require_rejected`). Each mutates a recorded
  trace, which the checker must reject:
  - move `SCommit` after `SStartedBegin`;
  - give two children overlapping names;
  - change a managed child's `SCommit` place to unmanaged;
  - add a `PNotice` after an `SAnswer` error for a trapping parent;
  - move `SAnswer` (error) before `SPublish`;
  - add an `STimer won` after an `SResolve` that committed ready.

### 16.3 Simulator

`concurrency/tests/start_sim.rs`, every race in 16.1's Sim column, each
under PCT (depths 1-3) and bounded DFS, with seeds and tapes. Each comes
with a mutation that makes it fail: the planted bugs below, or a failpoint
that reorders the commit.

### 16.4 Scenario fuzzer

- **Generator.** `SpawnHow` becomes
  `Start { verb: Spawn | Start | Link | Monitor, placement: Default | Group(g) | Unmanaged | SameAsParent, name: Option<u8>, init_timeout: Option<u8>, abandon: bool }`.
  Worker hooks gain `started()` outcomes (ready, continue, error, ignore,
  stop, panic, sleep, set `trap_exit`). Supervisor factories gain `Ignore`.
- **New oracles:**
  - placement inheritance;
  - commit invisibility;
  - readiness final;
  - notice disposition, with the non-trapping-parent rule kept;
  - names unique and released before observation;
  - continuation first;
  - S8 generation end.
- **Qualification.** Timeout containment applies only to unabandoned waits.

### 16.5 Planted bugs (`verification/planted/patches/`, #247)

Each is caught by at least two layers:

| Patch | Bug | Caught by |
|---|---|---|
| `start-link-ignores-parent-group` | 0.8 placement for `start_link` | `link_and_monitor_inherit_*`, fuzz placement oracle, trace `InheritedPlacement` |
| `start-link-after-dispatch` | edge created after the latch opens | `start_sim::child_dies_in_started_racing_commit`, trace `LinkBeforeStarted` |
| `start-name-check-then-insert` | name checked under a read lock, inserted under a separate write lock | `start_sim::name_registration_race`, fuzz name oracle, Loom `two_commits_one_name` |
| `start-monitor-down-lost-on-abandon` | a diverted `Down` dropped with the `Starting` | `abandoned_start_monitor_still_delivers_down`, `start_sim::abandon_races_publication` |
| `start-name-released-at-death` | name released at D (before destruction) | `state_drop_finishes_before_failure_answer_and_name_reuse`, trace `NoOverlapOnReuse` |
| `start-mark-not-cleared-at-ready` | readiness leaves the start marks | `ready_then_immediate_crash_with_unpolled_waiter_*`, `start_sim::ready_commit_races_post_ready_death` |
| `start-timer-without-cas` | the timer kills without checking the cell | `init_timeout_racing_ready_agrees_with_ready`, Loom `ready_commit_races_timer` |

### 16.6 Coverage matrix

- **`semantics.md`:** each guarantee in section 14 gets a "Checked by" line
  naming the checks above.
- **`docs/TESTING.md` "Semantics coverage":** a row per guarantee.
  - New rows: L8, L9, L10, X8, S7, S8, S9, T5.
  - Changed rows: L1, L2, X3, X5, S6, V1, V5, V7.
- **Merge rule.** A guarantee is merged as covered only when each clause
  fails under its planted bug or model control.

### 16.7 Client checks

The client-compat gate runs on the implementation PR, against `spawned-v0.9`
client branches that carry the mapped rewrite:

- ethrex: check, clippy `-D warnings`, `ethrex-p2p`, `p2p::`/`rpc::`, and the
  lib tests;
- ethlambda: clippy and `cargo test --workspace --profile release-fast`.

## 17. Alternatives considered

| Alternative | Why not |
|---|---|
| Keep `start()` immediate with only `ready()` | Startup failure stays invisible (G1); there is no starter for the flush and timeout rules |
| Lazy stage 1 (nothing until awaited) | A forgotten `.await` would silently never start the actor |
| Multi-step stage 1 with rollback (revision 1) | A name or capacity reservation is observable before a later refusal (Codex 4) |
| Readiness committed by the waiter's ack (revision 1) | A death after readiness but before the poll became a start error, or swallowed its notice (P0-1) |
| Unlink at the timeout (revision 1) | It races abandonment and readiness. One cell with the notice disposed of at publication is equivalent and has one owner (P0-2) |
| Answer at D (revision 1) | A retry could overlap the old `Drop` (P0-3) |
| Abort `started()` at an `.await` on a kill or timeout (agy P1-5) | Not in 0.9. Kills are cooperative everywhere (L3); aborting a turn at an await is introspection's 0.10 design and should cover handlers and `started()` together, with its own model. Until then the stuck policy (section 6) applies |
| Typed start data (`A::StartData`, Gleam's `returning`, #245's G4) | OTP's `gen_server` start returns only `{ok, Pid}`; the data channel is `proc_lib`'s, which `gen_server` does not expose. Under the match-Erlang rule, deferred; #245 drops `Started::data` (section 18). The associated type would also be required in every `impl Actor` (no stable defaults) |
| `Err(Ignored)` (revision 1) | Not Rust-forced; OTP returns `ignore` beside `{ok, Pid}`; the dynamic supervisor's `StartOutcome` already has the shape (Codex 9) |
| Inheritance for plain starts (ambient current actor) | A cost per turn, misses helpers, moves 20 client sites |
| Continuation from every handler | A branch per turn |
| `#[actor]` derives a child spec | A spec needs a factory; the derive saves nothing |
| A separate `init_timeout` on `ChildSpec` replacing `startup_timeout` (revision 1) | It dropped the factory deadline (agy P1-7, Codex 3) |
| `trap_exit` as a start option | Held signals already resolve with the flag `started()` leaves |

## 18. Cross-design agreement

These shapes are shared with the dynamic supervisor (#245, revision 3) and
introspection v1 (#256, approved). Where #245 must change, the change is
listed for its next revision. Revision 2 of this design does not edit that
branch, which its own author maintains.

| Item | Agreed shape | Unified start | #245 dynamic supervisor | #256 introspection |
|---|---|---|---|---|
| Start result | `StartOutcome<T> { Started(T), Ignored }` | `T = ActorRef<A>` or `(ActorRef<A>, MonitorRef)` | `StartOutcome<Started<A>>` (rename from `StartOutcome<A>`) | n/a |
| `Started` | `{ actor, child }` | n/a | **drop `data: A::StartData`** (deferred, section 17) | n/a |
| Factory result | `ChildStart<A> { Actor(A), Ignore }`, `From<A>` | static `ChildSpec` | as proposed | n/a |
| `ignore` | static: spec kept, `ChildState::Ignored`, restarted in later cohort rounds, Temporary removed; dynamic: nothing stored (first start) or removed (restart), no `ChildState` variant needed | section 9 | as proposed | `ChildState::Ignored` appears only for static children; counted in `specs` and `workers`, not `active` |
| Start errors | `StartError`; supervisors wrap it in `ChildStartError { Factory, Start, StartupTimeout { pending } }` | section 2.5, 9 | `FailedToStart { cause: ChildStartError, cleanup_stuck }` replaces `{ cause: String, exit }` | crash reports carry the `Display` |
| Whole-attempt deadline | `startup_timeout` covers the factory and `started()`; `Timeout::Infinity` allowed | section 9 | as proposed (default 5 s) | n/a |
| Options | `StartOptions<A>` | section 2.4 | `DynamicChildSpec::options(StartOptions<A>)`; the template's group stays positional | `label` is the initial `set_label` value |
| Continuation | `Init::continue_with(msg)` with `Handler<M>`; precedes messages, not a termination barrier | section 8 | the ethrex sketch's `#[continue_start] async fn handshake` becomes `Init::continue_with(Handshake)` plus `Handler<Handshake>`; a `terminate_child` may end the child before or during it | n/a |
| Lifecycle | `started()` running is `Lifecycle::Starting`; the ready commit moves to `Idle` | section 5.1 | n/a | as specified |
| One spawn path | every verb funnels through stage 1's commit, which inserts the actor-table entry | section 3 | n/a | as specified ("one spawn path registers every actor") |
| `ChildInfo`, `ChildCounts`, `ChildKey` | introspection's types | n/a | as proposed | owner |

## 19. Decisions on the round-one open questions

| # | Question | Decision | Source |
|---|---|---|---|
| 1 | A waiting `start_link` blocks the parent's turn | **Yes**, as OTP, with the deadlock documented (5.3) | maintainer |
| 2 | Tie a generation's end to its linked descendants | **Yes, as scope ownership** with explicit placement and deadline rules (4.1). agy did not answer this question, so Codex's answer is followed | Codex |
| 3 | `init_timeout` default | **Infinity** (OTP). Supervised children keep the 5 s whole-attempt `startup_timeout` | maintainer |
| 4 | Continuation from ordinary handlers | **Not in 0.9** (dispatch cost) | maintainer |
| 5 | `AlreadyStarted.existing` type | **`Option<ChildHandle>`**: a global name may belong to another type or a non-actor. Codex's answer; agy's answered a different question (name scope), which rank 12 owns | Codex |
| 6 | Dynamic supervisor adopts `StartOptions` | **Yes** (section 18) | maintainer |
| 7 | Keep `Starting::actor_ref()` | **Yes**: documented as an unready instance; it neither consumes nor abandons the result | Codex |

## 20. Review resolutions

### Round 1 (revision 2)

Each finding was treated as a claim and checked against the code, the design
and the model.

| Finding | Claim checked | Resolution |
|---|---|---|
| agy P0-1 = Codex 1: readiness depends on the waiter | **True** in revision 1: `res` pending kept the edge mark, and `ChildDie` → `AckFail` was reachable after `StartedEnd` committed ready. Codex's `GClose → StartedEnd → AckOk` also passed | **Accepted.** A child-side ready commit clears the marks before any continuation or dispatch (5.1). Invariants `ReadyIsFinal`, `ResultMatchesCommit`, `ReadyOnlyWhileOpen`; controls `DivertAfterReady`, `ReadyAfterClose`; tests in 16.1 |
| agy P0-2 = Codex 2: timeout, abandonment and failure arbitration | **True**: revision 1's timer acted only while a waiter existed, and the waiter path removed edges | **Accepted.** One cell with a CAS commit and a separate waiter state; a framework-owned timer; the notice disposed of at publication (5.1, 5.2, 6). `TimeoutNeverKillsReady`, control `TimerIgnoresCommit`; the bounded wait is explicit (`pend`, `pending`) |
| agy P0-3 = Codex 5: D vs publication | **True**: revision 1 released the name and answered at D, before destruction | **Accepted.** The answer, the name release and the slot release come at publication (5.2, 7). `FailureAfterPublication`, `NoOverlapOnReuse`, `NameFreeBeforeObservation`; controls `EarlyFailAck`, `UnregisterAtDeath`, `LateUnregister`. A hung `Drop` answers with `pending` at the bound |
| agy P1-7 = Codex 9: `ignore` and cross-design types | **True**: cohort behaviour was undefined; `Err(Ignored)` diverged from #245 | **Accepted.** OTP `ignore` semantics for static supervisors (9), `Ok(StartOutcome::Ignored)` everywhere, and the shared types (18). The `Supervision`/`Cohorts` model and trace updates are in the implementation PR, with named controls |
| agy P1-7 = Codex 3: factory deadline | **True** | **Accepted.** `startup_timeout` stays, covering the whole attempt, with `Timeout::Infinity` (9) |
| agy P1-8/P1-9 = Codex 10/11: weak invariants, trace validation | **Partly true.** Revision 1's `LiveInG` already excluded dead children (agy quoted a different text). But `Cap = 2` with a managed parent never held two children at once (true), capacity was released at D rather than publication (true), and the other weaknesses were true | **Accepted.** `Cap = 3`; slots are held until publication; real edge-creation history; notice counters with reasons; committed-outcome history; liveness `StartAnswered`; controls `DoubleNotice`, `ErrorAsOk`, `NoAnswerIgnore`. The lock protocol is in 3 and 5.2, and the event-to-action mapping with linearization points is in 16.2. Fault-only actions are excluded from coverage |
| Codex 4: refusal needs a commit protocol | **True** | **Accepted.** Prepare and an atomic commit under a global lock order (3); `Lookup` observer; control `SplitCommit`; Loom tests |
| Codex 6: combined outcomes and held signals | **True** | **Accepted.** The precedence table in 2.3, both the result and the exit reason; the superseded diagnostic is logged; the trap flag at return is labelled the cooperative R deviation (11) |
| Codex 7: S8 placement and deadlines | **True** | **Accepted.** Ownership is decided apart from placement, retained after unlink (justified), with one scope interval and direct kills (4.1). Modelled: `MCStartScope`, `GenDoneMeansOwnedDone`, `OwnedScopeFinishes`, controls `EarlyGenDone`, `NoScopeKill` |
| Codex 8: a continuation is not a termination barrier | **True** | **Accepted.** The guarantee is restated (8), and the client mapping and the #245 coordination row corrected (18) |
| agy P1-4: trapping child plus parent exit during `started()` | **True**: revision 1 contradicted E3 and 0.8's `resolve_held_exits` | **Accepted.** A trapping child commits ready, runs its continuation, then handles the queued parent exit (`Normal` included) (2.3). Modelled with `ChildCanTrap` |
| agy P1-5: a held kill cannot cancel a hung `started()` | **True as a description**; the proposed future-drop is rejected for 0.9 | **Partly accepted.** An explicit stuck policy with no thread abort (6). The abort-at-await mechanism belongs to the 0.10 design (17) |
| agy P1-6: `ready()` contradictions | **True** | **Accepted.** `ready()` reads the cell (5.5); parity tests |
| agy P1-10: untyped `StartOptions` | **False for this design**: revision 1 already had `StartOptions<A>` and `name(impl Into<Name<A>>)`. agy's text quotes a `named` method that never existed | **No change**, except Clone without bounds is now documented and the trybuild cases are listed |
| Codex 12 (P2) | | Zero and invalid options (2.4); `spawn()` dispatch failure (3); Clone without bounds; a no-cost benchmark gate; name ownership and cross-type tests; the two-name test holds the winner (16.1) |
| agy P2-11 | `NoticeOnce` had no control and used sets: true | Counters, control `DoubleNotice`; `MCStartContinue` runs every explicit placement |
| agy P2-12 | | `init_fail/3` cited; `Init::stop(Normal)` documented (2.3) |
| agy P2-13 | | All six tests added (16.1) |
