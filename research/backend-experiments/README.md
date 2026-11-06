# Backend experiments

Use the [backend usage guide](../../docs/BACKENDS.md) to choose supported modes.
Research profiles are private experiments; they are not permanent public backend
choices. Tokio remains the supported default. Ordinary owned messages, addressed
mailboxes, typed replies and cooperative lifecycle behavior remain the model.

## Completed studies

| Study | Evidence | Read first |
|---|---|---|
| [Shared versus split runtimes](2026-11-06-runtime-isolation/README.md) | 32 fresh processes, equal four-core budget, direct and relayed typed requests | [Results](2026-11-06-runtime-isolation/report/REPORT.md), [client audit](2026-11-06-runtime-isolation/CLIENT-WORKLOADS.md) |
| [fede scheduler and pinned pipeline, 6 November 2026](2026-11-06-fede/README.md) | 5,744 successful processes, 45 separate blocks, 51 figures, 144 profiles, 17 five-minute lifecycle windows | [Findings and recommendations](2026-11-06-fede/FINDINGS.md) |
| [Initial native Linux graphs/profiling](../../benchmarks/reports/fede-research-2026-11-06/REPORT.md) | 1,002 processes; earlier source and workload controls | [Findings](../../benchmarks/FINDINGS-FEDE-RESEARCH.md) |
| [Matched full actors across hosts](../../benchmarks/reports/full-actors-2026-11-05/REPORT.md) | Existing async, batching and sharding with full actor behavior | [Findings](../../benchmarks/FINDINGS-FULL-ACTORS.md) |
| [Backend and transport designs](../../benchmarks/reports/designs-2026-11-05/REPORT.md) | Dedicated threads, shared runtimes, native transports and I/O-engine experiments | [Findings](../../benchmarks/DESIGN-FINDINGS.md) |

Earlier studies keep their original paths for reproducibility. The new study
stores its analysis, graphs, inputs and review evidence together below this
folder. Benchmark executables and reusable runners remain in `benchmarks/`.
Do not pool different sources, workload shapes, host budgets or instrumentation.
