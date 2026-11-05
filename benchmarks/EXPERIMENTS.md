# Backend design experiments: team guide

This study compares runtime ownership, transport costs, scheduling, waiting and
I/O. It does not select a production replacement from a single microbenchmark.
The public `Actor`, `Handler<M>`, protocol, `ActorRef`, message and reply APIs
remain unchanged. Existing defaults remain unchanged. Runtime experiments are
compiled only with `backend-experiments`, selected by a private test environment
variable. Native scheduler/transport and I/O controls exist only in bench code.

## What each implementation means

| Variant | Execution resources | Purpose and limitations |
|---|---|---|
| blocking | Persistent actor loops occupy the caller Tokio blocking pool | Current `Backend::Blocking`; the finite pool can be exhausted by a large population of persistent actors. |
| async | Actor futures on the driver's two-worker Tokio runtime | Current general-purpose baseline; full Spawned lifecycle and messages. |
| threads | One native OS thread per synchronous actor | Current actor implementation; no executor needed by handlers; still uses some Tokio synchronization types. |
| plain-tokio | One task per worker-loop instance on two runtime workers | Simpler channel/oneshot control; omits actor cancellation/unwind/type-erasure machinery. |
| dedicated-default | One owner thread plus a default multi-worker Tokio runtime per actor | Current `Backend::Thread`. Resource-heavy at large actor counts. Restricted to small cases in this matrix. |
| dedicated-one | One owner thread plus one runtime worker per actor | Reduces runtime worker count, but still has multiple threads per actor. |
| dedicated-current | One owner thread with a current-thread Tokio runtime | Removes helper runtime workers. A blocking handler stalls nested helper tasks. Each actor still owns a reactor and descriptors. |
| dedicated-managed | One owner thread per actor, with a process-owned two-worker shared runtime | Avoids per-actor reactors and caller-lifetime dependence; helper-resource isolation differs and the pool lives until process exit. |
| dedicated-shared | One owner thread per actor, sharing the caller's two-worker Tokio runtime | Avoids per-actor reactors and allows helper progress elsewhere. Caller runtime must outlive actors. |
| native-block | One native thread per prototype actor; bounded addressed mailbox; blocking receive | Tokio-free actor/reply data path when bounded replies and direct observation are selected. Does not implement all Spawned lifecycle features. |
| native-hybrid | Native-block plus up to 256 empty polls before blocking | Measures the CPU/latency tradeoff of short spinning. It is a fixed policy, not auto-tuned. |
| native-spin | Native-block with continuous empty polling | Latency-oriented control; consumes CPU while idle. Limited to four actors to avoid uncontrolled oversubscription. |
| sharded-1 / sharded-16 | Two native workers; actor queues partitioned by actor ID; bounded batches of 1 / 16 | Round-robin scans include idle queues. Actor placement is static within each benchmark process. |
| ready-1 / ready-16 | Two native workers share a runnable-actor queue; batches of 1 / 16 | Avoids scanning idle actors; shared ready-queue lock can contend. A mutex-protected scheduling gate prevents simultaneous ownership and lost wakeups. |
| ready-adaptive | Shared runnable queue; full/empty-turn-adaptive batch size 1–64, with a 50 µs turn budget | Experimental adaptive batching. The time budget is checked between handlers and cannot preempt a handler. |
| local-ready-16 | One runnable queue per worker; fixed actor-to-worker placement; batch 16 | Combines active-only scheduling and per-shard placement. Can suffer load imbalance; no work stealing. |

Native prototypes use ordinary owned Rust messages and bounded `std::sync::mpsc`
mailboxes (256 slots by default), not Copy-only broadcast rings. A successful
send never silently overwrites an unread request. Blocking enqueue can stall a
producer when full. They are design controls: dynamic registration, async handler
polling, timers, monitors, panic recovery, cancellation and cyclic actor sends
still need implementation and tests before such a scheduler can become a backend.
They do not establish that a drop-in implementation will have identical speed.

## Reply transport experiment

The `async-reply-*` and `threads-reply-*` variants deliver the same benchmark-local
`Probe` message through Spawned, carrying an explicitly selected reply transport:

- `std`: a general unbounded `std::mpsc` channel per request.
- `bounded`: a one-slot `std::mpsc::sync_channel` per request.
- `tokio`: Tokio's dedicated oneshot primitive per request.

These bypass the normal framework reply envelope to isolate reply choices using
a matched message shape. They do not alter public request/reply behavior. Compare
within this matched group; comparison with the framework path includes envelope
layout and dispatch differences. Allocation counters include driver activity and
report successful allocation/reallocation calls and full requested sizes.

