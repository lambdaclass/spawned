# Native Linux graph, offered-load and profile validation

Measured source: clean `454d25a8baed62ee0a7b005e919d3d9e32f86893`.
Native Linux `fede-256gb`, shared Ryzen 9 9950X3D, nice=10. Rust 1.93.0,
Cargo 1.98.0, benchmark debug info enabled. Actor CPU mask 2–5, producers
6,7,14,15, collectors/sampler 8,9,16,17. Each role uses distinct physical cores;
workers migrate within the actor mask. Other users and frequency are uncontrolled.

## Functional gates

- Full workspace all-feature tests passed on the server before measurement.
- Selected sharded all-feature tests passed serially, excluding the inherited
  `badly_behaved_thread_non_blocking` test, as documented in earlier validation.
- Final source passed all-target/all-feature Clippy with warnings denied, using
  the available matched Clippy/Rust 1.98.1 toolchain.
- Benchmark behavior tests validated malformed arguments, exact graph outcomes,
  hot overload, timeout accounting, full drain and unordered cold observation.
- Watchdog tests validated child process-group cleanup and diagnostic retention.
- All 43 final smoke processes succeeded before the full campaign.

Earlier workspace/sharded logs cover the same library source. Final benchmark
corrections excluded extra graph fences from timing, synchronized the final queue
snapshot, and separated collectors from producers. Final behavior/Clippy/smoke
logs cover those corrected benchmark sources. No default library behavior or
public actor/protocol API changes in this report branch.

## Measured and archived evidence

- Main: 778/778 processes; confirmation: 224/224. Total: **1,002/1,002**.
- Phase counts: 12 reference/pilot, 280 short graph, 140 long graph, 504 offered,
  66 profiles. Timed unprofiled and profiled samples are never pooled.
- Both campaigns record the same source and SHA-256 executable hashes. The
  confirmation controller itself is archived and matches its recorded hash.
- Every original CSV field was compared byte-for-byte with the derived combined
  row; only `stem` gained a campaign prefix, plus two new identification columns.
- Every applicable `messages == succeeded + timeouts` identity was checked.
  The runner checks final admitted/completed counts and zero remaining work.
- All 2,040 archived input artifacts have SHA-256 entries. Original run metadata,
  CSVs, folded stacks/SVGs, scheduler counters, queue/RSS series and compressed
  subprocess diagnostics are retained. The Git source bundle depends on permanent
  main commit `b97de150862161644d07533a3c788156ff5f9d55` and preserves the measured
  branch even after squash merge.
- Fourteen figures rendered on fede; long-graph and overload plots visually
  inspected. Aggregate calculations checked against raw medians. A chart-unit
  correction converted observer lag from ns to ms; no measured values changed.

## Failed admission schedules remain evidence

**Only 260/504 offered processes passed** the predefined generator check; 244 had
more than 1% of requests admitted over 1 ms late. This is a major limit on offered
comparisons. Every affected cell is marked; do not interpret it as equal offered
load. All processes nonetheless validated IDs/outcomes and drained admitted work.

The first attempted calibration shared producers and collectors on four cores;
its six reference/pilot rows are retained separately under validation. It halted
before comparing backend policies. The corrected campaign used separate producer
and collector masks; it did not relax the validity threshold. A diagnostic result
is not silently discarded or relabeled as valid.

## Reproduction and interpretation

[Protocol](RESEARCH-FEDE-PROTOCOL.md) records workloads, routing, absolute rate
selection, caller deadline censoring, counters, profiles and commands. Build/test
before scoring; no builds or plotting overlapped scored main/confirmation runs.
[Findings](FINDINGS-FEDE-RESEARCH.md) keep the default decision conservative.

The optional pprof-rs SIGPROF sampler replaces unavailable unprivileged perf
(paranoid=4, no passwordless sudo); host settings were unchanged. Inclusive stack
ancestry is not exclusive CPU attribution. No hardware/off-CPU/migration claims.
No long-term stability claim follows from two-second injection or a ~one-second
graph window. Default Tokio stays supported; custom executors are a later,
separate opt-in research branch.
