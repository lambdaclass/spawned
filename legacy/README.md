# Legacy

Historical designs, reviews, research notes and experiment data, kept for
reference; not maintained; may be deleted or moved to an archive repo later.

Nothing here describes current behavior or plans. For those, use the live
documents linked from the [README](../README.md#documentation): the
[guide](../docs/GUIDE.md), [roadmap](../ROADMAP.md),
[changelog](../CHANGELOG.md) and [testing](../docs/TESTING.md). Links inside these
files were repaired where cheap when they moved; some may still point at old
locations.

The layout mirrors where each file used to live:

| Path | Contents | Previously |
|---|---|---|
| [`docs/research/`](docs/research) | Research notes: framework ideas, verification strategy, Commonware, Silver/flux, concurrency ideas and full-actor shards | `docs/research/` |
| [`docs/reviews/`](docs/reviews) | Implementation and security reviews with their evidence | `docs/reviews/` |
| [`docs/releases/`](docs/releases) | The v0.6.1 patch draft (release history lives in the [CHANGELOG](../CHANGELOG.md)) | `docs/releases/` |
| [`docs/design/`](docs/design) | v0.5 API alternatives, framework comparison and the superseded 2026-11-06 roadmap acceptance criteria | `docs/design/` |
| [`docs/BACKENDS.md`](docs/BACKENDS.md) | Backend guide with pinned-profile measurements (usage now in the [guide](../docs/GUIDE.md#execution-modes-and-backends)) | `docs/BACKENDS.md` |
| [`benchmarks/`](benchmarks) | Benchmark protocols, findings, validation notes, reviews, rendered Markdown reports, raw results of the 2026-11-05 campaigns and the baseline methodology | `benchmarks/*.md`, `benchmarks/{reports,reviews,results,validation}/` |
| [`research/`](research) | Experiment scripts, reports, findings and the smaller raw datasets: backend experiments, client observations, framework-metrics overhead and reliability campaigns | `research/` |
| [`verification/evidence/`](verification/evidence) | Historical v0.7 model, test and review logs; not live CI inputs | `verification/evidence/` |

The v0.7 reviews are archived in `docs/reviews/`. Host-specific one-off driver
scripts were removed rather than presented as portable verification commands;
their original versions remain in Git history at `8dc2e81e`. Use the current
[testing guide](../docs/TESTING.md) and CI workflows to reproduce checks. Raw logs
retain historical host paths as provenance. Archive checksum lists omit the
removed drivers; the remaining raw log bytes are unchanged.

Some scripts in `benchmarks/` read findings files from `legacy/benchmarks/` and
`legacy/research/` when re-rendering archived reports.

## Promoted and removed (2026-11-09)

Durable conclusions were promoted into the live documents: backend sizing
warnings and why `Backend::Thread` does not use a current-thread runtime
([guide](../docs/GUIDE.md#execution-modes-and-backends)), benchmark conclusions
([benchmarks README](../benchmarks/README.md#conclusions)), the non-goals from
the Silver/flux study ([roadmap](../ROADMAP.md#non-goals)), the next lifecycle
model's properties ([TLA+ README](../verification/tla/README.md#next-model)) and
the bugs covered by the PR #166 and #168 regressions
([testing](../docs/TESTING.md#bugs-reproduced-by-these-regressions)).

The following were then deleted. Everything is recoverable from git history at
commit `5273a5fa`, the parent of the removal (`git checkout 5273a5fa -- legacy/<path>`):

| Removed path | Size | Why |
|---|---:|---|
| `research/backend-experiments/2026-11-06-fede/data/` | 250 MB | Raw per-block CSVs, time series, profiles, diagnostics tarballs and source bundles; conclusions remain in that study's `FINDINGS.md` and `report/REPORT.md` |
| `research/backend-experiments/2026-11-06-fede/report/REPORT.html` | 17 MB | Generated duplicate of `REPORT.md` with embedded charts |
| `benchmarks/results/fede-research-2026-11-06/` | 36 MB | Raw results, profiles and source bundle behind `reports/fede-research-2026-11-06/REPORT.md` |
| `benchmarks/reports/*/REPORT.html` | 11 MB | Generated duplicates of each `REPORT.md` |
| `research/client-observations/2026-11-06-ethlambda-ab/server-probe/`: `evidence/`, `concurrent/evidence/`, `concurrent/longer-sparse/`, `one-worker/evidence/` | 46 MB | Raw probe samples and logs; READMEs, scripts and summary reports remain |
| `docs/research/idea-priorities.md` | small | Folded into the [ranked backlog](../ROADMAP.md#ranked-backlog) |
| `docs/releases/v0.6.0.md` | small | Superseded by the [CHANGELOG](../CHANGELOG.md) |
| `docs/design/roadmap-before-observability.md`, `docs/design/API_REDESIGN.md` | small | Superseded plans |
| `examples/planned/README.md` | small | Now the roadmap's [examples to add](../ROADMAP.md#examples-to-add) |

Links inside archived reports that pointed into the removed raw data no longer
resolve in the tree; restore the data from that commit to follow them.
