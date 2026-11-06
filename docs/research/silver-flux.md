# Research: Silver / flux and what Spawned can adopt

Date: 2026-11-05. Status: source analysis and proposed experiments, not a benchmark
report or a commitment to replace Spawned's runtime. See the [roadmap](../ROADMAP.md).

## Revisions and scope

| Project | Inspected revision |
|---|---|
| Spawned | `08a84f12bd03c5f307baa24dfc4ed88566e01576`, including PR #176 |
| Silver | `7360bacb98026c99508f274098d438d9b5acec43` |
| flux | `004884d1e82bb6afb1df81fa70a4c220a10368a0`, pinned by Silver |

The observations below come from the linked source files. Recommendations and
performance hypotheses are identified separately. No Silver/Spawned performance
comparison has been run, and feature presence does not establish relative speed.
Silver's [manifest][silver-manifest] confirms the flux pin.

## Different execution contracts

Spawned is an actor framework: dynamic instances, addressed mailboxes, ordinary
Rust message types, typed request/reply, and framework-managed dispatch. Silver
is an Ethereum consensus client using a mostly static tile-and-spine topology.
Its [glossary][silver-context] deliberately distinguishes tiles from actors.

| Aspect | Spawned at this revision | Silver / pinned flux |
|---|---|---|
| Concurrency unit | Actor owning state and handlers | Tile owning a loop on a configured thread |
| Scheduling | Tokio task, blocking-pool task, dedicated task thread, or synchronous actor thread | Silver attaches six tiles on cores 1–6 using high-priority configuration |
| Delivery | One recipient's mailbox; typed reply receiver for requests | Broadcast queues consumed by subscribed tiles, with filtering and application-level request correlation |
| Transport | Boxed envelopes over unbounded mpsc mailboxes | Fixed-size rings using sequence locks and Copy message records |
| Bulk payloads | Owned values or user-chosen shared ownership inside messages | Silver TCache references carry bulk data outside queue records |
| Overload | Mailbox can accumulate memory without a capacity limit | Ring overwrite can lap consumers and report `SpedPast` |
| Lifecycle | `started`, handlers, `stopped`, exit reasons, monitors | `on_attach`, repeated `try_init`, `loop_body`, `teardown`, shared stop flag |
| Diagnostics | Tracing and lifecycle information exist; comprehensive runtime metrics remain proposed | Transport timestamps, tile metrics and external inspection/profiling facilities |

Sources: [Spawned task implementation][spawn-tasks], [thread implementation][spawn-threads],
[Silver topology][silver-main], [message flow][silver-flow], [queue declarations][silver-spine],
[flux queues][flux-queues], [tile lifecycle][flux-tile], and [spine transport][flux-spine].

Silver declares `gossip_in` with 2^19 slots and `rpc_inbound` with 2^14 slots.
These are configuration choices in its spine, not universal flux capacities.
Likewise, six tiles is Silver's topology, not a framework-wide maximum. Flux
supports generic attachment, background configurations, and optional parking.
A tile's subscriptions determine which queues it consumes; every tile does not
read every queue. Multiple application producers can publish a given message
kind, as the Silver flow document shows.

Silver's release profile uses `panic = "abort"`; a panic there terminates the
process. Spawned's panic recovery depends on unwinding and cannot override an
application built with abort-on-panic. Neither design implies durable delivery.

## What transfers well

### 1. Timing at framework boundaries

Flux attaches ingestion/publish timing in its transport and records consumption
and processing information. Silver enables per-producer/consumer timers in its
spine declaration. This is a useful precedent for collecting data below business
handlers. [Transport][flux-spine], [consumer timers][flux-consumer].

For Spawned, propose enqueue timestamps, dispatch timestamps, handler completion,
request completion, and lifecycle counters. Define the measurement precisely:
handler wall time includes awaited I/O, and queue wait includes scheduling delay.
Ingress-to-result timing needs propagated context; a fresh enqueue timestamp
cannot reconstruct the original ingress time.

Clock reads, larger envelopes, atomics, histogram updates, and exporters have
costs. Compare instrumentation disabled, enabled, and sampled. An exact oldest
queued-message age may need extra bookkeeping beyond a depth counter; do not
promise it without measuring that cost. Limit labels and exporter buffers.

### 2. External inspection without application instrumentation

Silver's flow documentation identifies external consumers; flux provides
shared-memory tile metrics and a cross-process profiler. The profiler's function
traces use explicit annotations, so it is incorrect to say all observability is
obtained with no instrumentation. [Message flow][silver-flow], [tile metrics][flux-tile],
[profiler documentation][flux-profiler].

