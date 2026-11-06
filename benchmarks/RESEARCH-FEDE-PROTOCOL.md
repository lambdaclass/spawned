# Profiling, actor graphs and offered-load fairness on fede

This follow-up tests actual Spawned actors while preserving the public APIs and
Tokio default. The earlier matched study and its `9b22346` measurements remain
separate. New samples record the corrected source revision, executable hashes,
compiler/flags, CPU topology, load snapshot and exact randomized invocation order.

## Machine and CPU budget

Run everything on native Linux `fede-256gb`, an AMD Ryzen 9 9950X3D server. Actor
workers inherit CPUs 2–5. `workload_research` explicitly moves its producer,
collector and sampler threads to CPUs 6–9. The runner verifies disjoint sets and
physical core IDs, excluding SMT siblings. Two actor workers may migrate within
the same four-core mask; four workers share that mask. Worker threads are not
individually pinned. All variants use these same settings. Other users, frequency
scaling and cache effects remain uncontrolled; use nice=10.

The legacy `designs` CPU/hotspot/TCP profiles retain all their threads in CPUs 2–5,
matching that harness's earlier shared-budget arrangement. Their profiles are
separate from the graph/load affinity experiment.

## Actor-to-actor workloads

- 1,024 actual actors, grouped into 128 independent eight-node DAGs; 256 native
  closed-loop producers and 65,536 logical root requests per fresh process.
- An eight-node chain forwards one request at each hop and returns the reply.
  Seven-way fan-out starts every child request before awaiting the replies.
  Each logical request invokes eight handlers in either topology.
- Same-shard and cross-shard maps preserve logical IDs, work and edges. Sharded
  actors verify their actual worker names against the map during setup. Under
  ordinary Tokio, the map only controls spawn order; work stealing remains free
  to place and migrate actors.
- All seven existing policies, two/four worker budgets, both topologies/maps and
  five randomized repetitions. Replies validate request ID, node count and the
  visited node-ID sum. Every submitted logical ID must occur exactly once.
- CPU work is 100 dependent integer iterations per node. These are synthetic
  coordination workloads, not a full application or calibrated nanosecond service.

## Fixed offered-rate and burst traffic

Use 128 actors: four hot actors receive 80% of requests, with 5,000 CPU iterations
per hot handler; cold actors share the other 20% and perform 100 iterations. The
hot actor count, logical routing and work stay constant across worker budgets.
Place hot actors across workers or concentrate them on one shard, maintaining
equal actor counts per worker. Cold shard-zero/other cohorts use that map; for
Tokio they are nominal cohorts, not enforced runtime isolation.

Three closed-loop baseline references and offered-load pilots establish a common
absolute rate grid for every policy/map at each worker budget. Pilot service rates
come from completed-counter changes during the middle of the injection window;
inspect backlog and enqueue lag before describing any grid point as saturation.
The main grid is 50%, 90%, 105% and 125% of that reference, plus 1,000-request bursts
at the 105% average rate. Three fresh repetitions, two-second intended injection
windows, followed by complete drain. A four-million-request cap may shorten the
window; the actual count/rate is recorded. These short finite runs characterize
transients and scheduling; they do not establish a stable overload policy or SLO.

Producers follow one absolute schedule and submit every ID even when late. Four
native generator threads enqueue without waiting for replies. Separate collectors
use `FuturesUnordered`, so one hot reply cannot impose FIFO observation delay on
cold replies. Record intended-arrival-to-handler-completion, caller-observed
latency, admission lag and deadline outcomes. Flag schedules with more than 1% of
requests over 1 ms late; do not rank those rows as equivalent offered traffic.

The two-second caller deadline drops only the reply receiver. The handler still
runs. FIFO fences then wait for all admitted work before stopping actors; counters
must agree with the submitted count. Report timeouts and late hot/cold replies
beside every tail: successful-only completion quantiles omit timed-out requests,
and observation tails include timeout events with censored completion times.
`replies_per_s` counts successful replies over the full drain interval; in-window
service and queue growth are separate metrics, not interchangeable capacity claims.

Sample queue depth, outstanding work, hot/cold backlog and process RSS every
100 ms. Snapshots are approximate across independent monotonic atomic counters;
short spikes can be missed. Sample vectors are preallocated and request counts
bounded, but mailbox backlog, pending-reply queues and observer futures still
consume memory and appear in RSS. Process peak CPU/RSS also includes startup,
warmup, sorting and teardown. Per-worker `/proc` scheduler/context-switch deltas
cover the timed work and drain. Zero unavailable counters are not evidence of zero
waiting; these fields do not measure migrations or hardware cache events.

## Profiling

Unprivileged perf is blocked (`perf_event_paranoid=4`); passwordless sudo is not
available. Do not change server security settings. The optional Linux-only
`profiling-experiments` feature uses [pprof-rs](https://github.com/tikv/pprof-rs)'s
199 Hz SIGPROF sampling with its recommended unwind blocklist. Save thread-prefixed
folded stacks and SVG flamegraphs. These are statistical user-space call-stack
samples, not kernel tracing, hardware counters, allocation profiles or off-CPU
causal attribution. Profile overhead and unwind/signal bias apply.

Profile ordinary Tokio and sharded turn-1/turn-16 at both worker budgets: existing
large CPU, hotspot and persistent TCP cases (three repetitions), plus same/cross
chain placement. Guards start after setup/warmup and stop after traffic/drain,
before sorting/rendering; small gate/fence overhead can be included. Profile rows
are never pooled into unprofiled throughput/latency medians. Allocation/wakeup or
reactor bottlenecks remain hypotheses until the stacks and scheduler counters
support a specific explanation; kernel-level attribution may remain unavailable.

## Reproduce

Build/test first, then run the campaign with no concurrent builds or plotting:

```sh
cargo test --locked --workspace --all-features
cargo bench --locked -p spawned-concurrency \
  --features backend-experiments,profiling-experiments \
  --bench workload_research --bench designs --no-run
# Point to the resulting workload_research executable for the behavioral checks.
SPAWNED_RESEARCH_BINARY=/path/to/workload_research \
  python3 -m unittest discover -s benchmarks -p test_research_driver.py
nice -n 10 taskset -c 2,3,4,5 python3 benchmarks/fede_research.py \
  --output /tmp/spawned-research-smoke --smoke
nice -n 10 taskset -c 2,3,4,5 python3 benchmarks/fede_research.py \
  --output /tmp/spawned-research-full
```

Use the recorded Rust compiler and benchmark debug-info flags. Each output path
must be new. Watchdogs kill the full child process group and retain diagnostics;
failed samples halt the campaign and are never assigned throughput values. Review
raw ranges, censored outcomes, generator lag and affinity before drawing conclusions.
A longer soak with churn and queue/RSS tracking follows this initial characterization.