All new message experiments use direct reply observation with a process-level
watchdog. This avoids a Tokio timeout future per observed reply; thread and
future wake mechanisms still differ. The original baseline and its timer-based
observer remain available in `baseline.rs`; historical results are not rewritten.

## Scale and workload matrix

| Case | Actors | Producers | Measured requests per run | Routing / question |
|---|---:|---:|---:|---|
| rpc1 | 1 | 1 | 32,768 | Closed-loop single-request latency. |
| burst32 | 1 | 32 | 131,072 | Finite burst into a busy mailbox; separate collectors. |
| active128_p32 | 128 | 32 | 131,072 | Round-robin traffic reaches every actor. |
| active128_p128 | 128 | 128 | 131,072 | One closed-loop producer per actor. |
| active512_p128 | 512 | 128 | 262,144 | Every actor receives traffic; at most 128 requests in flight. |
| active1024_p256 | 1,024 | 256 | 262,144 | Large population and producer count; resource limits are results. |
| hotspot128 | 128 | 128 | 131,072 | Half of requests go to actor 0; remaining traffic spreads over others. Compare hot and cold p99. |
| lowload | 4 | 32 | 10,000 | Scheduled 5k requests/s, 5,000 CPU iterations per handler. |
| overload | 4 | 32 | 32,768 | Scheduled 1M requests/s with the same CPU work. |
| idle4 | 4 | 1 | 1 | One-second idle interval after warmup; reveals spinning CPU cost. |
| alloc1 | 1 | 1 | 32,768 | Allocation counting enabled; not a timing-overhead-free result. |

Closed-loop producers wait for replies; increasing the actor population beyond
the producer count does not make all actors simultaneously busy. Paced and burst
cases can accumulate more outstanding requests. Actor mailboxes may be bounded
in prototypes, but collector channels/sample storage remain finite-run driver
allocations; no experiment proves safety under indefinite overload.

The original 128-actor baseline had only four active actors. These new scaling
cases deliberately send to every actor. Default per-actor multi-worker runtimes
are not launched at 512/1024 actors to avoid thousands of runtime threads.
One-worker-per-actor is capped at 512. Spin/hybrid controls are capped at four.
Skipped combinations are design/resource constraints, not successful measurements.

## Runtime compatibility characterization

`concurrency/tests/backend_experiments.rs` runs each runtime mode in a separate
process. It checks:

1. Nested async spawning and timer completion.
2. A real TCP request/reply using the runtime's reactor.
3. Whether a detached helper progresses while the actor blocks.
4. Actor shutdown and completion.
5. Actor behavior after its caller runtime is dropped.

Tests intentionally characterize differences: current-thread mode cannot run
helpers while a handler blocks, and shared-runtime mode cannot provide independent
runtime lifetime. Passing this suite does not mean those variants are drop-in
replacements. The standard workspace suite covers ordinary lifecycle behavior;
no new default should be chosen without wider client compatibility tests.

## Real I/O mechanisms

`io_designs.rs` reuses persistent loopback TCP connections with TCP_NODELAY.
Each connection sends and validates 64-byte request/reply frames. Cases use
1, 16 and 128 connections, 1,000 measured exchanges per connection after warmup.
A common native echo server uses one OS thread per connection. The per-connection controls also use one client OS thread per connection. The
`*-mux` controls distribute all connections across one client reactor, and
`*-mux4` across up to four reactors. The server remains identical across these
cases. None is a complete actor backend.

| I/O variant | Mechanism | What it tests |
|---|---|---|
| std-io | Blocking standard-library TCP | Synchronous syscall/socket control. |
| tokio-io | Current-thread Tokio and Mio | kqueue on macOS; epoll on Linux. |
| monoio-legacy | Monoio's non-Tokio legacy driver | kqueue on macOS; epoll on Linux, with owned-buffer APIs. |
| monoio-uring | Explicit Monoio io_uring driver | Linux only; preflight fails explicitly if unavailable or denied. No silent fallback. |

Monoio and Tokio differ in buffer ownership, scheduling and code, not just kernel
syscalls. Monoio's thread-local futures are not automatically compatible with
Spawned's `Send` handler futures. These tests do not prove that arbitrary existing
Tokio-based application handlers can run unchanged on Monoio.

## Measurement and reporting

Each sample is a fresh release-mode process, with warmup and randomized ordering.
Request IDs and completed counts are checked. Runs retain raw values, failures,
compiler/source revisions, environment, descriptor limits and Linux cgroup limits.
The watchdog kills the process group, preventing a timed-out child from silently
contaminating later samples. Failed samples never receive fabricated throughput.

