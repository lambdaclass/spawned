# Baseline benchmark methodology

Archived from `benchmarks/README.md` (2026-11-08). Defines the cases and
measurement columns produced by `benchmarks/run.py`. Not maintained; the
[benchmarks README](../../benchmarks/README.md) has the current run instructions.

## Scope and load

Five implementations perform the same payload scan and optional CPU work:
Spawned async, blocking-pool, dedicated-thread task actors, synchronous thread
actors, and a plain Tokio unbounded mpsc + oneshot loop. The driver Tokio runtime has two workers. The current dedicated task backend
creates a separate `Runtime::new()` per actor via `spawned_rt::threads::block_on`;
its worker count follows Tokio defaults / `TOKIO_WORKER_THREADS`, independently
of the driver setting. Native actor and producer/collector thread counts vary;
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
  warmup, the 100 ms start delay, teardown, percentile sorting, sample buffers and collectors. They
  are not isolated actor memory or steady-state CPU utilization measurements.
- `allocations` and `allocated_bytes` count successful alloc/realloc/zeroed-alloc
  calls and requested bytes during the measurement window **only when enabled**.
  They include driver work and are not live bytes or framework-only allocations.
  Reallocation adds the full requested new size even in place; it does not count
  only incremental growth or bytes moved.
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
framework-level observability were follow-up work at the time.

## Runner robustness

The process watchdog kills the complete benchmark process group on timeout or
interruption and retains stdout/stderr before raising. Unknown, duplicate and
valueless benchmark flags fail before actors are started; booleans accept
`on`/`off` and `true`/`false`. The runner requires parseable system time resource
measurements and uses locale C. New CSVs use LF, and allocation summary cells are
blank when counting was disabled. Historical datasets are unchanged.

Run `python3 -m unittest discover -s benchmarks -p test_runner.py` to exercise
real descendant cleanup and diagnostic retention. Metadata records the actual
compiler/Cargo versions; the historical native run used Rust 1.93 and Cargo 1.98
from separate Nix packages. `bench_source_sha256` hashes the bench driver only;
the clean Git revision and executable hash identify the full measured build.

Review archives are retained as requested provenance, with explicit editorial
limits. Suggested numeric adoption thresholds are reviewer proposals, not
approved production criteria. Baseline observer asymmetry remains disclosed;
the later matched design study adds a direct observer and full-actor controls.
