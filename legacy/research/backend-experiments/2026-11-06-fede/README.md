# fede backend study — 6 November 2026

**Keep Tokio as the supported default. Investigate pinned parked shards for fixed
streaming pipelines.** Spinning can help a particular pipeline but consumes the
assigned cores while idle. The synthetic results do not establish a client
speedup or change public actor APIs.

Start with [findings and use-case recommendations](FINDINGS.md), then the
[full report](report/REPORT.md) or [HTML report with embedded charts](report/REPORT.html).
Download the HTML to open it locally; profile and evidence links also need this
folder's data tree. There are 51 figures, 144 independent CPU profiles and 5,744
successful processes in 45 separate blocks, including smokes and historical
prototypes with known limitations. Seventeen resource configurations ran for
five minutes each. These are separate evidence blocks, not one timing sample.

## Contents

| Path | Contents |
|---|---|
| [FINDINGS.md](FINDINGS.md) | Main results, source-specific tables, tradeoffs and use-case recommendations |
| [report/](report/REPORT.md) | Full analysis, 51 PNG/SVG figures, summaries, resource tables and profile leaf counts |
| [data/](data/metadata.json) | Original per-block CSV/metadata, combined raw rows, time series, profiles, diagnostics, source bundles and SHA 256 manifest |
| [PROTOCOL.md](PROTOCOL.md) | Plans, source boundaries, CPU masks, validity criteria, exclusions and limitations |
| [CORRECTNESS.md](CORRECTNESS.md) | Discovered bugs, actual negative controls and reproduction commands |
| [PINNED-PROFILES.md](PINNED-PROFILES.md) | Silver-inspired parked/spinning semantics and exact experimental usage |
| [SCHEDULER-RESEARCH.md](SCHEDULER-RESEARCH.md) | Primary-source design research, tested ideas and next questions |
| [reviews/AGY.md](reviews/AGY.md) | Independent review scope, conclusions and editorial qualifications |

## Backend ideas examined

| Design | What it tests | Disposition |
|---|---|---|
| Existing Tokio async | Shared work-stealing runtime and ordinary actor tasks | Supported default and reference |
| Tokio turns / shared runtime ownership | Batching and process-owned runtime resources | Private candidate/control; policies have distinct fairness/resource tradeoffs |
| Static current-thread shards | Assign many actors to a fixed set of Tokio runtimes | Private candidate; evaluate placement and overload behavior |
| Native Global / Local / Steal | Custom gates and centralized/split ready queues | Research controls; source-specific correctness defects are disclosed |
| Native Root | Amortize Tokio context entry using one multiplexing root future per worker | Private experiment; keeps Tokio I/O and cooperative machinery |
| Native Runnable | Use async-task scheduling tickets with guarded future disposal | Private experiment; task/queue lifecycle still requires substantial validation |
| Native Deque | Owner-local Crossbeam queues, injector and stealing | Private experiment; fairness correction is source-matched in V8 |
| Z/W switches | Metadata callback and idle-peer wake precheck | Workload-specific combined/controlled experiments, not universal optimizations |
| Generic inline futures | Change future storage/allocation and monomorphization | Archive; no convincing throughput benefit to justify complexity |
| Pinned park / spin | Fixed-core Tokio shards with one or 16 actors/core | V9 candidate profiles; parked merits application testing, spin needs a CPU budget |

The earlier design study retains io_uring/epoll/kqueue and native transport
controls. Those are not equivalent to these full actor workloads. No matched
benchmark against Silver was run; the pinned profile borrows fixed-core placement,
not Flux's broadcast, Copy-only payload or overwrite semantics.

The raw dataset is deliberately retained as requested, with a hash manifest and
original diagnostics. Generated artifacts are marked in GitHub diffs. This is a
bounded research snapshot, not application source or a supported feature set.
Plot versions are pinned in [requirements-research.txt](../../../benchmarks/requirements-research.txt);
the recorded analysis orchestration files are in [orchestration/](orchestration/).
See the [transcript index](reviews/INDEX.md) for failed or incomplete review attempts.

## Rebuild the report

Run analysis on fede with the recorded plotting environment. From the repository
root, after verifying the data manifest:

```sh
python benchmarks/render_fede_overnight.py \
  --data research/backend-experiments/2026-11-06-fede/data \
  --output research/backend-experiments/2026-11-06-fede/report \
  --findings research/backend-experiments/2026-11-06-fede/FINDINGS.md
```

Use the archived controller, environment, gate logs and exact source bundles to
rerun measurements, following the protocol. Measured V9 source is a8471f7;
post-measurement retained source 20260cd is separately gated and not substituted
into the recorded timings. CPU IDs and asymmetric cache domains are host-specific.
