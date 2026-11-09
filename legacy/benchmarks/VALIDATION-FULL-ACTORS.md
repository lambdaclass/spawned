# Validation: full-actor backend study

Existing public actor/protocol signatures and Backend variants remain unchanged.
The new paths require `backend-experiments` and explicit private selectors. Native
prototype loops are not used for this matched comparison. Stable default backend
selection remains Tokio; the completion-status bug fix is independent.

[Validation logs and input hashes](validation/full-actors-2026-11-05/sha256.json)
and [agy reviews/resolution](reviews/agy-full-actors-2026-11-05.md) are archived.
Original functional validation corresponds to Rust source `1f58ba6`; native fede
validation additionally tested the shutdown fix at `22d3ad4`. The merged source
was checked again locally. Every performance sample uses historical source `9b22346`.

## Measurements

[Raw data and hashes](results/full-actors-2026-11-05/manifest.json): 1,575/1,575
fresh measured processes passed, validating 129,024,105 requests. Five repetitions
per cell. All six campaigns use clean source `9b22346`; each environment uses the same
recorded executable hash at its two worker budgets. CSV
archives normalize CRLF to LF without changing fields, retaining both hashes.

Measurements precede the later completion-getter and shutdown-publication fixes. The fixes have separate functional tests; no claim is made that the final branch
was performance-measured after them. Benchmark actors stop after request replies
have been collected; the shutdown-publication race specifically overlaps sending
with actor termination.

The inherited raw `capacity` field configures native prototype queues and has no
effect on these unbounded full-actor mailboxes. Likewise, the baseline's generic
`batch` field does not enable explicit turns; only the selected actor-tokio and
actor-sharded policies do so.

## Tests and characterized differences

- Workspace tests with no experimental selector pass, including all features.
- Shared-pool resource tests: finite blocking-worker saturation/release; warmed
  default async and sharded reactor reuse; dedicated-reactor startup failure;
  cold shard startup failure and recovery after child-only descriptor exhaustion.
- Actual actor contracts: nested spawning, helpers, TCP/timers, caller lifetime,
  same-shard async suspension, blocking isolation, queued work after stop,
  detached helper and blocking child after parent stop, monitor waiter after the
  last sharded actor stops.
- Manual-poll regression places cancellation exactly inside the batch-yield gap;
  idle-time accounting and empty-mailbox behavior have a separate regression.
- Pool tests cover explicit shutdown, registration leases, terminal close and
  rejection of worker-thread shutdown/self-join.
- Generated lifecycle, monitor, FIFO, request identity and race scenarios pass
  under the selected backend, including a single-worker fixed-turn control.
- Linux runs the resource and actor contracts in the same four-vCPU container
  before measurement. Native smoke covers all 49 CPU/timer/blocking/TCP cells.
- Latest formatting, Clippy/all-target/all-feature checks, dataset integrity,
  report links and image review are recorded with the report artifacts.

One inherited unit demonstration is deliberately excluded from the selected
serial characterization: `badly_behaved_thread_non_blocking` expects a blocking
actor to interfere with a counter. Round-robin placement can put them on different
shards. Its original assertion is unchanged. Deterministic same/different-shard
tests cover the actual behavior. The global selected two-worker pool also causes
three timing demonstrations to fail when otherwise independent unit tests run in
parallel. Those attempts are retained and disclosed, not counted as clean passes.
The default/unselected workspace suite passes. This is a research backend, not a
claim of complete application compatibility or production readiness.

A general status regression now tests closed watch completion in both task/thread
modes, async/blocking agreement, current-thread and nested-executor fast paths,
and preservation of a published Normal reason. The resource child asserts liveness
and exit status agree after failed startup, including when descriptors are full.

## Reproduce

Run functional checks on current corrected source. For the historical performance
comparison, run the Python benchmark commands below from a separate clean checkout
of `9b223463599ffb18615e0fc7f538924bd3edd8e7`. Record the new source/binary hashes
if instead measuring current main; do not label those results as this dataset.

