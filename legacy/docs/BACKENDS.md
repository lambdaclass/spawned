# Choosing an execution backend

Use `Backend::Async` for ordinary async actors. It runs on the current Tokio
runtime and remains the supported default. Choose another execution mode for a
specific isolation or blocking requirement, then measure the application.

## Supported modes

| Mode | Appropriate work | Resource model |
|---|---|---|
| `tasks::Backend::Async` | Async I/O, many dynamic actors, general request/reply | Actors share Tokio workers; a running handler must cooperate |
| `tasks::Backend::Blocking` | Persistent actors that perform blocking calls | Each actor occupies a Tokio blocking worker until it stops; excess actors queue, including startup |
| `tasks::Backend::Thread` | Actors needing a dedicated execution thread | One dedicated root thread plus one isolated Tokio async worker per actor; blocking helpers can add threads |
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
The [API guide](../../docs/GUIDE.md#execution-modes-and-backends) covers these APIs.

`Backend::Thread` drives actor lifecycle hooks and handlers on its dedicated root
thread. Its isolated multithread Tokio runtime has one async worker, explicitly
configured independently of `TOKIO_WORKER_THREADS`. Spawned async helpers, timers
and nested `Backend::Async` actors can progress on that worker while the root
thread blocks; `block_in_place` remains supported. One blocked async worker can
still stall other work assigned to it. Await blocking helpers instead of assuming
that an isolated runtime provides unlimited parallel progress.

Tokio can create additional threads for `spawn_blocking`, filesystem or DNS work,
and each isolated runtime still consumes reactor descriptors. Stop and join
nested actors explicitly before their owning runtime exits. Actor `join()` waits
for actor completion, not detached helper completion or final runtime destruction;
nonterminating blocking helpers can keep the native thread alive during teardown.
`spawned_rt::threads::block_on` uses the same one-worker runtime configuration.

## Owned async groups (main, unreleased)

`ActorSystem::async_group(name, workers)` creates a parked async pool with an
explicit worker budget. `actor.start_on(&group)` places an actor there; ordinary
ActorRef request/reply crosses groups. The system owns admission, registered
helpers and runtime teardown, with incomplete shutdown reports at expiry.
[Usage and recovery](../../docs/GUIDE.md#execution-groups-and-supervision) and
[asserted examples](../../examples/capabilities/README.md) describe the contracts.
The budget counts async workers; owners/control and blocking helpers add threads.
These groups do not currently expose CPU affinity or spinning.

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
new per-actor public API for mixing pinned and default policies. Owned parked async groups now provide explicit ownership/placement; mixing
pinned policies still uses research configuration and needs application measurements. Communication compatibility alone is not a
reason to create more runtimes.


The [shared-versus-split runtime benchmark](../research/backend-experiments/2026-11-06-runtime-isolation/README.md)
now measures this boundary with equal total actor cores/workers, idle/busy
background actors, and both direct and general-runtime-relayed requests. It
shows a useful direct-path latency benefit with a background-throughput cost,
while the relay can still be delayed on the general runtime. Read its closed-loop
and short-run limitations before transferring the result to an application.
The [client source audit](../research/backend-experiments/2026-11-06-runtime-isolation/CLIENT-WORKLOADS.md)
identifies concrete ethrex/ethlambda paths and the measurements to try next.
