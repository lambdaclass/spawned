## What to keep and what to investigate

**Keep Tokio as the supported default; investigate pinned parked shards for fixed
streaming pipelines.** The latest two workloads give different answers. Tokio
leads the many-actor recursive graph; fixed-core shards lead a new eight-stage
streaming pipeline. Spinning can buy more throughput in that pipeline but consumes
the assigned cores while idle. None of the private profiles is promoted to a
supported backend by this study.

The completed archive contains 5,744 successful benchmark processes in 45 separate
blocks. That includes smokes, independent profiles, historical prototypes with
known limitations and 17 five-minute lifecycle windows; it is not 5,744 comparable
timing repetitions. Exact source/flags/resource budgets stay separate.

### Corrected custom schedulers still trail Tokio in the large actor graph

V8 source `2cec0a6` includes the deque fairness correction. The displayed chain/cross
case uses 1,920 actors,256 logical producers on four caller runtimes,
1,048,576 roots, eight handlers/root and 100 synthetic iterations/handler, with
five fresh processes/cell. All actor workers share CPUs2–5. Native policies add
one I/O worker and an idle runtime owner; Tokio5 is an oversubscription control.

| Policy | Median roots/s | Fresh-process min–max roots/s | Median observed p99 ms |
|---|---:|---:|---:|
| Tokio4 | 885,316 | 825,759–893,454 | 0.533 |
| Tokio5, extra-worker control | 900,905 | 826,149–989,920 | 2.888 |
| Root4, wake precheck | 539,815 | 510,273–585,649 | 2.021 |
| Runnable4, joint Z1W1 | 577,898 | 531,302–596,944 | 2.303 |
| Deque4, baseline Z0W0 | 517,990 | 492,442–574,409 | 1.996 |
| Deque4, joint Z1W1 | 603,716 | 582,442–702,396 | 1.704 |

Joint callback/wake settings improve Deque's displayed median by 16.6% over its
own baseline, while Tokio remains ahead. This is a combined settings comparison,
not an isolated atomic/lock causal measurement or a significance claim.

The separate eight-core block uses 16,128 actors,1,024 logical producers,
2,097,152 roots and three repetitions. In the same graph case, Tokio8 reaches
1,399,374 roots/s with 1.569ms observed p99; joint Deque8 reaches731,672/s with
10.823ms, versus its baseline508,283/s and 14.908ms. Root and Runnable reach
589,997/s and 640,152/s. CPUs2–9 span96MiB and 32MiB L3 domains; this is an expanded
workload/resource budget, not homogeneous-core strong scaling. All ranges and
other graph cases remain in the appendices.

### Fixed-core streaming is a promising use case

V9 source `a8471f7` is an independent fixture: N sequential actor stages on N actor
cores, with either one or 16 pipelines. Every root does the same800 LCG iterations
in total and carries one end-to-end reply. Unlike the earlier graph, hops forward
one-way messages without awaiting an intermediate response. There are 256 logical
producers on four fixed driver cores10–13,1,048,576 roots and three fresh processes
per cell. Sharded workers are individually pinned before actor startup; actual
thread/CPU placement is audited. Tokio workers share the same actor CPU mask
without individual pinning. Sharding, batching and placement differ together.

The eight-core cells show a repeatable advantage within this synthetic fixture:

| Actors/core | Profile | Median roots/s | Fresh-process min–max roots/s | Median observed p99 ms | Idle process busy-core equivalents |
|---:|---|---:|---:|---:|---:|
| 1 | Tokio8 | 1,012,110 | 733,502–1,022,009 | 0.537 | 0.00 |
| 1 | Pinned parked | 1,877,006 | 1,788,421–1,946,703 | 0.185 | 0.00 |
| 1 | Pinned spinning | 2,105,494 | 1,852,212–2,135,724 | 0.180 | 8.00 |
| 16 | Tokio8 | 901,193 | 891,742–1,152,491 | 0.771 | 0.00 |
| 16 | Pinned parked | 2,038,631 | 1,956,345–2,143,444 | 0.415 | 0.00 |
| 16 | Pinned spinning | 2,403,235 | 2,390,952–2,404,185 | 0.134 | 8.01 |

One to four cores give mixed results; the full matrix and CPU/root values are
plotted below. For example, with one actor/core at four cores, Tokio's median is
1,226,625/s, parked shards1,162,851/s and spinning shards1,211,613/s. There is no
universal fixed-core win or evidence that the smallest actor population is best.

