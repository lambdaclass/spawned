# Native actor executor experiment

All selectors require `backend-experiments`. Existing Actor/Handler/protocol and
Backend enum APIs and the default remain unchanged. Research selectors are private.

| Selector | Actor execution | I/O and helpers | Queue policy |
|---|---|---|---|
| native-global | N native executor threads, actor futures are custom jobs | R shared Tokio workers, default R=1 | Mutex-protected global FIFO, condition-variable parking |
| native-local | N native executor threads, migrating custom actor jobs | Same shared Tokio runtime | Preferred local LIFO, oldest local after three choices, peers steal oldest; 1 ms idle steal fallback |
| tokio-pool | N actual Tokio workers/tasks, process-owned runtime | Same runtime | Standard Tokio scheduler; no custom message turn |

Native jobs use a CAS readiness gate and one future mutex; duplicate wakes
coalesce, waking during a poll is remembered, and completion is terminal. Each
native actor poll uses a one-poll `Handle::block_on` bridge that installs Tokio
context and cooperative budget. The outer poll always returns Ready and never
parks waiting for the actor. Helpers/timers/network I/O bind the shared runtime.
This is a non-Tokio actor scheduler with a Tokio I/O/helper dependency, not a fully
Tokio-free runtime. A global mutex is a safe baseline, not a claim of scalability.

Pools are process-owned; zero actors does not terminate detached helper work.
Explicit terminal shutdown requires actors stopped/joined and helpers awaited,
rejects self-join, and prevents new actors. Non-async child actors hold conservative
runtime leases. Native actors have no Tokio task ID; debug warnings tolerate it.
Synchronous inter-actor waits, runtime-specific TLS/task-local assumptions,
`block_in_place`, raw blocking handlers and helper cancellation need compatibility
qualification; this executor is not preemptive. A blocked native worker does not
block the separate I/O reactor but still removes actor execution capacity.

## Gates before measuring

Six executor tests and three Loom gate models passed on fede256. Full workspace
all-feature tests, all-target/all-feature Clippy, and the concurrency all-feature
suite under each selector passed. The inherited `badly_behaved_thread_non_blocking`
test is excluded under selected executors, as in earlier shard validation. Shard
pinning-specific tests remain conditional on sharded; they are not evidence of
native affinity. Native actor coverage includes lifecycle/panics, monitors, nested
spawn, helper/timer/TCP, caller-runtime drop, blocking child lifetime, shutdown
publication, actor Drop panic isolation, and terminal spawn rejection. Agy approved
code for experimental measurement; no performance result is implied by that gate.

## Equal resources and workloads

Use the same actor physical CPU mask 2–5 and disjoint producer/collector masks as
the previous campaign, nice=10, shared host, workers not individually pinned.
`fede_native.py` tests total runtime-worker budgets B=3/4/5. Native configurations
use B−1 actor workers plus one I/O worker; Tokio and static shards use B workers.
Also compare equal actor-worker counts across budgets: native N+1 versus Tokio N,
and Tokio N+1 to expose the extra-thread cost. Five runtime workers share four
physical CPUs; this does not grant five cores. Owner/main and additional helpers
are outside this runtime-worker count; per-thread stats include reactor CPU.

Graphs use 1,920 actors (divisible by 2/3/4/5 worker layouts), 256 producers,
eight handlers/root, chain/fan-out, both spawn-order/placement controls, 1,048,576
roots, five fresh repetitions. Only static shards enforce same/cross placement;
for migrating native/Tokio schedulers the map controls spawn order. Do not pool
these with the older 1,024-actor dataset. Offered traffic uses 240 actors/four hot,
80/20 work, fixed absolute 500k/900k/1.2M requests/s, two seconds, three repetitions;
these are deliberately common rates, not per-policy capacity estimates. Preserve
the prior 1%/1 ms admission validity test and censored deadline/drain accounting.
Profile runs are separate. Each stage records source, binaries, exact schedule,
thread resources and controller hash. No builds/tests/plots overlap timing stages.
Every process has a watchdog; stop launching new work at the overnight deadline.
