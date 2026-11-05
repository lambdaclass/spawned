# Performance baseline

Run from the repository root with the Rust version in `.tool-versions`:

```sh
python3 benchmarks/run.py --output /tmp/spawned-baseline --repetitions 5
```

The runner builds the optimized `baseline` bench executable once, randomizes the
case/backend/repetition schedule, and launches each sample in a fresh process.
It saves configuration, revision, compiler, source/binary hashes, raw CSV, summary
CSV, build output, and per-process logs. Output paths must be new directories.
Each sample has a 90-second process watchdog. A failed sample fails the campaign;
partial raw data and logs remain available. Do not interpret incomplete campaigns
as comparable throughput results. `--cases` and `--backends` select smaller runs.

## Scope and load

Five implementations perform the same payload scan and optional CPU work:
Spawned async, blocking-pool, dedicated-thread task actors, synchronous thread
actors, and a plain Tokio unbounded mpsc + oneshot loop. Tokio runtime worker
count is two throughout. Native actor and producer/collector thread counts vary;
this is not an equal-total-thread or pinned-core experiment. Scheduling, affinity,
other machine load, frequency scaling, and OS power policy are uncontrolled.

- `rpc_single`: one actor and one sequential request producer.
- `rpc_parallel`: four actors and four producers, one outstanding RPC per producer.
- `mostly_idle`: 128 initialized actors, four active, same traffic as rpc_parallel.
- `burst`: four producers submit 50,000 requests to one actor without pacing.
- `large_shared_payload`: 4 KiB shared immutable payload scanned by each handler.
- `cpu_paced` / `cpu_overload`: identical work, scheduled at 20k / 200k requests/s.
  These names express intended offered load; actual service capacity and enqueue
  lag must be checked before calling either a steady-state or overload result.
- Timing-off and allocation-counting cases estimate benchmark measurement costs.

Payloads are shared `Arc<[u8]>`, not deep-copied per request. These cases measure
request/reply, not send-only throughput, real networking, I/O waits, application
state transitions, broadcast, recovery, or an Ethereum client workload. Each
producer addresses one actor, preserving that sender's FIFO for reply collection.

Every actor is warmed before measurement. Producer threads and primary sample
buffers are prepared before the start barrier; burst/paced collectors are created
after it. The campaign is finite (at most one million requests per process), but
all mailbox and collector queues are unbounded. This does not establish safety
under indefinite overload. The runner's process timeout bounds stalled samples.

## Measurements

- `completion_*`: scheduled arrival to **end of handler**, excluding reply delivery.
- `observed_p99`: scheduled arrival to caller observation of the reply, including
  queueing in the benchmark collector. RPC cases observe replies immediately;
  burst/paced collectors consume each producer's replies in sender order.
- `queue_p99`: actual enqueue-call start to handler start; includes producer-side
  envelope allocation/channel send and scheduling. It is not a pure mailbox timer.
- `handler_p99`: handler elapsed time, including payload scan and CPU loop.
- `enqueue_lag_p99`: scheduled arrival to actual enqueue-call start. Paced load
  uses an absolute schedule rather than waiting for the previous reply; sleep
  resolution and a late producer can still create bursts. Report lag explicitly.
- `completed_per_s`: replies observed divided by measurement duration, including
  final producer joins. Paced runs therefore track offered load when unsaturated.
- Process peak RSS and CPU come from `/usr/bin/time`; they include initialization,
  warmup, the 100 ms start delay, teardown, sample buffers and collectors. They
  are not isolated actor memory or steady-state CPU utilization measurements.
- `allocations` and `allocated_bytes` count successful alloc/realloc/zeroed-alloc
  calls and requested bytes during the measurement window **only when enabled**.
  They include driver work and are not live bytes or framework-only allocations.
  Zero in counting-off rows means unavailable. The allocator wrapper retains a
  flag check even when counting is off, so this is not a zero-instrumentation build.

The benchmark always measures reply/handler completion timestamps. Timing-off
removes the extra handler-start clock read and queue/service breakdown, retaining
the same payload layout. It does not measure a future production metrics exporter.
Allocation counters use shared atomics and can perturb concurrent workloads.

The summary contains the median and min/max of each per-process measurement.
A median of per-run p99 values is **not** a pooled p99. Keep raw results, report
variation, and repeat on a quiet target machine before choosing an optimization.
No significance tests or production-performance rankings are implied.

## Validation

Replies must have the submitted request ID; completed sample count must match
submitted count. The runner fails on process errors, malformed output, and
watchdog expiry. Run a small smoke campaign across every backend and scenario
before the full campaign. `cargo clippy --all-targets -- -D warnings` includes
the bench code. The production library is unchanged by this benchmark.

Use the baseline to select profiling targets. Client integration, send-only
traffic, real I/O, equal CPU budgets, long RSS/queue-growth experiments, and
framework-level observability remain follow-up work from `docs/PLAN.md`.
