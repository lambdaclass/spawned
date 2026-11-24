# Benchmarks

Process-isolated release benchmarks for the existing backends. These measure
framework behavior and its limits; they are not client-level results and do not
rank backends for every workload. Tokio (`Backend::Async`) stays the supported
default; see [execution modes and backends](../docs/GUIDE.md#execution-modes-and-backends).

## Run the baseline

Run from the repository root with the Rust version in `.tool-versions`:

```sh
python3 benchmarks/run.py --output /tmp/spawned-baseline --repetitions 5
```

The runner builds the optimized `baseline` bench executable once, randomizes the
case/backend/repetition schedule, and launches each sample in a fresh process.
It saves configuration, revision, compiler, source/binary hashes, raw CSV,
summary CSV, build output and per-process logs. Output paths must be new
directories. Each sample has a 90-second process watchdog; a failed sample fails
the campaign and partial raw data and logs remain. Do not compare incomplete
campaigns. The bench's raw requests are unbounded to match 0.7, so invoke the
binary through this runner or an external process timeout. Per-request timers
would change the operation being compared. `--cases` and `--backends` select
smaller runs. Run a small smoke campaign across every backend and scenario
before a full one.

Cases: `rpc_single`, `rpc_parallel`, `mostly_idle`, `burst`,
`large_shared_payload`, `cpu_paced`/`cpu_overload`, plus timing-off and
allocation-counting controls. They measure request/reply over Spawned async,
blocking-pool, dedicated-thread and synchronous-thread actors and a plain Tokio
mpsc + oneshot loop. Payloads are shared `Arc<[u8]>`; mailboxes and collectors
are unbounded. Scheduling, affinity, frequency scaling and other machine load are
uncontrolled. The summary reports the median and min/max of per-process values;
a median of per-run p99 values is not a pooled p99. Full case and measurement
column definitions are in the archived
[baseline methodology](https://github.com/lambdaclass/spawned/blob/efba24340a01df82f872ec422b9dbaf48f897b04/legacy/benchmarks/BASELINE-METHODOLOGY.md).

Runner self-tests (descendant cleanup and diagnostic retention):

```sh
python3 -m unittest discover -s benchmarks -p test_runner.py
```

## Hot-path regression checks

`hotpath.py` compares a revision against its base on one shared driver
(`hotpath/driver.rs`: async burst, `request()` sequential and concurrent,
`send` with one and two producers, and the threads-mode equivalents):

```sh
python3 benchmarks/hotpath.py gate                       # instruction counts (callgrind), CI
python3 benchmarks/hotpath.py abba --cores 12-15 --reps 30  # wall clock, before a release
```

`gate` is the `perf-gate` CI check: it fails when instructions per operation
grow more than 3% over the merge base, or more than 3% over the last release
tag (`--release`; the cumulative budget per release cycle, so small per-PR
regressions cannot add up). `abba` alternates base and head on the
same cores and reports the median and spread. See
[performance gate](../docs/TESTING.md#performance-gate).

## Client qualification

`qualification/` runs a client built on a spawned candidate next to the
client's `main` on one host, for a day or two on a real network, and judges
it against fixed thresholds. It is a release gate (a 24 h paired ethlambda run
and a 48 h ethrex memory run per release); manual only, never scheduled.

| File | Role |
|---|---|
| `qualify.py` | CLI: `init`, `build` (clone, build, single-spawned check), `launch` (preflight, detached controller), `run`, `status`, `stop` (bounded), `analyze`, `check-binary` |
| `clients.py` | per-client ports, command lines, probes and fault hooks (ethlambda; ethrex with its Lighthouse) |
| `sampler.py` | CPU, RSS, fds, threads, head/sync, peers, datadir, spawned metrics, host RAM |
| `analyze.py` | `results/SUMMARY.md` and `verdict.json`: pass/fail checks, per-node tables, shutdowns, faults, supervision events, heap-growth attribution |
| `node.sh` | runs one node with a per-run log and PID file |
| `patches/ethrex-jemalloc-stats.patch` | optional ethrex patch: jemalloc counters and raw dumps for `HEAP_PROFILE=1` |

```sh
D=~/spawned-work/qual-ethlambda-<release>
python3 benchmarks/qualification/qualify.py init $D PRESET=ethlambda-paired-24h \
  CANDIDATE_REF=pull/783/head SPAWNED_REV=<commit> BUILD_ENV=$D/build.env
python3 benchmarks/qualification/qualify.py build $D
python3 benchmarks/qualification/qualify.py launch $D
python3 benchmarks/qualification/qualify.py check-binary <binary> --lock Cargo.lock
```

The plan, the settings, the thresholds and how to read the summary are in
[client qualification](../docs/TESTING.md#client-qualification).

## Removed experiments

The backend, I/O and profiling experiments (the `benchmarks/experiments`
crate and its patch, the reliability campaign runner, the `fede_*` research
drivers and the report renderers) settled their questions and were removed.
They are at commit `bf59ace0`, the last commit that has them; the campaign data
they were last run on is at
[`legacy/benchmarks/results`](https://github.com/lambdaclass/spawned/tree/efba24340a01df82f872ec422b9dbaf48f897b04/legacy/benchmarks/results)
(`efba2434`).

## Conclusions

The archived campaigns support these conclusions. They are synthetic results on
specific hosts; do not transfer the ratios to a client.

- **Keep Tokio as the default.** On the large recursive actor graph (1,920
  actors, four actor cores), Tokio reached a median 885k roots/s; the corrected
  custom Root, Runnable and Deque schedulers reached 518k–604k across their
  settings. On eight cores Tokio led 1.40M to 732k.
- **Pinned parked shards suit fixed streaming pipelines.** On an eight-stage,
  eight-core pipeline with one actor per core, pinned parked shards reached a
  median 1.88M roots/s against Tokio's 1.01M (p99 0.19 ms against 0.54 ms).
  Spinning shards reached 2.11M but kept all eight cores busy while idle. At one
  to four cores the results were mixed, so this is not a universal win. Parked
  pinned shards became the public pinned execution group.
- **Size persistent backends explicitly.** `Backend::Blocking` with 1,024 actors
  exhausted Tokio's default 512-thread blocking pool, and per-actor runtimes hit
  a 4,096 descriptor limit at 1,024 actors; see the
  [guide](../docs/GUIDE.md#execution-modes-and-backends).

Sources: [backend study findings](https://github.com/lambdaclass/spawned/blob/efba24340a01df82f872ec422b9dbaf48f897b04/legacy/research/backend-experiments/2026-11-06-fede/FINDINGS.md)
and [design findings](https://github.com/lambdaclass/spawned/blob/efba24340a01df82f872ec422b9dbaf48f897b04/legacy/benchmarks/DESIGN-FINDINGS.md).

## Archived reports and protocols

Measurement definitions, protocols, findings, validation notes, reviews, rendered
reports and raw results from earlier campaigns are archived (not maintained):

- [Initial baseline report](https://github.com/lambdaclass/spawned/blob/efba24340a01df82f872ec422b9dbaf48f897b04/legacy/benchmarks/REPORT-2026-11-05.md)
- [Backend design report](https://github.com/lambdaclass/spawned/blob/efba24340a01df82f872ec422b9dbaf48f897b04/legacy/benchmarks/reports/designs-2026-11-05/REPORT.md),
  [findings](https://github.com/lambdaclass/spawned/blob/efba24340a01df82f872ec422b9dbaf48f897b04/legacy/benchmarks/DESIGN-FINDINGS.md) and
  [implementation/test guide](https://github.com/lambdaclass/spawned/blob/efba24340a01df82f872ec422b9dbaf48f897b04/legacy/benchmarks/EXPERIMENTS.md)
- [Full-actor report](https://github.com/lambdaclass/spawned/blob/efba24340a01df82f872ec422b9dbaf48f897b04/legacy/benchmarks/reports/full-actors-2026-11-05/REPORT.md) and
  [findings](https://github.com/lambdaclass/spawned/blob/efba24340a01df82f872ec422b9dbaf48f897b04/legacy/benchmarks/FINDINGS-FULL-ACTORS.md)
- [Native Linux graph, overload and profiling report](https://github.com/lambdaclass/spawned/blob/efba24340a01df82f872ec422b9dbaf48f897b04/legacy/benchmarks/reports/fede-research-2026-11-06/REPORT.md)
  and [protocol](https://github.com/lambdaclass/spawned/blob/efba24340a01df82f872ec422b9dbaf48f897b04/legacy/benchmarks/RESEARCH-FEDE-PROTOCOL.md)
- [Backend research index](https://github.com/lambdaclass/spawned/blob/efba24340a01df82f872ec422b9dbaf48f897b04/legacy/research/backend-experiments/README.md)
- [Everything else in the archived `legacy/` directory](https://github.com/lambdaclass/spawned/tree/efba24340a01df82f872ec422b9dbaf48f897b04/legacy)