For Spawned, start with an exporter-independent metrics interface and one
optional exporter. Separate data capture from presentation. Investigate external
snapshots/shared memory only if a client needs diagnostics while its main loop
is stalled. That would require versioned layouts, snapshot consistency,
permissions, stale-process cleanup, and a bounded loss policy for telemetry.
Those are additional requirements, not automatic benefits of mmap.

### 3. Explicit resource budgets

Preallocated queues make capacity and loss visible. Spawned can adopt explicit
capacity planning without adopting overwrite semantics: measure queue growth,
then evaluate bounded admission, rejection, or waiting. Bound pending producers
and payload bytes where needed; bounding message count alone is insufficient.

A bounded mailbox needs clear full/closed outcomes and tests for cancellation,
shutdown, self-sends, and cyclic actor dependencies. Silent overwrite must not
replace normal actor request delivery. A separate opt-in lossy telemetry stream
could have different semantics without changing actor mailboxes.

### 4. Optional locality controls

Affinity may benefit a small number of actors with stable hot working sets.
Prototype it as an opt-in placement policy for dedicated actors only after
measurements identify migration or locality as a cost. Compare equal core
budgets, NUMA placement, idle CPU, and oversubscription.

Busy polling is a separate experiment. Pinning a thread does not remove envelope
allocations, dispatch costs, I/O integration, or the OS scheduler. A pinned
`Backend::Thread` is not equivalent to Silver's end-to-end pipeline.

## What must not become Spawned's default

- Static topology replacing dynamic actor creation.
- Copy-only payload requirements replacing ordinary Rust message types.
- Broadcast replacing addressed delivery.
- Silent loss on mailbox overwrite, especially for requests.
- Application handlers taking ownership of poll loops, reply routing, or runtime
  infrastructure simply to make the backend faster.
- Mandatory dedicated cores or spinning for ordinary actors.

Broadcast in flux still entails consumer reads and filtering. Conversely,
Spawned fan-out can share bulk bytes through `Arc` or similar ownership rather
than deep-copying every payload, although separate envelopes still have costs.
Avoid categorical claims that either broadcast or copies are universally cheap.

Preallocation of transport does not prove that an entire client is allocation
free. Flux's consumer timing code, for example, creates timer structures; timing
and allocation claims must identify the measured path and steady-state conditions.
Busy polling also is not preemptive actor scheduling: a long callback can still
monopolize its tile.

## Should Spawned remove Tokio?

Separate three decisions rather than treating them as one migration.

### A. A Tokio-free thread-only build

Potentially useful if users require a smaller dependency graph or synchronous
integration with no Tokio dependency. Today thread actors do not require a Tokio
executor, but `spawned-rt` depends on Tokio and thread completion/monitoring use
runtime-independent Tokio synchronization primitives. Also, the task-mode
`Backend::Thread` runs its own Tokio runtime. [Runtime manifest][spawn-runtime],
[task backends][spawn-tasks], [completion][spawn-completion].

Possible work: feature-gate the async dependency graph, introduce runtime-neutral
completion/cancellation interfaces, and provide equivalent synchronous
implementations. Prove wakeup, cancellation, panic, and monitor behavior with the
existing regressions and focused models. Measure dependency/build-size benefits;
this separation alone does not imply lower message latency.

**Gate:** a concrete embedding or dependency requirement and a demonstrated
thread-only build whose public contracts and cleanup behavior remain intact.

### B. A specialized thread backend inspired by Silver

Potentially useful for a small, stable group of latency-critical actors. Explore
core affinity, explicit wait strategies, preallocated or pooled transport, and
I/O ownership under an opt-in backend configuration. Keep typed protocols and
ordinary message semantics; isolate any restrictions in the experimental backend.

One OS thread per actor does not scale like many lightweight tasks. Multiplexing
many actors onto a few threads introduces a scheduler again. Supporting arbitrary
async handlers without Tokio requires compatible I/O/timer facilities or a
restricted contract. Merely replacing task spawning with thread spawning does
not solve those problems.

**Gate:** repeatable improvements on a real workload at the same CPU budget,
without unacceptable idle CPU, memory growth, tail latency, or semantic changes.

### C. Replace Tokio for general async actors

Not currently justified by the inspected sources. This means owning an executor,
I/O reactor, timer driver, wakeups, fairness, cancellation, blocking integration,
platform support, and compatibility with libraries that expect Tokio. Tokio-free
thread-only builds and specialized backends can be pursued without that rewrite.

**Gate:** profile evidence that the runtime is the limiting cost, alternatives
that cannot meet the requirement, a written compatibility/migration plan, and
maintenance capacity. Keep the current backend as the comparison baseline.

## Proposed performance study

Do this before runtime replacement or transport redesign. It is proposed work;
this document contains no measured performance results.

1. **Baselines:** current task/thread modes, relevant task backends, and equivalent
   plain Tokio tasks/channels. Add a flux microbenchmark only for operations with
   matching semantics; don't rank a lossy broadcast against completed RPCs.
