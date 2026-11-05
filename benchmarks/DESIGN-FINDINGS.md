## Findings and decision

**Keep Tokio as the default. Continue sharded and adaptive runnable scheduling as opt-in
research candidates, and improve dedicated-runtime resource ownership separately.**
This campaign finds useful design tradeoffs, not a universal replacement. Native
scheduler controls do not yet implement Spawned's full lifecycle or async handlers.
Three short repetitions on an unpinned host are descriptive evidence, not a
statistical proof or a production capacity forecast.

### What we learned on macOS

| 1,024 actors / 256 producers | Median replies/s | Median completion p99 | Runtime/scheduler workers |
|---|---:|---:|---:|
| Spawned async | 459,298 | 0.850 ms | 2 |
| Native ready queue, adaptive batch | 545,723 | 0.560 ms | 2 |
| Native ready queue, fixed batch 16 | 462,953 | 0.645 ms | 2 |
| Spawned synchronous thread actors | 276,040 | 0.730 ms | 1 thread per actor |
| Spawned async | 545,484 | 1.444 ms | 16 |
| Native ready queue, adaptive batch | 515,021 | 0.472 ms | 16 |

The adaptive control's two-worker median is 18.8% above async, but their observed
throughput ranges overlap: async 458,895–561,504 and adaptive 493,039–571,128
replies/s. With sixteen workers async leads median throughput. Every row still
has producer/observer threads outside the scheduler worker count. Native controls
use bounded concrete messages; the framework uses boxed envelopes and has more
semantics. This does not isolate batching as the cause of the entire difference.

Other concrete findings:

- **Spinning buys latency with reserved CPU.** Four idle `native-spin` actors use
  about 3.94 process CPU seconds per wall second. Blocking and short-spin controls
  round to approximately zero at the process timer's resolution. This is not a
  wattage measurement. Continuous spinning belongs only in a deliberately
  provisioned low-latency mode, if later application tests justify it.
- **Per-actor reactors have a resource ceiling.** `dedicated-current` at 1,024
  actors failed all three starts with the host's 4,096-descriptor soft limit.
  Raising only the benchmark child's limit to 16,384 let all three runs complete
  (median 261,070 replies/s). Removing helper runtime threads does not eliminate
  per-actor reactor costs.
- **Persistent blocking-pool actors can exhaust their executor.** At 1,024 actors,
  all three `Backend::Blocking` samples hit the 15-second watchdog; at 512 actors
  all three completed. Linux reproduced the same three watchdogs at 1,024 actors
  and three completions at 512. The benchmark's Tokio builder leaves the blocking-pool
  maximum at its dependency's [default of 512](https://github.com/tokio-rs/tokio/blob/tokio-1.48.0/tokio/src/runtime/builder.rs#L282), and each actor retains a blocking
  worker for its loop. The stalled warmup is consistent with queued actor loops
  having no worker available. This is a capacity/liveness result, not zero
  measured throughput.
- **A shared helper runtime is a tradeoff, not a free win.** The process-managed
  two-worker variant reaches only about 60k replies/s at 1,024 actors on this host.
  The separate caller-owned shared variant additionally depends on caller runtime
  lifetime. Current-thread mode cannot progress a spawned helper while its owner
  thread blocks. All are characterized explicitly by compatibility tests.
- **Reply shape matters.** In matched async `Probe` tests, Tokio oneshots use about
  3 allocation/reallocation calls per request versus about 6 for either standard
  channel. The normal async request path already uses Tokio oneshots: this is
  evidence against replacing them casually, not a newly discovered optimization
  of that path. Counts include driver activity.
- **A bounded queue can make a burst latency chart misleading.** A 256-slot queue
  blocks producers, delaying subsequent request timestamps. When capacity is
  raised to 131,072 in the control, native burst completion p99s grow into roughly
  91–183 ms. Their earlier few-millisecond tails were partly admission behavior,
  not an equivalent offered workload being processed dramatically faster.
- **Static placement needs skew tests.** In the two-worker hotspot case,
  `sharded-16` cold-actor p99 is 2.912 ms, versus 0.726 ms for async and 0.216 ms
  for ready-adaptive. This includes placement imbalance; it does not establish a
  general scheduling or fairness ranking.
- **I/O architecture matters as much as the API.** In the supplemental 128-socket
  macOS test, one-reactor Monoio kqueue and Tokio reach similar median throughput
  (239k and 236k echoes/s), with different tails (0.724 and 2.974 ms). Four client
  reactors yield about 210k and 198k/s; Tokio's tail improves to 0.843 ms. More
  reactors do not automatically improve throughput. The common echo server still
  uses one thread per socket, and this is loopback I/O rather than a networked
  application benchmark.

