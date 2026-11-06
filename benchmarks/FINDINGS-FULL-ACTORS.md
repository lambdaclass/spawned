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
| Native Linux, fede | 2 | 1,779,639 | 1,712,394 | 0.233 ms | 0.187 ms |
| Native Linux, fede | 4 | 1,776,924 | 2,070,492 | 1.081 ms | 0.159 ms |

These are comparisons within each row. macOS runs on the host; Linux runs in an
OrbStack VM/container restricted to four virtual CPUs and 8 GiB. There is no
cross-OS kernel-speed claim. The previous native-loop advantage does not reproduce
in the large macOS full-actor case, while Linux sharding remains promising here.
These experiments do not isolate which envelope, allocation, wakeup, placement or
reactor cost explains the difference; profiling is needed for causal attribution.

### Native Linux reproduction on fede

The November 6 reproduction adds 525 successful measured processes on an AMD
Ryzen 9 9950X3D. It uses the same clean measured revision and seven variants, with
five repetitions per cell. The complete job, including producers, observers and
TCP server threads, shares CPUs 2–5 (four physical cores in one L3 group), at
nice=10. Workers are not pinned individually and the host is shared. This is a
separate environment from the ARM64 VM, not a controlled kernel or CPU comparison.

At 1,024 actors, fixed-16 sharding is 16.5% faster with 85.3% lower completion p99
at four workers. At two workers it is 3.8% slower with 19.8% lower p99. The larger
VM throughput gain is therefore not a portable constant. Four workers also do not
universally beat two within this four-core job budget; the producer/observer
threads compete for those CPUs too, and profiling is needed to explain the tails.

The other cells prevent a simple winner claim. At four workers and 128 actors,
fixed-16 sharding has a 2.305M/s median versus Tokio's 1.878M/s, but p99 rises from
0.156 to 0.555 ms; the throughput run ranges overlap. At two workers under skew,
cold p99 improves (0.237 to 0.116 ms) while overall p99 worsens (0.482 to 0.893 ms)
and throughput falls. Native Linux reproduces the scheduling tradeoffs, not a
universal fairness win.

For persistent TCP at four workers, medians are 588k/s and 0.821 ms for fixed-16
sharding versus 513k/s and 2.548 ms for Tokio. At two workers throughput is similar
and sharded p99 is worse (2.005 versus 1.339 ms). Timer throughput is close across
the reference and fixed-16 paths, about 59–61k/s. Deliberately blocking handlers
show similar throughput but lower sharded tails; they remain non-preemptible.
The TCP server and producer setup remain common controls, not an optimized client.

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

The performance dataset is pinned to clean revision `9b22346`, before both the getter-only status fix and the later shutdown-publication fix.
The latter closes/drains/discards admitted requests before reporting ordinary actor
completion; its deterministic regression fails on the historical source and passes
with the fix. Fede functional checks use corrected source separately. All successful benchmark cells are of that source/binary; performance
was not remeasured after the status-path fix. The later fix has separate validation.
Do not describe these numbers as measurements of an unrecorded final-branch binary.

### What to do next

1. Keep the merged completion-status and shutdown-publication fixes covered by
   regressions, document blocking-pool / dedicated-reactor capacity, and retain
   the runtime experiment as opt-in research.
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
