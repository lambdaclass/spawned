# Other concurrency models: compute pools, structured concurrency, preemption and more

Research note, 2026-11-09. Surveys concurrency models beside actors and says
which ones Spawned should borrow from. Proposes ranks 8a, 11b and 15a and
research pointers 52-54 in the [roadmap](../../ROADMAP.md#ranked-backlog), and
adds design notes to ranks 11a, 13, 15, 19, 33, 34, 39, 44, 48 and 49-51.

Client evidence is from upstream main of ethrex
[`1dfaa09ec`](https://github.com/lambdaclass/ethrex/tree/1dfaa09ec791b36d9f0ea3c613c2f3747c09f788)
and ethlambda
[`c3f5f566`](https://github.com/lambdaclass/ethlambda/tree/c3f5f566cb3a61e3d388c66ce21794c18a6618ce),
the same revisions as the [client search](../../ROADMAP.md#client-search-results).
Paths below are relative to those trees. Counts come from `git grep` and are
source reading, not profiles.

## Summary

| Model | Client evidence | Fit for Spawned | Proposed rank / status |
|---|---|---|---|
| [CPU compute pools integrated with actors](#1-cpu-parallel-compute-pools-integrated-with-actors) | **Strong.** Ethrex builds four rayon pools and calls `spawn_blocking` 50 times; a fixed rayon pool plus blocked Tokio workers deadlocked L2 import at 0% CPU (535647e45); synchronous cell serving occupied every runtime worker (d958dcba4). Ethlambda hand-owns aggregation on `spawn_blocking` and gossip checks behind semaphores with `catch_unwind`. | High: completes rank 11's pipe-to-self for the commonest case | **New rank 11b**, verification rule |
| [Structured concurrency and cancel scopes](#2-structured-concurrency-and-cancel-scopes) | Ethlambda's aggregation session hand-builds cancel token + join-with-timeout + session fencing; ethrex's unowned `tokio::spawn`s and racy shutdown | High as a design input, not a public "nursery" API | **New rank 15a** (design input to 13, 15, 19), verification rule |
| [Preemption and cooperative budgets](#3-preemption-reduction-counting-and-cooperative-budgets) | Same incidents as rank 8: handlers that fill or starve runtime workers; ethlambda's chain actor moved to `Backend::Thread` | Medium: fairness between messages is cheap; inside a handler only opt-in | **New rank 8a**, tests only |
| [Streams with backpressure](#4-streams-with-backpressure) | Unbounded and lag-dropping `spawn_listener` inputs (client sweep) | Already ranked | Design notes for rank 33 |
| [Software transactional memory](#5-software-transactional-memory) | None | Low; actor state is already single-writer | Design note for rank 11a |
| [Thread-per-core and io_uring](#6-thread-per-core-and-io_uring) | None; both clients are Tokio throughout | Low until a client I/O profile says otherwise | **Research pointer 52**, frozen |
| [Incremental dataflow](#7-incremental-dataflow) | None | Low; a different programming model | **Research pointer 53** |
| [CRDTs, sagas and process managers](#8-crdts-sagas-and-process-managers) | None in-process | Only with distribution or durability | **Research pointer 54**, with ranks 44, 49-51 |
| [CSP select, algebraic effects, join calculus](#9-briefly-csp-select-algebraic-effects-join-calculus) | None | Vocabulary and design checks | Notes for ranks 15a and 48 |

## Recommendation

1. **Managed compute pool (rank 11b).** The strongest client fit in this
   survey. Both clients move CPU work off actors by hand, and the hand-built
   versions have already deadlocked or starved a node. Build it on rank 11's
   completion contract: one bounded, owned pool per `ActorSystem`; work submitted
   from a handler returns as a message to the submitting actor; admission is
   explicit; cancellation follows the owner; a panic becomes an error message.
2. **Cancel scopes as the shared design of ranks 13, 15 and 19 (rank 15a).**
   Trio, Swift, Kotlin and Java converge on one idea: work started inside a
   scope cannot outlive it, a scope carries a deadline, and closing a scope
   cancels then joins its children. Spawned's draining, helper ownership and
   propagated deadlines are three faces of that idea and should share one
   ownership tree and one model.
3. **Cooperative budgets and per-turn work measurement (rank 8a).** Spawned
   cannot preempt Rust code, but it can do what the BEAM does for native code:
   keep fairness between messages in the framework, offer opt-in yield points
   inside long async handlers, and detect and isolate synchronous code it cannot
   interrupt (stall watchdog, placement, compute pool).

Everything else stays a design note or a research pointer.

## 1. CPU-parallel compute pools integrated with actors

### What it is

A pool of OS threads sized to the CPU, separate from the I/O runtime, that runs
CPU-bound jobs in parallel. Rayon's work-stealing pool is the Rust standard.
"Integrated with actors" means the actor framework owns the pool and the job's
result comes back to the actor as an ordinary message, so actor state is never
touched from a pool thread.

### How it works

- **Rayon.** Each worker has a local deque; idle workers steal from others.
  `par_iter`, `join`, `scope` and `spawn` split work recursively. A pool has a
  fixed thread count; work that blocks a worker (I/O, a channel wait) holds a
  thread the pool cannot replace.
- **Tokio's advice.** `spawn_blocking` "is poorly suited for expensive
  CPU-bound computations" per Tokio's maintainer, whose rule of thumb is "no
  more than 10 to 100 microseconds between each `.await`"; rayon "provides a
  thread pool specifically intended for expensive CPU-bound computations", with
  the result sent back over a `oneshot` channel
  ([Ryhl, *Async: what is blocking?*](https://ryhl.io/blog/async-what-is-blocking/)).
  The `spawn_blocking` docs add that its thread limit "is very large by
  default", that many CPU-bound computations need "a semaphore or some other
  synchronization primitive", that "rayon, may also be a good fit", and that
  blocking tasks "cannot be aborted" ([docs.rs](https://docs.rs/tokio/latest/tokio/task/fn.spawn_blocking.html)).
- **Akka dispatchers.** Akka isolates blocking work on a dedicated dispatcher,
  "a `thread-pool-executor` based dispatcher [that] allows us to limit the number
  of threads", sized to the workload
  ([Akka: blocking needs careful management](https://doc.akka.io/libraries/akka-core/current/typed/dispatchers.html#blocking-needs-careful-management)).
  The result returns to the actor by pipe-to-self, not by mutating state from
  the future ([Pekko](https://pekko.apache.org/docs/pekko/current/typed/interaction-patterns.html#send-future-result-to-self)).
- **Orleans.** Sub-tasks inherit the grain's single-threaded scheduler. Blocking
  work must "break out" with `Task.Run` onto the .NET thread pool; code after
  `await Task.Run(...)` "runs back under the scheduler active at the point the
  task was created, which is the grain's scheduler". Orleans calls this "a very
  advanced and seldom-required usage scenario"
  ([Orleans: external tasks and grains](https://learn.microsoft.com/en-us/dotnet/orleans/grains/external-tasks-and-grains)).
  The await-and-resume shape is pipe-to-self with the compiler doing the routing.
- **Ray.** Ray separates *tasks* (stateless remote functions on a reused worker
  pool) from *actors* (stateful workers), and advises "if you don't need the
  stateful part of an actor, it's better to use tasks". Task cancellation is
  best-effort; a synchronous task must poll `is_canceled()`
  ([Ray actors](https://docs.ray.io/en/latest/ray-core/actors.html)).
- **The BEAM.** A native function should return "within 1 millisecond";
  longer work goes to *dirty schedulers*, a separate pool, classified as CPU- or
  I/O-bound ([erl_nif](https://www.erlang.org/doc/apps/erts/erl_nif.html)).
  This is a VM-owned compute pool.

The common pattern: a separate, bounded pool; the actor stays responsive; the
result re-enters the actor's mailbox.

### Who uses it

Both clients, by hand.

**Ethrex** builds four rayon pools: the global pool
([`cmd/ethrex/ethrex.rs:177`](https://github.com/lambdaclass/ethrex/blob/1dfaa09ec791b36d9f0ea3c613c2f3747c09f788/cmd/ethrex/ethrex.rs#L177)),
a 17-thread merkleization pool whose "size is load-bearing ... A smaller pool
deadlocks rather than running slower"
([`crates/blockchain/blockchain.rs:572-586`](https://github.com/lambdaclass/ethrex/blob/1dfaa09ec791b36d9f0ea3c613c2f3747c09f788/crates/blockchain/blockchain.rs#L572-L586)),
a mempool prewarm pool
([`crates/blockchain/prewarm.rs:245`](https://github.com/lambdaclass/ethrex/blob/1dfaa09ec791b36d9f0ea3c613c2f3747c09f788/crates/blockchain/prewarm.rs#L245))
and a BAL warming pool deliberately wider than the core count because its jobs
block on storage reads
([`crates/vm/backends/levm/mod.rs:107-128`](https://github.com/lambdaclass/ethrex/blob/1dfaa09ec791b36d9f0ea3c613c2f3747c09f788/crates/vm/backends/levm/mod.rs#L107-L128)).
Counted in `.rs` files: 127 non-comment lines name `rayon` (most are
`#[cfg(feature = "rayon")]` gates), with about 30 parallel call sites
(`par_iter`, `into_par_iter`, `par_chunks`, `par_bridge`, `install`,
`in_place_scope`) in trie, storage, VM, sync and block code, for example
[`crates/storage/store.rs:1426`](https://github.com/lambdaclass/ethrex/blob/1dfaa09ec791b36d9f0ea3c613c2f3747c09f788/crates/storage/store.rs#L1426)
and [`crates/networking/p2p/sync/snap_sync.rs:1466`](https://github.com/lambdaclass/ethrex/blob/1dfaa09ec791b36d9f0ea3c613c2f3747c09f788/crates/networking/p2p/sync/snap_sync.rs#L1466).
(The brief for this note said "~72 uses"; we could not reproduce that number
with any single pattern, so it is not repeated here.) `spawn_blocking` appears
50 times, 20 of them in `crates/storage/store.rs` and most outside actors; three
are in the connection actor
([`rlpx/connection/server.rs:1874, 2329, 2490`](https://github.com/lambdaclass/ethrex/blob/1dfaa09ec791b36d9f0ea3c613c2f3747c09f788/crates/networking/p2p/rlpx/connection/server.rs#L2329)).

Two incidents show what the hand-built version costs:

- **Synchronous cell serving occupied every runtime worker**
  ([d958dcba4](https://github.com/lambdaclass/ethrex/commit/d958dcba4), #7404):
  KZG cell expansion "ran synchronously on the peer connection task, so a few
  concurrent requests could occupy every runtime worker". The fix bounds the
  work and moves the handler under `spawn_blocking`, which is unbounded by
  default.
- **L2 import deadlocked at 0% CPU**
  ([535647e45](https://github.com/lambdaclass/ethrex/commit/535647e45), #7429):
  each connection actor ran `add_block_pipeline` on a Tokio worker; "each
  merkleization parks 17 jobs on the blockchain's 17-thread merkle pool until
  its block is done, so concurrent imports split the pool between them and none
  can finish ... because the imports block tokio workers, RPC and logging stop
  too". The fix is a single node-wide importer that runs one import at a time
  on a blocking thread
  ([`rlpx/l2/block_importer.rs:103`](https://github.com/lambdaclass/ethrex/blob/1dfaa09ec791b36d9f0ea3c613c2f3747c09f788/crates/networking/p2p/rlpx/l2/block_importer.rs#L103)).
  Two failures compound: a gang-scheduled job (all 17 threads resident at once)
  on a shared fixed pool, and the wait for it held on runtime workers.

**Ethlambda** runs its chain actor on `Backend::Thread` with the comment "these
handlers are long synchronous CPU that starves a shared runtime"
([`crates/blockchain/src/lib.rs:433-434`](https://github.com/lambdaclass/ethlambda/blob/c3f5f566cb3a61e3d388c66ce21794c18a6618ce/crates/blockchain/src/lib.rs#L433-L434)).
Its committee-signature aggregation runs on a hand-owned `spawn_blocking`
worker that streams results back as messages, with a per-session cancellation
token, a session id to drop stale results and a 2 s join timeout before the next
session
([`crates/blockchain/src/lib.rs:1325-1428`](https://github.com/lambdaclass/ethlambda/blob/c3f5f566cb3a61e3d388c66ce21794c18a6618ce/crates/blockchain/src/lib.rs#L1325-L1428),
[`aggregation.rs:130-145`](https://github.com/lambdaclass/ethlambda/blob/c3f5f566cb3a61e3d388c66ce21794c18a6618ce/crates/blockchain/src/aggregation.rs#L130-L145)).
The semaphores and `catch_unwind` are in gossip validation, not aggregation:
`spawn_stateful_checks` takes a permit with `try_acquire_owned`, reports
`Ignore(Overloaded)` instead of queueing, runs the checks on `spawn_blocking`,
and `guarded` turns a panic into `Ignore(Internal)` so exactly one verdict
returns to the actor
([`crates/net/p2p/src/beacon/verdict.rs:645-690`](https://github.com/lambdaclass/ethlambda/blob/c3f5f566cb3a61e3d388c66ce21794c18a6618ce/crates/net/p2p/src/beacon/verdict.rs#L645-L690);
five permit pools at
[`crates/net/p2p/src/lib.rs:1252-1262`](https://github.com/lambdaclass/ethlambda/blob/c3f5f566cb3a61e3d388c66ce21794c18a6618ce/crates/net/p2p/src/lib.rs#L1252-L1262)).
The XMSS prover is a hand-built single-thread job queue that catches panics
([`crates/common/crypto/src/lib.rs:105-125`](https://github.com/lambdaclass/ethlambda/blob/c3f5f566cb3a61e3d388c66ce21794c18a6618ce/crates/common/crypto/src/lib.rs#L105-L125)).
Ethlambda also uses rayon directly (19 lines name it).

### What it means for Spawned

Spawned already has `ctx.spawn_blocking_helper` (owned and joined on stop; see
the [guide](../GUIDE.md#owned-helpers-and-registrations)) and `Backend::Thread`
/ native groups for actors that are CPU-heavy as a whole. Neither is a compute
pool: a blocking helper uses Tokio's blocking pool (hundreds of threads, no CPU
bound, no admission), and its result comes back through a join handle the
handler must await, not through the mailbox.

**Proposed rank 11b: managed compute pool.** A sketch of the contract, to be
designed with rank 11 because it is pipe-to-self with a pool behind it:

- **Owned and bounded.** One CPU pool per `ActorSystem` (or per execution group),
  with an explicit thread budget defaulting to the core count, created and torn
  down with the system like other groups. It never runs on Tokio workers.
- **Result as a message.** `ctx.compute(job, |result| Msg)` (name illustrative)
  runs `job` on the pool and delivers `Msg` to the submitting actor; the handler
  returns immediately. Actor state is never touched from the pool.
- **Bounded admission.** A per-actor and per-pool cap on queued plus running
  jobs; on overflow the call returns an error at once (ethlambda's
  `Ignore(Overloaded)`), or waits under rank 17's admission rules. No silent
  unbounded queue.
- **Cancellation tied to the owner.** Jobs carry a token cancelled when the
  owner stops, restarts or the scope closes (rank 15a); queued jobs are dropped,
  running jobs see the token at their own check points. A result from a previous
  incarnation or a cancel-previous generation is never delivered (rank 15's
  generation fencing; ethlambda's session id).
- **Panic becomes a message.** A panicking job is caught and delivered as an
  error to the owner, exactly once, never as a missing reply (ethlambda's
  `guarded`).
- **Metrics.** Queue depth, wait time, run time and rejections per pool and per
  actor type, under the bounded-cardinality rules of rank 29.
- **Rules from the incidents.** Jobs must not block on other jobs of the same
  pool or on actors; a job that needs N co-resident threads (ethrex's merkle
  gang) needs a dedicated pool of at least N and must be admitted one at a time.
  Document this; detect the simple case (a job waiting on its own pool) in debug
  builds if cheap.
- **Backing.** Rayon behind an optional feature is the obvious first
  implementation (`ThreadPool::spawn` plus the completion hook), since both
  clients already link it; a fixed native pool like `thread_group` is the
  Tokio- and rayon-free alternative. Data-parallel code inside a job may keep
  using `par_iter` on the same pool.

Relation to other ranks: rank 11 supplies exactly-one completion and the
owner-exit outcome; rank 15 supplies ownership and fencing; rank 34 (blocking
worker leasing) is the I/O-blocking counterpart and should share the
admission design; rank 39 (CPU service classes) would later weight several
compute pools. Under the [verification rule](../../ROADMAP.md#verification-rule)
the model must show: every admitted job yields exactly one completion or a
cancellation outcome, including on panic and owner exit; no completion reaches a
restarted instance; the pool's in-flight count never exceeds its budget.

## 2. Structured concurrency and cancel scopes

### What it is

Concurrent work is started inside a lexical scope and cannot outlive it: the
scope waits for its children, a failure cancels siblings, and cancellation and
deadlines are attached to scopes rather than to individual tasks.

### How it works

- **Trio** (Python) introduced *nurseries*: "the block does not exit until
  *all* tasks have completed"; an unhandled child exception makes "the nursery
  immediately [cancel] all the tasks inside". *Cancel scopes* carry a deadline
  ("an absolute time ... at which this scope will automatically become
  cancelled"), nest, can be *shielded* from outer cancellation for cleanup, and
  deliver cancellation "by raising Cancelled at its next checkpoint"
  ([Trio reference](https://trio.readthedocs.io/en/stable/reference-core.html)).
  `move_on_after` and `fail_after` are deadline scopes.
- **Swift** task groups (`withTaskGroup`, `withThrowingTaskGroup`) bind child
  tasks to the closure; the group awaits them; `cancelAll()` marks children
  cancelled; cancellation is cooperative (`Task.isCancelled`,
  `Task.checkCancellation()`) and a cancelled parent cancels its children
  ([Swift concurrency](https://docs.swift.org/swift-book/documentation/the-swift-programming-language/concurrency/),
  [TaskGroup](https://developer.apple.com/documentation/swift/taskgroup)).
  *Unverified wording:* both pages render client-side and could not be quoted;
  the behaviour stated is the documented model as we understand it.
- **Kotlin** coroutines "form a tree hierarchy of parent and child tasks with
  linked lifecycles ... A parent coroutine waits for its children to complete
  before it finishes. If the parent coroutine fails or gets canceled, all its
  child coroutines are recursively canceled too"; `coroutineScope()` "waits
  until the block and any coroutines launched in it complete"
  ([Kotlin coroutines basics](https://kotlinlang.org/docs/coroutines-basics.html)).
- **Java** `StructuredTaskScope` is still a preview API: JEP 453 (JDK 21), JEP
  462 (JDK 22), JEP 480 (JDK 23), JEP 499 (JDK 24), JEP 505 (JDK 25, factory
  methods replace public constructors), JEP 525 (JDK 26, sixth
  preview, adds a timeout callback on `Joiner`) and JEP 533 (JDK 27, seventh
  preview). Finalization is proposed as JEP 543 for JDK 28
  ([JEP 525](https://openjdk.org/jeps/525),
  [InfoQ on JEP 533](https://www.infoq.com/news/2026/05/jep-533-jdk-27/),
  [InfoQ on JEP 543](https://www.infoq.com/news/2026/10/jep-543-jdk-28/)).
  The JEP 533 and 543 statuses come from InfoQ reporting; we did not read the
  JEP pages themselves.
- **Rust.** No language-level scope for async tasks.
  [`async-scoped`](https://docs.rs/async-scoped/latest/async_scoped/) offers a
  safe `scope_and_block` that blocks the thread and an `unsafe`
  `scope_and_collect`, because `mem::forget` of the scope future is safe Rust and
  would let borrowed tasks outlive their data.
  [`moro`](https://github.com/nikomatsakis/moro) is "experiments with
  structured concurrency"; its tasks "only advance when they are polled
  (they're not parallel)". Tokio's `JoinSet` aborts its tasks on drop and keeps
  results; tokio-util's
  [`TaskTracker`](https://docs.rs/tokio-util/latest/tokio_util/task/task_tracker/struct.TaskTracker.html)
  keeps no results, "dropping a TaskTracker does not abort the tasks", and
  `wait` returns only once it is closed and empty. Together with
  `CancellationToken` child tokens, that is Rust's practical nursery:
  cancel, close, wait.

### Who uses it

Kotlin's and Swift's standard concurrency libraries are built on it; Trio and
AnyIO in Python. We did not verify named production users for this note.

### What it means for Spawned

Actors are not lexically scoped, so a public "nursery around a block" API does
not fit handlers that must return to the mailbox. But an actor *is* a scope:
everything it starts (helpers, timers, compute jobs, deferred replies,
outbound requests) should be owned by it and end with it. Spawned already
implements half of this (owned helpers are cancelled and joined before cleanup
completes; supervisors stop children in reverse order).

**Proposed rank 15a: cancel scopes as the shared design for ranks 13, 15 and
19.** One ownership tree, `ActorSystem` ⊃ group ⊃ supervisor ⊃ actor ⊃ turn or
request, where each node is a scope with:

- **close**: stop admitting new children (rank 13's "close admission"; rank 15's
  "gate new helper registration");
- **cancel**: signal children cooperatively, with shielding for cleanup work
  (Trio's shield) so `stopped()` can still flush;
- **join with deadline**: wait for children up to the scope's deadline, then
  report what did not finish (rank 13's "report unfinished work", rank 15's
  join-with-deadline);
- **deadline inheritance**: a request scope's deadline flows into outbound
  requests and compute jobs started in that turn, and is checked before
  dispatch (rank 19's propagated budgets and stale-work skipping). A child
  deadline is never later than its parent's.

Rank 13 is then "close the actor scope, drain the mailbox under the scope's
deadline"; rank 15 is "helpers are children of the actor scope, cancel-previous
is closing a sub-scope"; rank 19 is "a request carries its scope's deadline".
One TLA+ model of scope close/cancel/join with deadlines can cover all three
instead of three models. Rust-specific constraints: scopes own `'static`
tasks (no borrowing across the scope, avoiding `async-scoped`'s soundness
problem), and closing is an explicit async step, because `Drop` cannot await.
Model obligations: no child is admitted after close; every child ends or is
reported unfinished by the deadline; a shielded cleanup cannot extend past the
parent's hard deadline.

## 3. Preemption, reduction counting and cooperative budgets

### What it is

Fairness between concurrent activities when one of them runs long. Preemptive
systems interrupt it; cooperative ones rely on it yielding.

### How it works

- **BEAM reductions.** The VM counts *reductions* (roughly function calls, with
  built-in functions charging in proportion to their work) and switches
  processes when a process uses its budget: `CONTEXT_REDS` is 4000, "Swap
  process out after this number"
  ([`erts/emulator/beam/erl_vm.h`](https://github.com/erlang/otp/blob/master/erts/emulator/beam/erl_vm.h)).
  This works because every Erlang instruction runs under the VM. Native code
  (NIFs) does not: it should return "within 1 millisecond", call
  `enif_consume_timeslice()` to report progress and yield by rescheduling itself
  (`enif_schedule_nif`, "always preferred"), or run on dirty schedulers
  ([erl_nif](https://www.erlang.org/doc/apps/erts/erl_nif.html)). The
  per-function-call accounting is described in
  [The BEAM Book](https://github.com/happi/theBeamBook) (*unverified against
  the emulator source beyond `CONTEXT_REDS`*).
- **Go** made goroutines "asynchronously preemptible" in 1.14, so "loops
  without function calls no longer potentially deadlock the scheduler", using
  signals (hence more `EINTR`)
  ([Go 1.14 release notes](https://go.dev/doc/go1.14)). This needs the runtime
  to find safe points in compiled code; Rust has no such runtime.
- **Tokio cooperative budgeting.** Each task gets a budget per poll: `Budget(Some(128))`,
  "chosen somewhat arbitrarily ... high enough to amortize wakeup and scheduling
  costs, but low enough that we do not starve other tasks"
  ([`tokio/src/task/coop/mod.rs`](https://github.com/tokio-rs/tokio/blob/master/tokio/src/task/coop/mod.rs)).
  Tokio resources such as `mpsc` receive call `poll_proceed` and return
  `Pending` when it is spent
  ([`sync/mpsc/chan.rs`](https://github.com/tokio-rs/tokio/blob/master/tokio/src/sync/mpsc/chan.rs)).
  `tokio::task::coop` exposes `consume_budget`, `has_budget_remaining`,
  `cooperative` and `unconstrained` ("will never be forced to yield")
  ([docs.rs](https://docs.rs/tokio/latest/tokio/task/coop/index.html)).
  Code that touches no Tokio resource never yields.

### Who uses it

Every Erlang/Elixir system relies on reductions; every Go program on async
preemption since 1.14; every Tokio application on coop budgets. In Spawned
itself, the async mailbox is a Tokio unbounded channel, so a long burst of
messages already yields after Tokio's budget, and a private time-slice
experiment (yield after N messages or T µs) exists behind
`backend-experiments` (`MessageTurn` in `concurrency/src/tasks/actor.rs`).

### What it means for Spawned

Erlang can preempt because the VM counts reductions at every call and switches
processes at a budget. Native Rust has no VM, so arbitrary code cannot be safely
preempted. The BEAM's own answer for native code (NIF guidance above) maps onto
three layers Spawned can provide.

**(A) Fairness between messages, in the framework. Cheap.** The dispatch loop
yields after N messages or T µs of handler time per poll (the per-slice budget
analog, today's `MessageTurn` experiment made a supported opt-in policy),
consumes Tokio's coop budget on its own operations (send, request, dispatch)
so framework work counts like any Tokio resource, and records per-actor work
metrics: poll time per message, polls per turn, turns over a threshold. This is
the input rank 8 needs and that ranks 29 (cohort metrics) and 39 (CPU service
classes) would aggregate. Elapsed time includes awaits, so it is "time holding a
worker per poll", not CPU accounting (rank 39's caveat).

**(B) Opt-in budgets inside long async handlers.** `ctx.checkpoint().await`
(name illustrative) yields only when the current turn has exceeded its budget,
using a cheap clock or counter check, like `enif_consume_timeslice`; a budgeted
iterator adapter does the same per item. Optionally a lint or macro hint for
loops in handlers with no await. Not automatic instrumentation.

**(C) Synchronous code cannot be preempted: detect and isolate.** The stall
watchdog (rank 8) names the actor and message when one poll blocks a worker.
Placement on `Backend::Thread` or a native group gives OS preemption for actors
known to be heavy (what ethlambda does for its chain actor,
[`lib.rs:433-434`](https://github.com/lambdaclass/ethlambda/blob/c3f5f566cb3a61e3d388c66ce21794c18a6618ce/crates/blockchain/src/lib.rs#L433-L434)).
The managed compute pool (rank 11b) offloads CPU work with pipe-to-self, as
dirty schedulers do for NIFs.

**Rejected:** auto-instrumenting every function or loop with proc macros (cost
on every call, partial coverage since dependencies are not instrumented), and
signal-based preemption of Rust threads (not async-signal-safe; would need
`unsafe` and breaks `forbid(unsafe_code)`).

**Acceptance tests:** on a single CPU, a busy actor sending itself a stream of
messages cannot delay another actor's message beyond a stated bound under (A);
the watchdog fires, naming actor and message, on a handler that blocks a worker;
`checkpoint()` in a long loop lets a second actor progress.

**Incident mapping:** d958dcba4 (synchronous cell serving filled all workers) is
(C): detect with rank 8, fix with rank 11b. Ethlambda's `Backend::Thread` chain
actor is (C) placement. Fairness metrics feed ranks 29 and 39. Proposed as
**rank 8a**, next to the stall watchdog; it changes scheduling between turns,
not lifecycle, replies or ownership, so tests suffice.

## 4. Streams with backpressure

**What and how.** Reactive Streams is "a standard for asynchronous stream
processing with non-blocking backpressure": a subscriber "MUST signal demand
via `Subscription.request(long n)`" and a publisher never sends more than was
requested ([reactive-streams-jvm](https://github.com/reactive-streams/reactive-streams-jvm)).
Akka Streams builds typed, materialized graphs on that protocol
([Akka Streams](https://doc.akka.io/libraries/akka-core/current/stream/index.html)).
In Rust, `futures::Stream` is pull-based, so demand is implicit in polling;
`tokio-stream` adapts Tokio channels to it.

**Who uses it.** Akka, Project Reactor and RxJava implement the spec (from the
spec's README; not independently verified).

**For Spawned (rank 33).** Rank 33 is already the place. Two notes: request(n)
demand is the same thing as rank 18's credits, so build streams on credits, not
a second mechanism; and `spawn_listener` should pull with bounded demand rather
than drain an unbounded channel, reporting lag instead of dropping it (the
client sweep's `Lagged` drops).

## 5. Software transactional memory

**What and how.** Shared memory accessed through transactions that commit
atomically or retry. Haskell's STM composes transactions with `retry` and
`orElse` ([Harris, Marlow, Peyton Jones, Herlihy, *Composable memory
transactions*, PPoPP 2005](https://www.microsoft.com/en-us/research/publication/composable-memory-transactions/));
Clojure's refs use MVCC with `dosync`, `alter` and `commute`
([Clojure refs](https://clojure.org/reference/refs)).

**Who uses it.** GHC and Clojure ship it; we did not verify production users.

**For Spawned.** Actor state is single-writer, so STM's main job is already
done. The overlap is rank 11a's transactional turns: holding a turn's sends
until it returns and discarding them on panic is STM's "effects only on commit",
restricted to one actor. Two STM ideas are worth recording in the 11a design:
`retry` (block a turn until the state it read changes) is selective receive in
disguise and should stay out; and the "no I/O inside a transaction" rule is why
external effects can never be rolled back. Design note only; no rank.

## 6. Thread-per-core and io_uring

**What and how.** Seastar "runs one application thread per core, and depends on
explicit message passing, not shared memory between threads"
([Seastar shared-nothing](https://seastar.io/shared-nothing/)). Rust
equivalents: [Glommio](https://github.com/DataDog/glommio) (io_uring,
thread-per-core), [monoio](https://docs.rs/monoio/latest/monoio/) (io_uring or
epoll/kqueue, owned-buffer I/O) and
[tokio-uring](https://github.com/tokio-rs/tokio-uring) (io_uring under a
Tokio-compatible runtime; "still very young", needs a recent kernel).
Futures are `!Send` and pinned to a core; completion-based I/O needs owned
buffers, which changes cancellation.

**Who uses it.** ScyllaDB is built on Seastar (the Seastar site is ScyllaDB's;
the page itself does not name users).

**For Spawned.** Parked pinned shards already give per-core placement without
changing the I/O layer. Earlier experiments measured Monoio epoll and io_uring
against Tokio: io_uring improved the measured tail on 128 loopback sockets but
did not lead throughput, and the result "does not measure the isolated effect
of io_uring or justify replacing Tokio"
([design findings](../../legacy/benchmarks/DESIGN-FINDINGS.md#what-we-learned-in-the-linux-vm),
[experiments](../../legacy/benchmarks/EXPERIMENTS.md),
[scheduler research](../../legacy/research/backend-experiments/2026-11-06-fede/SCHEDULER-RESEARCH.md)).
Both clients use Tokio I/O throughout. **Research pointer 52, frozen** until a
client I/O profile shows syscall or reactor cost dominating, per the roadmap's
backend freeze.

## 7. Incremental dataflow

**What and how.** Timely dataflow runs cyclic dataflow graphs with logical
timestamps (Naiad, [Murray et al., SOSP 2013](https://www.microsoft.com/en-us/research/publication/naiad-a-timely-dataflow-system-2/));
differential dataflow, built on it, "is a data-parallel programming framework
designed to efficiently process large volumes of data and to quickly respond to
arbitrary changes in input collections"
([differential-dataflow](https://github.com/TimelyDataflow/differential-dataflow)).

**Who uses it.** Materialize, which compiles SQL views into differential
dataflows on Timely ([Materialize architecture](https://materialize.com/blog/materialize-architecture),
[about](https://materialize.com/about/)).

**For Spawned.** A different programming model: operators over collections, not
addressed actors. The only overlap is progress tracking (frontiers), which is a
general form of rank 33's completion watermarks. **Research pointer 53**; read
before designing watermarks, do not build.

## 8. CRDTs, sagas and process managers

**What and how.** Conflict-free replicated data types merge concurrent updates
without coordination and converge
([Shapiro, Preguiça, Baquero, Zawirski, *Conflict-free replicated data types*,
SSS 2011](https://inria.hal.science/hal-00932836)). A saga splits a long
transaction into steps with compensating actions
([Garcia-Molina and Salem, *Sagas*, SIGMOD 1987](https://dl.acm.org/doi/10.1145/38713.38742));
a process manager is an actor that holds a saga's state and drives its steps.

**Who uses it.** Akka Distributed Data provides CRDTs
([docs](https://doc.akka.io/libraries/akka-core/current/typed/distributed-data.html));
Temporal and Restate run sagas as durable workflows. Not verified further.

**For Spawned.** CRDTs matter only with replicas on several nodes (rank 44).
A process manager is an ordinary actor today; making it survive a crash is
durable execution (ranks 49-51, Restate as the reference design in
[virtual actors](virtual-actors.md#what-it-means-for-spawned)). **Research
pointer 54**, in the distribution/durability tier; a saga example could later
show compensation with supervision and timeouts.

## 9. Briefly: CSP select, algebraic effects, join calculus

**CSP channels and `select` (Go).** Goroutines communicate over channels and
`select` waits on several at once
([Go spec: select](https://go.dev/ref/spec#Select_statements)). Spawned's
actor is a single mailbox with typed handlers; `tokio::select!` remains available
inside a helper. The useful lesson is negative: `select` over many channels has
no ownership, which is what rank 15a adds.

**Algebraic effects (OCaml 5, Eio).** OCaml 5 effect handlers let a library
implement direct-style concurrency without coloured functions; Eio uses them
and adds structured concurrency with switches that own fibers and resources
([Eio](https://github.com/ocaml-multicore/eio)). Rust has no effects; the
relevant borrow is Eio's *switch*, which is the cancel scope of rank 15a.

**Join calculus (rank 48).** Join patterns fire a body when a set of messages
on several channels is present
([Fournet and Gonthier, POPL 1996](https://dl.acm.org/doi/10.1145/237721.237805)).
Rank 48 already bounds unmatched messages and asks for fairness; no change.

## Sources

Primary documentation and papers:

- [Ryhl, *Async: what is blocking?*](https://ryhl.io/blog/async-what-is-blocking/)
- [Tokio `spawn_blocking`](https://docs.rs/tokio/latest/tokio/task/fn.spawn_blocking.html), [`tokio::task::coop`](https://docs.rs/tokio/latest/tokio/task/coop/index.html), [coop source](https://github.com/tokio-rs/tokio/blob/master/tokio/src/task/coop/mod.rs), [mpsc source](https://github.com/tokio-rs/tokio/blob/master/tokio/src/sync/mpsc/chan.rs)
- [tokio-util `TaskTracker`](https://docs.rs/tokio-util/latest/tokio_util/task/task_tracker/struct.TaskTracker.html), [async-scoped](https://docs.rs/async-scoped/latest/async_scoped/), [moro](https://github.com/nikomatsakis/moro)
- [Akka dispatchers](https://doc.akka.io/libraries/akka-core/current/typed/dispatchers.html), [Pekko pipe-to-self](https://pekko.apache.org/docs/pekko/current/typed/interaction-patterns.html#send-future-result-to-self), [Akka Streams](https://doc.akka.io/libraries/akka-core/current/stream/index.html), [Akka Distributed Data](https://doc.akka.io/libraries/akka-core/current/typed/distributed-data.html)
- [Orleans: external tasks and grains](https://learn.microsoft.com/en-us/dotnet/orleans/grains/external-tasks-and-grains)
- [Ray actors](https://docs.ray.io/en/latest/ray-core/actors.html)
- [erl_nif](https://www.erlang.org/doc/apps/erts/erl_nif.html), [`erl_vm.h`](https://github.com/erlang/otp/blob/master/erts/emulator/beam/erl_vm.h), [The BEAM Book](https://github.com/happi/theBeamBook)
- [Go 1.14 release notes](https://go.dev/doc/go1.14), [Go spec: select](https://go.dev/ref/spec#Select_statements)
- [Trio core reference](https://trio.readthedocs.io/en/stable/reference-core.html), [Swift concurrency](https://docs.swift.org/swift-book/documentation/the-swift-programming-language/concurrency/), [Swift TaskGroup](https://developer.apple.com/documentation/swift/taskgroup), [Kotlin coroutines basics](https://kotlinlang.org/docs/coroutines-basics.html)
- [JEP 525](https://openjdk.org/jeps/525), [InfoQ: JEP 533 in JDK 27](https://www.infoq.com/news/2026/05/jep-533-jdk-27/), [InfoQ: JEP 543 in JDK 28](https://www.infoq.com/news/2026/10/jep-543-jdk-28/)
- [Reactive Streams JVM](https://github.com/reactive-streams/reactive-streams-jvm)
- [Composable memory transactions (PPoPP 2005)](https://www.microsoft.com/en-us/research/publication/composable-memory-transactions/), [Clojure refs](https://clojure.org/reference/refs)
- [Seastar shared-nothing](https://seastar.io/shared-nothing/), [Glommio](https://github.com/DataDog/glommio), [monoio](https://docs.rs/monoio/latest/monoio/), [tokio-uring](https://github.com/tokio-rs/tokio-uring)
- [Naiad (SOSP 2013)](https://www.microsoft.com/en-us/research/publication/naiad-a-timely-dataflow-system-2/), [differential-dataflow](https://github.com/TimelyDataflow/differential-dataflow), [Materialize architecture](https://materialize.com/blog/materialize-architecture), [Materialize about](https://materialize.com/about/)
- [CRDTs (SSS 2011)](https://inria.hal.science/hal-00932836), [Sagas (SIGMOD 1987)](https://dl.acm.org/doi/10.1145/38713.38742)
- [Eio](https://github.com/ocaml-multicore/eio), [Join calculus (POPL 1996)](https://dl.acm.org/doi/10.1145/237721.237805)

Client sources: ethrex commits
[d958dcba4](https://github.com/lambdaclass/ethrex/commit/d958dcba4) and
[535647e45](https://github.com/lambdaclass/ethrex/commit/535647e45), and the
file links above at ethrex `1dfaa09ec` and ethlambda `c3f5f566`. Earlier
Spawned I/O-engine experiments:
[design findings](../../legacy/benchmarks/DESIGN-FINDINGS.md),
[experiments](../../legacy/benchmarks/EXPERIMENTS.md).
