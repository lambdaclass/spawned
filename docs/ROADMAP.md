# Spawned roadmap

Updated 2026-11-08. This is the implementation order, not a release schedule.

Business handlers stay ordinary Rust. The framework owns dispatch and lifecycle.
Keep typed messages/replies, dynamic actors, addressed delivery, cooperative
handlers and the existing default behavior. New policies are explicit and opt-in.

## Available in v0.6.0

Typed protocols, async/blocking/thread execution, actor identities, monitors,
links and trapped exits, linked startup, shutdown fixes, optional framework
metrics, and reproducible backend research. v0.6.0 does not include automatic
supervision. Thread actors need no active Tokio executor but still compile Tokio.

## 1. Execution groups plus supervision

The parked async + OneForOne MVP is implemented on main and unreleased.
See the [usage guide](EXECUTION-GROUPS.md) and the
[MVP design and acceptance criteria](design/execution-groups-supervision.md).

### 1a. ActorSystem and execution groups — implemented

- Named parked async pools with explicit worker budgets and `start_on(&group)`.
  Ordinary `.start()` remains ambient and unmanaged.
- Ownership of admitted actors, registered async/blocking helpers and runtime
  teardown. Managed timers/listeners/monitors use the assigned group.
- Readiness, closed admission, reverse shutdown and incomplete reports that
  retain actual cleanup without overlapping replacement generations.

Still to add: dedicated/pinned groups, sized blocking-actor pools and additional
platform/resource contracts. Spinning remains experimental. Worker budgets count
async workers, not all OS threads or reserved CPU cores.

### 1b. Automatic supervision — implemented OneForOne

- Fresh-instance ChildSpec factories with fallible startup, readiness and retained
  group placement; Permanent, Transient and Temporary restart modes.
- Shared rolling retry limits, per-child capped backoff/jitter, ordered startup
  and reverse shutdown. Lifecycle observation bypasses the user-message backlog.
- Instance-bound references and explicit replacement lookup. Queued work is
  discarded; no replay, automatic durable state recovery or exactly-once effects.
- Runnable examples and failure tests for startup/cleanup deadlines, constructor
  cancellation, restart storms and concurrent shutdown. Blocking code remains
  cooperative; helper admission closes at shutdown start, with no drain policy.

Still to add: nested supervisor child specs, OneForAll and RestForOne after the
OneForOne lifecycle is verified (milestone 2) and proven in applications.

Full Ethrex/Ethlambda replay or a same-node production canary remains a validation
gate. Existing local EngineClient probes and synthetic isolation/pipeline studies
are different evidence levels; do not transfer their ratios to a production client.
See [backend usage](BACKENDS.md), the [runtime-isolation study](../research/backend-experiments/2026-11-06-runtime-isolation/README.md)
and the [implementation review](reviews/execution-groups-supervision-implementation.md).

## 2. Verify what landed, before building on it

Supervision shipped before the lifecycle model meant to precede it. Before
OneForAll/RestForOne and before a release that includes supervision:

- a TLA+ (or P/Stateright) model of the lifecycle and the implemented OneForOne
  contract, with trace validation against the existing scenario, churn and
  supervision failure tests;
- Miri and ThreadSanitizer in CI;
- a `cargo-mutants` pass over supervision, system and lifecycle code.

Start with a one-day search of the pinned client revisions for hand-built pending
queues, per-key actor maps, requests awaited in handlers and broadcast lists; it
settles where several backlog items belong. See the
[verification strategy](research/verification-strategy.md).

## 3. Release and run in a client

Release v0.7 with execution groups and supervision and run it in Ethlambda. In
parallel, a small developer-experience track: a test kit with `ActorRef::mock()`,
request deadlock detection and a stall watchdog.

## Then, guided by application needs

- Deferred typed replies, delegation and pipe-to-self first: distinguish reply
  ownership from dispatch, bound pending work and define late/owner-exit results.
  Bounded stashing follows as a reply-ownership variant.
- Bounded admission, pre-allocation byte credits and opt-in inherited request
  deadlines as one design package. Credits work independently of optional metrics.
- Actor-level ordering exploration, then per-actor crash tapes, then
  record/replay.
- Typed streams, causal tracing, cohort metrics, runtime inspection, batching,
  coalescing and targeted dispatch optimizations.
- When a client or user justifies them: OneForAll/RestForOne, a lighter
  dedicated-thread backend, virtual actors, process groups/pub-sub, and
  Tokio-free thread builds.

**Tokio-free builds versus the clients.** A Tokio-free *build* lets thread-only
applications avoid compiling Tokio; it does not help Ethrex or Ethlambda, which
depend on Tokio throughout. The client-relevant item is a lighter dedicated-thread
backend: task-mode `Backend::Thread` creates a Tokio runtime per actor (capped to
one helper worker), and Ethlambda's `BlockChainServer` uses it. See
[Silver/flux](research/silver-flux.md) for the Tokio-free dependency boundaries;
design public ownership types so Tokio-specific handles stay behind the async
feature.

Start client profiling in parallel from the first milestone, alongside failure
testing, simulations and stress campaigns. A representative workload must establish
the performance case before promoting a policy. The
[local Engine-client probes](../research/client-observations/2026-11-06-ethlambda-ab/server-probe/README.md)
are useful scheduling evidence, not a full-client performance result. Improve
existing backends before adding names; backend research stays frozen until a
client profile supplies a concrete reason.

[Idea priorities](research/idea-priorities.md) is the single ranked backlog with
the detail and tradeoffs behind this order. Other research pages
([framework ideas](research/framework-ideas.md),
[verification strategy](research/verification-strategy.md),
[Commonware](research/commonware.md), [concurrency ideas](research/concurrency-ideas.md),
[Silver/flux](research/silver-flux.md)) and the
[detailed acceptance criteria](design/roadmap-acceptance-2026-11-06.md) are
reference material without rankings. No speculative feature is promised by a date.
