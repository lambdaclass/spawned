## Findings and recommendation

**Keep Tokio as the default. Retain full-actor sharding as opt-in research for
Linux workloads, with an explicit runtime owner and compatibility contract before
considering a supported backend.** The new matched comparison changes the earlier
native-loop story: gains depend strongly on environment, worker budget and traffic.
Five short, unpinned repetitions are exploratory evidence, not production forecasts.

### Full actor CPU scaling: 1,024 actors / 256 producers

| Environment | Actor workers | Existing Tokio replies/s | Sharded batch-16 replies/s | Tokio completion p99 | Sharded completion p99 |
|---|---:|---:|---:|---:|---:|
| Native macOS | 2 | 488,999 | 365,045 | 0.779 ms | 1.557 ms |
| Native macOS | 4 | 587,212 | 509,723 | 0.693 ms | 1.111 ms |
| Linux VM | 2 | 826,880 | 999,398 | 0.918 ms | 0.503 ms |
| Linux VM | 4 | 1,029,048 | 1,179,428 | 2.106 ms | 0.542 ms |

These are comparisons within each row. macOS runs on the host; Linux runs in an
OrbStack VM/container restricted to four virtual CPUs and 8 GiB. There is no
cross-OS kernel-speed claim. The previous native-loop advantage does not reproduce
in the large macOS full-actor case, while Linux sharding remains promising here.
These experiments do not isolate which envelope, allocation, wakeup, placement or
reactor cost explains the difference; profiling is needed for causal attribution.

### Throughput and overall p99 do not establish fairness

In Linux's four-worker hotspot case, fixed-16 sharding improves overall p99 from
1.465 to 1.053 ms, but **cold-actor p99 worsens from 0.553 to 1.305 ms**. Static
placement can isolate some traffic and concentrate other traffic. Use cold/hot
breakdowns, not only the overall distribution, when deciding a scheduling policy.

Batches are not a universal improvement on existing Tokio. With two macOS workers
and 128 actors, Tokio-adaptive reaches 492k/s versus baseline 392k/s, but their run
ranges overlap (453–548k and 300–509k). With four workers, the same case is roughly
596k versus 602k/s. At 1,024 actors, Linux Tokio-adaptive trails its unmodified Tokio
baseline at both budgets. Fixed batch 16 can improve on forced yield-every-message
without improving on Tokio's existing cooperative policy; always keep `async` as
the reference, not just the artificially frequent-yield control.

The balanced RPC cases usually have at most one outstanding request per actor,
so they often do not sustain a mailbox backlog. Differences between those cells
cannot automatically be credited to batch size. The hotspot case exercises a busy
mailbox; burst and offered-rate sweeps should quantify turn policy further. Also,
`-1` means an explicit one-message turn, not the absence of a batching policy.

### Async waits, networking and blocking work

Persistent TCP on macOS clusters around 150k echoes/s across these designs, with
wide overlapping ranges. Linux with two workers fixed-16 sharding reaches 349k/s versus 274k/s for
Tokio; Linux with four workers reaches 377k versus 302k/s, with mixed latency outcomes (fixed sharding
p99 2.050 ms versus Tokio 2.002 ms at four workers). The common server has one native
thread per socket; this is not a complete optimized application/network stack.

The 1 ms async-timer case also changes by environment and worker count. It measures
runtime/OS timer behavior plus scheduling, not calibrated 1 ms service or CPU work.

The blocking case deliberately sleeps inside an async handler. Its requested
100 µs sleep has Linux with two workers handler p99 around 3.7 ms. Sharding can reduce completion
p99 while also reducing throughput; Linux with four workers fixed-16 is about 1,403/s versus Tokio's
1,736/s, with p99 about216 versus310 ms. This is a scheduling tradeoff under blocking
misuse, not preemption, faster CPU execution, or proof of a better blocking backend.

### Correctness and useful library changes

The experiment reuses actual Spawned lifecycle machinery instead of shipping the
native benchmark loop. Review caught and corrected idle-time turn accounting,
stop during a yield, and last-actor runtime teardown under detached helpers,
blocking children and monitor waiters. The process-owned pool now has explicit
terminal shutdown after actors/helpers complete; documented current-thread and
caller-context limitations remain.

Agy also found a general completion-status bug: a dropped completion sender could
make an async wait return `Kill` while synchronous `exit_reason()` still returned
`None` and `ChildHandle::is_alive()` returned true. The [separate fix](https://github.com/lambdaclass/spawned/pull/180) preserves a
published final reason, treats closed-without-reason as `Kill`, and allows the
already-exited blocking fast path to work without entering another runtime.
Tests cover both tasks and thread completion modes and resource-exhaustion startup.

The performance dataset is pinned to clean revision `9b22346`, before that getter-only
status fix. All successful benchmark cells are of that source/binary; performance
was not remeasured after the status-path fix. The later fix has separate validation.
Do not describe these numbers as measurements of an unrecorded final-branch binary.

### What to do next

1. Land the independently reviewable completion-status fix and document blocking
   pool / dedicated-reactor capacity. Keep the runtime experiment opt-in.
2. Profile the representative macOS loss and Linux gain, then test actor-to-actor
   chains/fan-out with controlled same/cross-shard placement and skew. This decides
   whether locality or a specific wakeup cost justifies a new supported backend.
3. Run fixed-arrival-rate saturation tests with intended-arrival timestamps,
   producer-lag accounting, cold tails and queue/RSS time series. Closed-loop RPC
   does not establish latency targets under overload.
4. Separately evaluate borrowing blocking workers per active handler to address
   persistent blocking actors exhausting the finite pool. That design is proposed,
   not implemented; sequencing, migration/TLS, async work and shutdown need review.

[Research protocol and decision criteria](RESEARCH-NEXT.md) describe these
follow-ups. Add another kernel backend, affinity policy or custom work-stealing
executor only after the evidence identifies the bottleneck or missing behavior.
