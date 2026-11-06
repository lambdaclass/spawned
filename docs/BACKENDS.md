# Choosing an execution backend

Use `Backend::Async` for ordinary async actors. It runs on the current Tokio
runtime and remains the supported default. Choose another execution mode for a
specific isolation or blocking requirement, then measure the application.

## Supported modes

| Mode | Appropriate work | Resource model |
|---|---|---|
| `tasks::Backend::Async` | Async I/O, many dynamic actors, general request/reply | Actors share Tokio workers; a running handler must cooperate |
| `tasks::Backend::Blocking` | Persistent actors that perform blocking calls | Each actor occupies a Tokio blocking worker until it stops; excess actors queue, including startup |
| `tasks::Backend::Thread` | Actors needing a dedicated execution thread | One OS thread and an isolated Tokio runtime per actor; account for helpers and descriptors |
| `threads` module | Synchronous actor handlers | One native OS thread per actor; synchronous API |

In tasks mode, existing actor/protocol definitions work with each supported
`Backend` value:

```rust
use spawned_concurrency::tasks::{ActorStart, Backend};

// MyActor is an application-defined actor.
let actor = MyActor::new().start();
let blocking = MyActor::new().start_with_backend(Backend::Blocking);
let isolated = MyActor::new().start_with_backend(Backend::Thread);
```

Stop and await actors when their work is finished. A dedicated thread does not
make one long-running handler preemptible, and a persistent blocking actor does
not release its worker between messages. Size persistent populations accordingly.
The [API guide](API-GUIDE.md#backend-selection-tasks-mode) covers these APIs.

## Experimental fixed-core profiles

The `backend-experiments` feature contains private research selectors, separate
from the supported enum. They are not stable application configuration APIs.
The [research study](../research/backend-experiments/2026-11-06-fede/README.md)
retains the complete graphs, raw results, source bundles, reviews and protocols.

The Silver-inspired profile assigns actors to process-owned current-thread Tokio
shards and pins each worker to a selected Linux CPU. One actor per core is a
particular deployment; multiple actors can share each worker. The worker count,
not the actor count, determines this profile's actor execution thread population.

| Profile | When to investigate | Tradeoff |
|---|---|---|
| Pinned, parked shards | Fixed streaming pipelines with known placement | Encouraging eight-core synthetic results; fixed placement can strand capacity or concentrate a hot stage |
| Pinned, spinning shards | A measured latency need with dedicated spare CPU capacity | Keeps assigned cores busy while idle; mixed benefit at smaller core counts |
| Root/Runnable/Deque schedulers | Targeted scheduler-cost research | More implementation and correctness obligations; no general replacement demonstrated |

The eight-core streaming fixture with one actor/core measured median throughput
of 1.01M roots/s for Tokio, 1.88M for pinned parked shards and 2.11M for spinning
shards, across three fresh processes each. Spinning consumed about eight busy-core
equivalents while idle. The separate many-actor recursive graph still favored
Tokio. These workloads differ; do not transfer the pipeline ratios to a client or
claim an isolated affinity effect. See [full findings and ranges](../research/backend-experiments/2026-11-06-fede/FINDINGS.md).

Use the [experimental reproduction guide](../research/backend-experiments/2026-11-06-fede/PINNED-PROFILES.md)
for exact feature/env settings, CPU audits and limitations. Set private options
before the first actor starts. The pinned profile requires Linux and taskset;
blocking helpers may inherit shard affinity and compete with spinning workers.
It retains Tokio, ordinary owned messages, addressed unbounded mailboxes,
request/reply and cooperative lifecycle behavior. It does not implement Flux's
broadcast rings or overwrite-on-lapping semantics.

## Actors on different runtimes

Actors hosted on separate Tokio runtimes can exchange ordinary messages through
`ActorRef` and use request/reply. The pinned pipeline already exercises this:
each shard has its own runtime, stages forward messages between them, and a
separate driver runtime receives the final replies.

A useful application experiment is a small fixed pipeline isolated from a general
async runtime. It can separate scheduling pressure and give the pipeline explicit
CPU placement. It also adds threads, inter-runtime wakeups and capacity that
cannot automatically rebalance between pools. Keep both runtimes alive and manage
actor stop/join before shutting them down. Tokio I/O resources and helpers still
have runtime ownership requirements; sending an `ActorRef` does not migrate those
resources or an existing actor.

The current private experiment selector is process-wide: it does not provide a
new per-actor public API for mixing pinned and default policies. A supported
mixed-runtime configuration needs explicit ownership/placement and application
measurements before API design. Communication compatibility alone is not a
reason to create more runtimes.