2. **Workloads:** many mostly idle actors; few saturated actors; multiple producers;
   mixed CPU and I/O; small and large payloads; fan-out with shared ownership;
   lifecycle churn; burst and sustained overload. Include one reproducible Ethrex
   or Ethlambda workload before drawing client-level conclusions.
3. **Measurements:** offered and completed rates, errors/timeouts, p50/p95/p99,
   queue wait versus handler time, allocations per operation, RSS over time,
   CPU utilization, and idle power/CPU cost where observable. Report warmup,
   duration, repeated runs, variance, machine, compiler, build flags, and commits.
4. **Load generation:** measure from scheduled arrival as well as actual enqueue
   under overload; don't let a closed-loop caller hide queue buildup. Bound the
   experiment's duration/memory and report aborted runs instead of extrapolating.
5. **Instrumentation study:** off/on/sampled overhead with identical inputs.
   Keep the benchmark driver independent enough to detect actor accounting bugs.
6. **Decision:** agree on a target and acceptable tradeoffs before optimizing.
   Choose admission control, allocation changes, batching, placement, or recovery
   work based on evidence. Re-run correctness tests for every candidate.

A baseline benchmark executable can collect caller latency, completion rate,
allocations, memory, and CPU before production metrics exist. Framework timing
then explains those results; the two efforts can proceed together.

## Recommendation

Implement optional observability and establish realistic baselines first. Use
those measurements to choose performance work. Develop supervision as the next
major recovery capability with explicit policies. Keep Tokio removal, busy
spinning, shared-memory transport, and specialized thread execution as distinct,
evidence-gated research projects. Preserve Spawned's protocol and actor model.

## Sources

All implementation links below are pinned to the revisions listed above.

- [silver-manifest][silver-manifest]
- [silver-context][silver-context]
- [silver-main][silver-main]
- [silver-flow][silver-flow]
- [silver-spine][silver-spine]
- [flux-queues][flux-queues]
- [flux-tile][flux-tile]
- [flux-spine][flux-spine]
- [flux-consumer][flux-consumer]
- [flux-profiler][flux-profiler]
- [spawn-tasks][spawn-tasks]
- [spawn-threads][spawn-threads]
- [spawn-runtime][spawn-runtime]
- [spawn-completion][spawn-completion]

[silver-manifest]: https://github.com/gattaca-com/silver/blob/7360bacb98026c99508f274098d438d9b5acec43/Cargo.toml
[silver-context]: https://github.com/gattaca-com/silver/blob/7360bacb98026c99508f274098d438d9b5acec43/CONTEXT.md
[silver-main]: https://github.com/gattaca-com/silver/blob/7360bacb98026c99508f274098d438d9b5acec43/crates/bin/src/main.rs
[silver-flow]: https://github.com/gattaca-com/silver/blob/7360bacb98026c99508f274098d438d9b5acec43/docs/spine-message-flow.md
[silver-spine]: https://github.com/gattaca-com/silver/blob/7360bacb98026c99508f274098d438d9b5acec43/crates/common/src/spine.rs
[flux-queues]: https://github.com/gattaca-com/flux/blob/004884d1e82bb6afb1df81fa70a4c220a10368a0/crates/flux-communication/src/queue/mod.rs
[flux-tile]: https://github.com/gattaca-com/flux/blob/004884d1e82bb6afb1df81fa70a4c220a10368a0/crates/flux/src/tile/mod.rs
[flux-spine]: https://github.com/gattaca-com/flux/blob/004884d1e82bb6afb1df81fa70a4c220a10368a0/crates/flux/src/spine/mod.rs
[flux-consumer]: https://github.com/gattaca-com/flux/blob/004884d1e82bb6afb1df81fa70a4c220a10368a0/crates/flux/src/spine/consumer.rs
[flux-profiler]: https://github.com/gattaca-com/flux/blob/004884d1e82bb6afb1df81fa70a4c220a10368a0/crates/flux-profiler/README.md
[spawn-tasks]: https://github.com/lambdaclass/spawned/blob/08a84f12bd03c5f307baa24dfc4ed88566e01576/concurrency/src/tasks/actor.rs
[spawn-threads]: https://github.com/lambdaclass/spawned/blob/08a84f12bd03c5f307baa24dfc4ed88566e01576/concurrency/src/threads/actor.rs
[spawn-runtime]: https://github.com/lambdaclass/spawned/blob/08a84f12bd03c5f307baa24dfc4ed88566e01576/rt/Cargo.toml
[spawn-completion]: https://github.com/lambdaclass/spawned/blob/08a84f12bd03c5f307baa24dfc4ed88566e01576/concurrency/src/child_handle.rs