Handler-completion latency, caller-observed reply latency, queue wait, handler
elapsed time and enqueue lag are distinct. CPU and peak RSS are process-wide,
including initialization, idle delay, driver/sample storage and teardown.
Reported medians of run-level p99s are not pooled p99s. Error bars show run-to-run
ranges, not confidence intervals. No cross-OS absolute speed comparison is valid
without accounting for the VM and budget differences.

Linux runs in OrbStack's ARM64 VM, with the container restricted to CPUs 0–3,
8 GiB RAM and 4,096 PIDs. Its source is mounted read-only. `seccomp=unconfined`
is used inside this unprivileged VM container to allow io_uring syscalls; this is
an experiment setting, not a production security recommendation. Descriptor limits
are recorded; a separately raised limit is a distinct experiment. macOS runs use
the native host's scheduling and cannot claim equal CPU placement.

## Reproduction

```sh
python3 benchmarks/design_experiments.py --output /tmp/design-smoke --repetitions 1 --smoke
python3 benchmarks/design_experiments.py --output /tmp/design-mac --repetitions 3
python3 benchmarks/design_experiments.py --output /tmp/design-fd-control --repetitions 3 --group messages --variants dedicated-current --cases active1024_p256 --fd-limit 16384
```

Use `--group`, `--cases` and `--variants` to limit a campaign. The experimental
feature must never be enabled implicitly for production. Public API preservation
is a requirement for any eventual backend implementation, not a conclusion from
these prototype controls.

## Research informing the candidates

- [Seastar's shared-nothing architecture](https://seastar.io/shared-nothing/): per-core ownership and explicit cross-core messaging. Our prototype does not reproduce its allocator/network stack.
- [Glommio](https://github.com/DataDog/glommio): cooperative thread-per-core async I/O on Linux io_uring; requires a Linux-specific integration study.
- [Akka dispatcher throughput](https://doc.akka.io/japi/akka-core/current/akka/dispatch/Dispatcher.html): bounded mailbox batches trade throughput against fairness.
- [Monoio legacy driver](https://github.com/monoio-rs/monoio/blob/master/docs/en/use-legacy-driver.md): kqueue/epoll counterpart to its io_uring driver.
- [Mio](https://github.com/tokio-rs/mio): OS readiness mechanisms underlying Tokio networking.
- [Compio](https://github.com/compio-rs/compio): portable thread-per-core I/O candidate for later integration; researched, not measured in this campaign.
- [Silver/flux research](../docs/research/silver-flux.md): transport and observability ideas, with explicit differences from actors.

## Scheduler review correction

The first exploratory runnable-queue prototype used separate atomics for a pending
count and scheduling flag. Review identified a possible lost notification and
spurious rescheduling while a sender had reserved but not published work. Those
samples are invalid for selecting a design and are excluded from final graphs.

The measured revision uses `support/ready_gate.rs`: a mutex serializes producer
notification with the worker's lookahead-and-release decision. The worker retains
one lookahead message rather than maintaining a speculative pending count.
`ready_gate_loom.rs` models that same gate implementation under publication/finish
interleavings. Adaptive batching grows after full turns and resets on empty turns;
its clock reads are part of the measured policy cost, not free instrumentation.

Bounded and unbounded burst tails are different admission experiments: blocking
a producer delays creation of its next burst timestamp. Burst p99 is therefore
not a matched offered-load comparison. Use the absolute-arrival paced cases and
a separately pre-sized capacity control before attributing differences to queue
implementation. Hotspot placement deliberately creates a hot shard; it measures
load imbalance as well as batching. Worker counts and total process CPU budgets
are distinct controls.

Multiplexed I/O variants (`tokio-mux`, `monoio-legacy-mux`, `monoio-uring-mux`)
drive all connections on one client runtime/thread. The per-connection-runtime
variants remain as resource controls. They use the same native echo server;
server thread costs remain common confounders. Linux io_uring is explicitly
selected and preflighted, with no silent fallback.

Additional controls include the existing `Backend::Blocking` pool, and I/O
multiplexing on four client reactors (`*-mux4`) as well as one (`*-mux`).
Per-reactor connections are partitioned round-robin; both configurations use the
same echo server. This separates reactor amortization from a one-client-core
bottleneck. Blocking-pool exhaustion is captured by a shorter process watchdog
in its separate capacity study, rather than fabricated as zero throughput.