Idle CPU is measured for one second after warmup. Zero values mean no measured
process CPU-tick increase, not literally zero work or electrical power. Active
process CPU includes drivers and runtime threads. Pinned parked shards merit a
representative application trial before spending idle cores on spinning. N also
changes inter-stage message count; eight cores cross asymmetric L3 domains.
These results do not isolate affinity, explain a cache cause, or measure Silver.
This profile retains Tokio, boxed addressed mailboxes, ordinary owned messages
and replies; it does not implement Flux's broadcast/overwrite transport.

### Optimizations are workload-specific

V6 source `0f81b57`'s Root wake-precheck comparison improved the displayed four-core
chain/cross median from 517,754 to 596,682 roots/s; Tokio was943,042/s. This narrows a
cost hypothesis without establishing the precheck as an improvement everywhere.
V8 resource windows show why graph-only tuning is insufficient: joint Z1W1
Runnable and Deque complete fewer churn cycles than their Z0W0 counterparts in
their single 300-second windows. Those are finite lifecycle/resource checks with
one sample/configuration, not a repeatable throughput ranking.

V7 source `acd37a3` compared generic inline future storage with boxed storage.
Runnable's displayed four-core chain/cross rate changed622,911→629,457/s, and
its eight-core rate689,489→679,968/s. These small differences do not justify the
pin/drop complexity on throughput evidence. Inline storage remains on a research
branch and in a source bundle. Historical V5/V6/V7 Deque rows inherit the later
found fairness defect and are observations, not evidence of readiness.

V3 source `097ae02`'s clock study compares policies, not just clock cost. Its
count-only Tokio16 chain/cross case reached944,187/s and 0.465ms p99; a50µs turn
control reached863,051/s and 0.643ms. Yield counts were not recorded, so do not
attribute this specifically to slice expiration. Neither preempts a handler.

### Overload and lifecycle limit the conclusions

Fast cold replies can coexist with an ever-growing hot mailbox. V3's concentrated,
steady500k/s sharded16 case had three valid schedules: median cold other-cohort
observed p99 was0.011ms while median peak hot queue depth was410,074 and in-window
queue growth about 202k messages/s. It is a finite two-second injection followed
by complete drain, not sustainable capacity.

Strict admission validity passed316/432 V3 schedules,177/252 V4 schedules and
130/176 V8 schedules. The rest remain diagnostics; invalid or mixed-validity
aggregates cannot support equivalent-offered-load rankings. Completion quantiles
omit timed-out responses; observed misses, timeouts and admission lag remain
visible alongside tails. Unbounded mailboxes still permit memory exhaustion.

All17 completed five-minute resource configurations pass their tracked
starts/stops/drops, stateful requests and monitor assertions, followed by terminal
shutdown. V8's five windows each completed between 6.09M and 12.49M tracked actor
lifecycles. Their sampled RSS maxima were10.8–12.1MB. These whole-process samples
are neither isolated actor allocations nor proof of leak freedom. Last samples
precede terminal runtime shutdown and include still-live infrastructure. Resource
plots retain warmup; the invalid paused-reader and nested-nice attempts are
archived separately as harness errors.

### Recommendations by use case

| Use case | Choice / next step | Status |
|---|---|---|
| Dynamic actors, general async integration, many-actor recursive requests | Existing Tokio async backend | Supported default; strongest current large-graph baseline |
| Persistent blocking actor work | Existing blocking/dedicated-thread modes, sized for their worker/thread costs | Supported; one persistent blocking actor occupies one pool worker |
| Fixed CPU-bound streaming pipeline with known placement | Trial pinned parked shards in a client workload, including stage imbalance and I/O | Private candidate; encouraging eight-core fixture, mixed smaller-core results |
| Fixed pipeline with dedicated spare capacity and strict measured latency need | Compare spin against parked with explicit CPU budget | Experiment; consumes assigned cores while idle |
| Custom Root/Runnable/Deque scheduling | Retain for targeted cost/compatibility research | Private experiments; no demonstrated general replacement |
| Generic inline futures, old queue policies and engine-only transports | Preserve exact sources and negative results | Research archive / comparison controls |

No API, owned-message, addressed delivery, cooperative lifecycle or request/reply
philosophy change is required. Deterministic idle-stop regressions did produce a
production fix in [PR185](https://github.com/lambdaclass/spawned/pull/185). Wake
publication, idle-shutdown and deque fairness defects were also found and fixed.
Finite tests and reviews do not prove every possible schedule correct.

The next valuable evidence is a representative fixed pipeline versus the current
Tokio application, including idle/wake bursts, stage imbalance, real payloads and
I/O. That can establish whether the V9 benefit survives the synthetic fixture and
justify a small supported configuration surface. It does not justify removing
Tokio or exposing every prototype as a permanent public backend.