### What we learned in the Linux VM

The container has four allowed CPUs, 8 GiB RAM and a 4,096-PID limit. Its main
scheduler matrix uses two workers, with a separate four-worker control. The main
subset covers ten implementations and seven message cases plus all ten I/O
controls; macOS covers the wider message matrix. Neither CPU affinity per worker
nor bare-metal Linux was tested. Compare rows within each environment.

| 1,024 actors / 256 producers | Median replies/s | Median completion p99 | Workers |
|---|---:|---:|---:|
| Spawned async | 942,293 | 0.696 ms | 2 |
| Global ready queue, adaptive | 748,750 | 0.464 ms | 2 |
| Local ready queues, batch 16 | 964,785 | 0.472 ms | 2 |
| Scanning shards, batch 16 | 1,212,820 | 0.413 ms | 2 |
| Spawned async | 1,081,213 | 2.150 ms | 4 |
| Global ready queue, adaptive | 883,769 | 0.424 ms | 4 |
| Local ready queues, batch 16 | 1,254,441 | 0.449 ms | 4 |
| Scanning shards, batch 16 | 1,266,961 | 0.514 ms | 4 |

The global adaptive scheduler's macOS advantage does not reproduce here. Sharding
is the more promising large-population Linux control. However, at 128 actors with
four workers, async reaches 2.55M replies/s versus 2.50M for scanning shards and
1.60M for local ready queues. There is no single scheduler winner across scale,
worker budget and platform. These remain concrete-message controls with fewer
semantics than Spawned.

For 128 persistent loopback sockets with four client reactors:

| Client mechanism | Median echoes/s | Median echo p99 |
|---|---:|---:|
| Tokio / epoll | 992,365 | 0.825 ms |
| Monoio / epoll | 1,285,613 | 0.700 ms |
| Monoio / io_uring | 862,842 | 0.307 ms |

Explicit io_uring worked without fallback. It improves the measured tail versus
both four-reactor readiness controls, but does not lead throughput. The Monoio
epoll throughput range is wide (1.15M–1.86M/s); three processes do not establish a
stable production advantage. With one reactor, Monoio epoll and io_uring reach
about 502k and 468k/s. Buffer ownership, executor behavior, syscall choices and
reactor parallelism all change between controls. This does not measure the
isolated effect of io_uring or justify replacing Tokio.

### What should become library work

1. Preserve the current actor/protocol APIs and Tokio default while improving
   measurements and runtime guidance. Document blocking-pool saturation and
   dedicated-runtime ownership explicitly.
2. Evaluate sharded ownership and bounded turns/adaptive batching in a backend that actually runs
   Spawned envelopes, async handlers, cancellation, monitors and panic cleanup.
   Compare it with the existing executor at the same worker budget; do not ship
   the benchmark loop as a finished backend.
3. Investigate an explicitly owned shared runtime for dedicated actors, with
   helper-task isolation, shutdown and lifetime contracts. The current managed
   prototype's poor large-population result rules out promoting it unchanged.
4. Treat thread/core affinity, short spinning and sharded placement as optional
   specialist policies. Add work-stealing/placement experiments before selecting
   a static layout for uneven traffic. No affinity or work-stealing backend was
   measured here.
5. Consider a Linux io_uring or portable non-Tokio backend only after bridging the
   current `Send` handler model, timers and application I/O compatibility. Kernel
   I/O controls alone cannot establish a drop-in backend.

### Trust and coverage

The old separate-atomic runnable gate had a possible lost wakeup and speculative
rescheduling. Those exploratory samples are excluded. The corrected code shares
its gate with a bounded Loom model (preemption bound 3, up to 10,000 permutations).
That model exercises publication/finish races; it does not model a complete actor
runtime or prove every possible execution. Independent agy review of the corrected gate and one-reactor I/O found no
remaining blockers for that benchmark scope after the correction, while retaining
all disclosed prototype limitations.

All measured campaigns retain source revisions, binary hashes, process outcomes,
resource limits and raw samples. Default FD failures and blocking watchdogs appear
in the report. Initial sandbox-denied socket/measurement runs and smoke-only runs
are excluded from measured datasets. Standard workspace tests, runtime-mode
contract tests, model checks and lint checks are reported separately from
throughput. Public API preservation does not imply every experimental runtime has
identical behavior.
