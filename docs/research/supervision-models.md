# Supervision models: Erlang/OTP, Elixir, Akka and Orleans

Research note, 2026-11-09. Informs ranks 12a and 13 of the
[roadmap](../../ROADMAP.md#ranked-backlog), and the
[verification rule](../../ROADMAP.md#verification-rule) rows for them. What
Spawned implements is specified in the [guide](../GUIDE.md#execution-groups-and-supervision)
and the [design contract](../design/execution-groups-supervision.md); Rust crates'
supervision is compared in [Rust actor landscape](rust-actor-landscape.md).

## What it is

A *supervisor* owns a set of children, starts them in order, watches them, and
on a child's exit decides from a policy whether to restart it, restart some of
its siblings too, or give up and fail itself so that its own supervisor decides.
Three parameters define a model: **which** children restart (the strategy),
**when** a child restarts (its restart type and the exit reason), and **how
often** before giving up (the restart intensity), plus how children stop
(shutdown timeouts). Orleans is the counterpoint: virtual actors need no
supervisor because the runtime recreates them on the next call.

## How it works

### Erlang/OTP `supervisor`

From the [`supervisor` reference](https://www.erlang.org/doc/apps/stdlib/supervisor.html):

- **Strategies.** `one_for_one` restarts only the exited child. `one_for_all`:
  "all other child processes are terminated and then all child processes are
  restarted". `rest_for_one`: the "rest" (children after the exited one in start
  order) are terminated, then it and they are restarted. `simple_one_for_one` is
  "a simplified `one_for_one` supervisor, where all child processes are
  dynamically added instances" of one child spec.
- **Restart types.** `permanent` always restarts, `transient` only after an
  abnormal exit, `temporary` never.
- **Intensity.** "If more than `MaxR` restarts occur within `MaxT` seconds, the
  supervisor terminates all child processes and then itself", with exit reason
  `shutdown`. Defaults: `intensity` 1, `period` 5. The budget is per
  supervisor, not per child, and exhaustion **escalates**: the parent sees one
  child (the supervisor) exit and applies its own policy.
- **Shutdown.** Children stop in reverse start order. Per child: `brutal_kill`
  ("unconditionally terminated"), an integer timeout after which the child is
  killed, or `infinity`; defaults are 5000 ms for workers and `infinity` for
  supervisors. `simple_one_for_one` children stop "asynchronously", so "the
  order in which they are stopped is not defined".
- **Significant children and `auto_shutdown`** (OTP 24.0). A child marked
  `significant` can end its supervisor: `auto_shutdown` is `never` (default),
  `any_significant` or `all_significant`. Used for a supervisor that exists only
  to run a group of workers to completion.
- Management calls: `start_child`, `terminate_child` (stops without restart),
  `restart_child`, `delete_child`, `which_children`, `count_children` (same
  page).

### Elixir

- **`DynamicSupervisor`** replaces `simple_one_for_one`. Its only strategy is
  `:one_for_one`; defaults `max_restarts` 3 within `max_seconds` 5;
  `max_children` defaults to `:infinity` and, when exceeded, `start_child/2`
  returns `{:error, :max_children}`; `extra_arguments` are prepended to each
  child's arguments. There is "no ordering between children", and operations
  such as shutting down run concurrently
  ([DynamicSupervisor](https://elixir.hexdocs.pm/DynamicSupervisor.html)).
- **`PartitionSupervisor`** (Elixir 1.14) "starts multiple partitions of the
  same child", by default `System.schedulers_online/0` of them, to spread load
  that would bottleneck on one process
  ([PartitionSupervisor](https://elixir.hexdocs.pm/PartitionSupervisor.html)).
- **`Task.Supervisor`** supervises one-off tasks, usually `temporary`, so a
  caller can run work under a supervisor and await or ignore it (from the Elixir
  docs; page not loaded for this note).

### Akka classic

From [fault tolerance (classic)](https://doc.akka.io/libraries/akka-core/current/fault-tolerance.html):
the parent's strategy maps an exception to a directive. **Resume** keeps the
child and its state; **Restart** replaces it with a fresh instance; **Stop**;
**Escalate** fails the parent with the same exception.
`OneForOneStrategy` applies the directive to the failed child,
`AllForOneStrategy` to all children. `maxNrOfRetries` within `withinTimeRange`
counts restarts **per child**: "the child actor is stopped if the restart count
exceeds `maxNrOfRetries` during the `withinTimeRange` duration"; it does not
fail the parent. Default strategy: stop on `ActorInitializationException`,
`ActorKilledException` and `DeathPactException`, restart on other `Exception`s,
escalate other `Throwable`s. There is no `rest_for_one`.

### Akka typed

From [fault tolerance (typed)](https://doc.akka.io/libraries/akka-core/current/typed/fault-tolerance.html):
the default flips to **stop** on an unexpected exception. A behavior opts in with
`Behaviors.supervise(...).onFailure[E](strategy)`: `restart`, `resume` or
`stop`, with `restart.withLimit(maxNrOfRetries, withinTimeRange)`; nesting
`supervise` gives per-exception strategies. Restart re-installs the initial
behavior (state comes from `Behaviors.setup`) and by default stops the actor's
children (`withStopChildren(false)` keeps them). There is no escalate directive:
a parent that `watch`es a child gets `ChildFailed`, and an unhandled
`Terminated` fails the parent with `DeathPactException`.
`SupervisorStrategy.restartWithBackoff(minBackoff, maxBackoff, randomFactor)`
adds exponential backoff; the strategy also carries `resetBackoffAfter`,
`maxRestarts` and a stash capacity for messages that arrive during backoff
(excess messages are dropped) ([API, Akka 2.5.8](https://doc.akka.io/japi/akka/2.5.8/akka/actor/typed/SupervisorStrategy.Backoff.html);
later versions not re-checked). Classic's `BackoffSupervisor` pattern wraps a
child in an intermediate actor that restarts it with backoff
([API](https://doc.akka.io/api/akka-core/2.6/akka/pattern/BackoffSupervisor$.html)).

### Orleans

No supervision tree. A grain is "a purely logical entity" whose "virtual
existence is unaffected by the failure of a server that executes it"
([overview](https://learn.microsoft.com/en-us/dotnet/orleans/overview)); after a
silo failure the next call activates it elsewhere. The lifecycle page warns that
grains "aren't always deactivated during some error cases (such as silo
crashes)", so cleanup hooks cannot be relied on
([grain lifecycle](https://learn.microsoft.com/en-us/dotnet/orleans/grains/grain-lifecycle)).
An exception in a grain method is returned to the caller and the activation
stays (our understanding of Orleans error handling; not verified for this note).
Recovery of state is the application's job through persistence.

### Spawned v0.7.0

Read from `concurrency/src/supervision.rs`, `supervision/nested.rs` and the
[guide](../GUIDE.md#construct-fresh-children-after-failure):

- **Strategies:** `OneForOne` (default), `OneForAll`, `RestForOne`. Cohorts stop
  in reverse order and all affected scopes finish before any replacement is
  built; concurrent failures coalesce into one recovery round.
- **Restart modes:** `Permanent` (default), `Transient` (restart only after an
  abnormal exit), `Temporary`: OTP's three types. Supervisor or system shutdown
  suppresses every mode. Stopping a Permanent child individually restarts it.
- **Intensity:** `max_restarts` 5 per rolling `restart_window` 30 s, **shared by
  the supervisor**, as in OTP. Initial construction is free; every later factory
  call counts, including collateral reconstruction in a cohort and failed
  factories. Exhaustion fails the whole supervisor and stops its children;
  `max_restarts = 0` fails at once. A nested subtree's failure "escalates
  through the adapter to its parent's restart policy" (`SupervisorSpec`).
- **Backoff:** per child, capped exponential (`initial` 10 ms, `maximum` 5 s)
  with full jitter (uniform in `[0, bound]`) on by default and a seed for
  reproducible tests. A child that stays ready for `stable_after` (30 s) resets
  its exponent. Collateral restarts get no extra delay. Neither OTP nor Akka
  classic has built-in backoff; this follows Akka's `restartWithBackoff`.
- **Shutdown:** reverse declaration order, per-spec `startup_timeout` and
  `shutdown_timeout` (5 s each), and a supervisor-level `shutdown(deadline)`
  report. A child stuck past its cleanup timeout fails the supervisor and
  suppresses replacements until it actually finishes: Rust cannot
  `brutal_kill` a handler mid-poll, so "kill" is cooperative cancellation.
- **Capacity:** `max_children` 1024 caps declared children
  (`SupervisorError::Capacity`).
- **Generations:** at most one live generation per slot; callers resolve the
  current one with `current()` or `wait_for_generation()`.

### Comparison

| | OTP | Elixir `DynamicSupervisor` | Akka classic | Akka typed | Spawned 0.7 |
|---|---|---|---|---|---|
| Strategies | one_for_one, one_for_all, rest_for_one, simple_one_for_one | one_for_one | OneForOne, AllForOne | Per behavior | OneForOne, OneForAll, RestForOne |
| Restart types | permanent, transient, temporary | Same | By exception (directive) | By exception | Permanent, Transient, Temporary |
| Resume (keep state) | No | No | Yes | Yes | No |
| Intensity scope | Per supervisor, then escalate | Per supervisor | Per child, then stop child | Per child (`withLimit`) | Per supervisor, then escalate |
| Defaults | 1 in 5 s | 3 in 5 s | Per strategy (example 10 per minute) | Unlimited unless `withLimit` | 5 in 30 s |
| Backoff | No | No | `BackoffSupervisor` | `restartWithBackoff` | Per child, capped exponential, jitter, stable reset |
| Dynamic children | `simple_one_for_one` | Yes, `max_children` | `context.actorOf` | `spawn` | No (rank 12a) |
| Stop a child without restart | `terminate_child` | `terminate_child` | `context.stop` | `context.stop` | No API; Permanent restarts |
| Kill on timeout | `brutal_kill` / timeout | Same | Stop is asynchronous | Same | Cooperative only; stuck cleanup fails the tree |
| Supervisor ends with its children | `auto_shutdown` (significant) | — | — | — | No |

## Who uses it

- **OTP supervisors** are the standard structure of Erlang and Elixir
  applications (every OTP application has a top supervisor; from the OTP design
  principles, not separately cited here).
- **Akka**: Lightbend lists commercial users on its site; we did not verify
  which use supervision specifically. None claimed here.
- **Orleans**: see [virtual actors](virtual-actors.md#who-uses-it).

## What it means for Spawned

- **Spawned follows OTP, not Akka, on intensity.** A supervisor-wide budget that
  escalates on exhaustion is the OTP choice; kameo and Akka instead give up on
  one child. Keep it: escalation is what makes nested trees meaningful. Our
  defaults (5 in 30 s) are looser than OTP's (1 in 5 s); document why next to
  the backoff, since backoff spreads restarts in time and a tight window would
  fail trees that backoff is meant to save.
- **Rank 12a (dynamic supervisor) should copy Elixir, with two additions.** The
  proposed contract already matches `DynamicSupervisor`: one strategy
  (one_for_one), a `max_children` that rejects with a typed error, concurrent
  shutdown with no ordering, and a supervisor-wide budget. Elixir does not say
  whether a restarting child counts against `max_children`; Spawned's contract
  does (it counts), which is the case the TLA+ model must cover. Add
  `extra_arguments`-style per-call arguments (already in the contract) and a
  `terminate_child` that stops without restart.
- **Gap: stopping a child without restarting it.** OTP and Akka can stop one
  child permanently; Spawned restarts a Permanent child stopped individually
  and has no `terminate_child`/`delete_child`. Rank 12a's "explicitly stopping a
  child does not restart it" covers dynamic children; consider the same call on
  static supervisors.
- **Gap: significant children / `auto_shutdown`.** Rank 12a defers this until a
  user needs it. It is the natural shape for "run these workers, then end", and
  it interacts with rank 13 (draining): a supervisor that ends when its
  significant child finishes is a drain trigger.
- **Gap: per-child kill after timeout.** OTP's `brutal_kill` cannot exist for
  cooperative Rust code. Spawned's answer (fail the tree, keep the slot until
  cleanup really finishes) is safer than overlapping generations; rank 13 should
  keep it and add only reporting of what was unfinished.
- **Not a gap: resume.** Akka's Resume keeps state after an exception; Spawned
  loses the message and stops the actor on a handler panic
  ([panic recovery](../GUIDE.md#panic-recovery)). A handler that wants to
  continue returns an error instead of panicking. Do not add Resume: state after
  a panic mid-handler may be inconsistent.
- **Backoff reset semantics** match Akka's `resetBackoffAfter` (Spawned's
  `stable_after`). Akka's typed restart also stashes messages during backoff
  with a capacity; Spawned's replacement is a new generation, and messages sent
  to the old `ActorRef` fail with `ActorStopped` instead of being buffered. Keep
  that: buffering hides failures, and instance-bound references are decided.
- **`PartitionSupervisor`** maps to rank 27 (pools with key-hash routing), and
  **`Task.Supervisor`** to ranks 11, 11b and 15 (owned helpers and the compute
  pool), as rank 12a already notes.
- **Orleans** confirms that virtual actors (rank 26) can replace supervision
  for keyed entities, but only with durable state (ranks 49-51); without it
  reactivation loses in-memory state exactly as a restart does.

## Sources

- [Erlang/OTP `supervisor` reference](https://www.erlang.org/doc/apps/stdlib/supervisor.html)
- [Elixir `DynamicSupervisor`](https://elixir.hexdocs.pm/DynamicSupervisor.html), [`PartitionSupervisor`](https://elixir.hexdocs.pm/PartitionSupervisor.html)
- [Akka fault tolerance (classic)](https://doc.akka.io/libraries/akka-core/current/fault-tolerance.html)
- [Akka fault tolerance (typed)](https://doc.akka.io/libraries/akka-core/current/typed/fault-tolerance.html)
- [Akka typed `SupervisorStrategy.Backoff` (2.5.8 API)](https://doc.akka.io/japi/akka/2.5.8/akka/actor/typed/SupervisorStrategy.Backoff.html), [`BackoffSupervisor` (2.6 API)](https://doc.akka.io/api/akka-core/2.6/akka/pattern/BackoffSupervisor$.html)
- [Orleans overview](https://learn.microsoft.com/en-us/dotnet/orleans/overview), [grain lifecycle](https://learn.microsoft.com/en-us/dotnet/orleans/grains/grain-lifecycle)
- Spawned: `concurrency/src/supervision.rs`, `concurrency/src/supervision/nested.rs`, [guide](../GUIDE.md#execution-groups-and-supervision)