```sh
cargo test --workspace --all-features
SPAWNED_EXPERIMENT_SCHEDULER=sharded SPAWNED_EXPERIMENT_WORKERS=2 SPAWNED_EXPERIMENT_BATCH=0 \
  cargo test -p spawned-concurrency --all-features -- --test-threads=1 --skip badly_behaved_thread_non_blocking
python3 benchmarks/design_experiments.py --output /tmp/full-actors --group messages \
  --variants async actor-tokio-1 actor-tokio-16 actor-tokio-adaptive actor-sharded-1 actor-sharded-16 actor-sharded-adaptive \
  --cases rpc1 active128_p128 active1024_p256 hotspot128 async_wait128 blocking128 tcp128 idle4 alloc1 \
  --workers 2 --repetitions 5
```

The four-worker control uses cases `active128_p128 active1024_p256 hotspot128
async_wait128 blocking128 tcp128`. Run every configuration in a fresh process;
do not mutate selectors while actors are alive. Stop/join actors and await
important helpers before the private terminal pool shutdown. `block_in_place`
is not supported inside a current-thread shard; external Context helpers retain
the runtime at their call site. Blocking work can outlive background shutdown.

The linux2/linux4 environment is an unprivileged OrbStack ARM64 container with CPUs 0–3, 8 GiB and 4,096 PIDs,
source mounted read-only, separate Cargo/output volumes and compiler 1.93. The
source and binary are recorded at build/start; no builds/tests/plotting run during
measured campaigns. CPU/RSS are whole-process values. Run ranges are descriptive,
not confidence intervals; the workloads are finite and predominantly closed-loop.

## Native Linux reproduction on fede, November 6

The fede2/fede4 campaigns add 525/525 successful samples (43,008,035 requests).
Host: AMD Ryzen 9 9950X3D, x86-64 Linux. The complete process tree used taskset CPUs
2–5, four distinct physical cores within one L3 group, and nice=10. Producers,
observers and TCP server threads share the same four CPUs; these are not four
exclusive actor cores. Other users, scheduling and frequency scaling remained
uncontrolled. [Host metadata](results/full-actors-2026-11-05/fede-host.json) records
CPU topology, the initial load/idle snapshot and compiler environment. Tests and
builds preceded measurements; no additional experiments ran concurrently.

The first default generated scenario run reported a request timeout during
shutdown. Its seed and failure log are retained. A local allocator-gated regression
deterministically reproduced a mailbox admission/publication race: receiver drop
could miss an in-flight request and retain its reply sender after actor completion.
[PR182](https://github.com/lambdaclass/spawned/pull/182) closes/drains/discards the
mailbox before publishing completion. The test fails before and passes after; it
now requires successful admission to prevent a stale allocation hook silently
passing. Agy approved the fix; Claude's admission-assertion request was applied.
Outgoing MonitorCleanup only cancels the exiting actor's own subscriptions; target
completion notifications still follow the drain. Forced runtime teardown and user
destructor panics retain their existing abnormal completion behavior.

After the fix, fede passed the workspace all-feature suite, the selected sharded
serial suite with the previously disclosed timing-demo exclusion, and 1,000
additional generated actor scenarios. Performance then ran from clean `9b22346`
for comparability, not from the corrected source. The archived reproduction script
records both revisions and the process-level settings; its cached Nix tool paths
are host-specific, not installation prerequisites for other machines.

The [final agy review](reviews/agy-fede-2026-11-06.md) approved the native Linux
interpretation and merged cleanup/turn forwarding without blockers. It reviewed
our aggregate summary, not an independent rerun. Dataset hashes, five-run cell
counts, identity of historical summary cells, report links and twelve embedded
figures were checked separately; the new throughput/latency figures were inspected
visually. The [shutdown review](reviews/agy-shutdown-publication-2026-11-06.md) is
also retained.
