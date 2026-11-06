## Findings and next experiments

Keep Tokio as the default. The longer actor-to-actor experiment confirms that a policy which helped independent requests in the earlier study can lose when actor replies cross fixed worker boundaries. Placement, communication pattern and overload behavior matter more than a single peak-throughput number.

Four worker threads, 1,024 actors, 256 closed-loop native producers, 1,048,576 logical root requests, eight handlers/root. Each cell is **k root replies/s / caller-observed p99 ms**, median of five fresh processes; figures show full min/max ranges. Same/cross is an enforced placement for shards and only a spawn-order control for Tokio.

| Policy | Chain same | Chain cross | Fan-out same | Fan-out cross |
|---|---:|---:|---:|---:|
| async | 955.2 / 0.410 | 948.3 / 0.337 | 950.5 / 0.348 | 957.8 / 0.331 |
| tokio-1 | 815.9 / 0.472 | 821.0 / 0.470 | 846.3 / 0.423 | 883.8 / 0.418 |
| tokio-16 | 847.3 / 0.585 | 839.6 / 0.586 | 853.0 / 0.430 | 874.8 / 0.415 |
| tokio-adaptive | 840.2 / 0.581 | 863.2 / 0.573 | 875.9 / 0.414 | 871.1 / 0.453 |
| sharded-1 | 850.0 / 0.325 | 506.0 / 0.583 | 934.7 / 0.303 | 612.2 / 0.473 |
| sharded-16 | 857.9 / 0.316 | 508.4 / 0.573 | 947.3 / 0.295 | 610.1 / 0.493 |
| sharded-adaptive | 845.1 / 0.332 | 513.3 / 0.572 | 933.1 / 0.301 | 610.4 / 0.485 |

Sharded turn-16 falls from 857.9k to 508.4k root replies/s between same/cross chain placement (40.7% lower), while ordinary Tokio stays near 950k in both controls. The corresponding sharded actor-worker CPU cost rises from about 4,636 to 7,831 ns/root. This is evidence of a cross-worker coordination cost in this workload; it does not identify cache misses, kernel wakeups or any single cause. Same-placement fan-out reaches 947.3k with shards versus 950.5k with Tokio; this near parity is workload-specific, not proof of equivalence.

The additional explicit turn checks did not improve long-graph throughput over ordinary Tokio here. The default may benefit from continuation locality, but confirming that explanation needs targeted scheduler/driver controls. The shared ~950k plateau could also contain load-generator or instrumentation costs; do not call it the framework’s maximum capacity.

**Overload evidence is incomplete.** All 1,002 processes returned valid outcomes and completed their admitted work, but only **260/504 offered-load runs** met the predefined admission schedule. All 244 invalid runs remain visible, and every affected aggregate is marked. Do not rank those cells as equivalent offered traffic or interpret their tail differences as isolated backend effects. A better-controlled producer/observer experiment comes before stronger fairness claims.

A valid diagnostic at 950,237 requests/s concentrates four hot actors on one shard: sharded turn-16 has 66.3% hot caller timeouts (median, three valid schedules), despite a 0.014 ms other-cohort cold observed p99. The worst cold actor p99 is 0.216 ms, showing why other-shard and worst-actor tails must accompany each other. Good cold isolation does not cure a hot shard’s growing backlog. These are two-second injection windows with a two-second caller deadline, followed by full drain; they are not a steady-state capacity or SLO result.

CPU profiles are available for all 66 profile processes. Inclusive scheduler ancestry is nearly universal and cannot be interpreted as scheduler overhead. Mailbox/reply frames and per-worker CPU counters motivate experiments, not causal conclusions about the kernel or allocator. No hardware counters or off-CPU profiles were available.

The next tests are (1) a process-owned Tokio pool control, (2) native actor dispatch with shared I/O, using global FIFO and bounded local continuation/stealing queues, (3) equal total runtime-thread controls, (4) asynchronous versus blocking callers and instrumentation controls, and (5) longer bounded soaks with actor churn. Native schedulers must first pass wakeup models and real actor lifecycle/I/O tests. These remain opt-in research; no public API, default scheduler, delivery semantics or lifecycle contract is promoted or replaced.
