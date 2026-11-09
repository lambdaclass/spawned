# Legacy

Historical designs, reviews, research notes and raw experiment data, kept for
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
| [`docs/research/`](docs/research) | Research notes: framework ideas, verification strategy, Commonware, Silver/flux, concurrency ideas, full-actor shards, and the former ranked backlog (`idea-priorities.md`, now folded into the [roadmap](../ROADMAP.md#ranked-backlog)) | `docs/research/` |
| [`docs/reviews/`](docs/reviews) | Implementation and security reviews with their evidence | `docs/reviews/` |
| [`docs/releases/`](docs/releases) | v0.6.0 release notes and the v0.6.1 patch draft (release history now lives in the [CHANGELOG](../CHANGELOG.md)) | `docs/releases/` |
| [`docs/design/`](docs/design) | v0.5 API redesign alternatives, framework comparison and superseded roadmap plans | `docs/design/` |
| [`docs/BACKENDS.md`](docs/BACKENDS.md) | Backend guide with pinned-profile measurements (usage now in the [guide](../docs/GUIDE.md#execution-modes-and-backends)) | `docs/BACKENDS.md` |
| [`examples/planned/`](examples/planned) | Former planned-example catalog (now the roadmap's [examples to add](../ROADMAP.md#examples-to-add)) | `examples/planned/` |
| [`benchmarks/`](benchmarks) | Benchmark protocols, findings, validation notes, reviews, rendered reports, raw results and the baseline methodology | `benchmarks/*.md`, `benchmarks/{reports,reviews,results,validation}/` |
| [`research/`](research) | Raw experiment data, scripts and reports: backend experiments, client observations, framework-metrics overhead and reliability campaigns | `research/` |

Some scripts in `benchmarks/` read findings files from `legacy/benchmarks/` and
`legacy/research/` when re-rendering archived reports.
