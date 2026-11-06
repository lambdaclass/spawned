# Spawned overnight backend experiments on fede256

5,744 successful fresh processes archived in 45 completed blocks. Original source revisions, binary hashes, exact schedules and inputs remain separate. **Tokio remains the default; these are private research backends.**

[Machine-readable summary](SUMMARY.csv) · [Raw inputs and hashes](../data/metadata.json) · [Experiment protocol](../PROTOCOL.md) · [Correctness findings](../CORRECTNESS.md)


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
case uses 1,920 actors, 256 logical producers on four caller runtimes,
1,048,576 roots, eight handlers/root and 100 synthetic iterations/handler, with
five fresh processes/cell. All actor workers share CPUs 2–5. Native policies add
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

The separate eight-core block uses 16,128 actors, 1,024 logical producers,
2,097,152 roots and three repetitions. In the same graph case, Tokio8 reaches
1,399,374 roots/s with 1.569 ms observed p99; joint Deque8 reaches 731,672/s with
10.823 ms, versus its baseline 508,283/s and 14.908 ms. Root and Runnable reach
589,997/s and 640,152/s. CPUs 2–9 span 96 MiB and 32 MiB L3 domains; this is an expanded
workload/resource budget, not homogeneous-core strong scaling. All ranges and
other graph cases remain in the appendices.

### Fixed-core streaming is a promising use case

V9 source `a8471f7` is an independent fixture: N sequential actor stages on N actor
cores, with either one or 16 pipelines. Every root does the same 800 LCG iterations
in total and carries one end-to-end reply. Unlike the earlier graph, hops forward
one-way messages without awaiting an intermediate response. There are 256 logical
producers on four fixed driver cores 10–13,1,048,576 roots and three fresh processes
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
1,226,625/s, parked shards 1,162,851/s and spinning shards 1,211,613/s. There is no
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
chain/cross median from 517,754 to 596,682 roots/s; Tokio was 943,042/s. This narrows a
cost hypothesis without establishing the precheck as an improvement everywhere.
V8 resource windows show why graph-only tuning is insufficient: joint Z1W1
Runnable and Deque complete fewer churn cycles than their Z0W0 counterparts in
their single 300-second windows. Those are finite lifecycle/resource checks with
one sample/configuration, not a repeatable throughput ranking.

V7 source `acd37a3` compared generic inline future storage with boxed storage.
Runnable's displayed four-core chain/cross rate changed 622,911→629,457/s, and
its eight-core rate 689,489→679,968/s. These small differences do not justify the
pin/drop complexity on throughput evidence. Inline storage remains on a research
branch and in a source bundle. Historical V5/V6/V7 Deque rows inherit the later
found fairness defect and are observations, not evidence of readiness.

V3 source `097ae02`'s clock study compares policies, not just clock cost. Its
count-only Tokio16 chain/cross case reached 944,187/s and 0.465 ms p99; a 50 µs turn
control reached 863,051/s and 0.643 ms. Yield counts were not recorded, so do not
attribute this specifically to slice expiration. Neither preempts a handler.

### Overload and lifecycle limit the conclusions

Fast cold replies can coexist with an ever-growing hot mailbox. V3's concentrated,
steady 500k/s sharded 16 case had three valid schedules: median cold other-cohort
observed p99 was 0.011 ms while median peak hot queue depth was 410,074 and in-window
queue growth about 202k messages/s. It is a finite two-second injection followed
by complete drain, not sustainable capacity.

Strict admission validity passed 316/432 V3 schedules, 177/252 V4 schedules and
130/176 V8 schedules. The rest remain diagnostics; invalid or mixed-validity
aggregates cannot support equivalent-offered-load rankings. Completion quantiles
omit timed-out responses; observed misses, timeouts and admission lag remain
visible alongside tails. Unbounded mailboxes still permit memory exhaustion.

All 17 completed five-minute resource configurations pass their tracked
starts/stops/drops, stateful requests and monitor assertions, followed by terminal
shutdown. V8's five windows each completed between 6.09M and 12.49M tracked actor
lifecycles. Their sampled RSS maxima were 10.8–12.1 MB. These whole-process samples
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
production fix in [PR 185](https://github.com/lambdaclass/spawned/pull/185). Wake
publication, idle-shutdown and deque fairness defects were also found and fixed.
Finite tests and reviews do not prove every possible schedule correct.

The next valuable evidence is a representative fixed pipeline versus the current
Tokio application, including idle/wake bursts, stage imbalance, real payloads and
I/O. That can establish whether the V9 benefit survives the synthetic fixture and
justify a small supported configuration surface. It does not justify removing
Tokio or exposing every prototype as a permanent public backend.


## What each backend does

| Backend | Scheduling / execution | Main tradeoff |
|---|---|---|
| async | Existing Tokio multi-thread runtime; ordinary actor task and mailbox | Mature scheduler and compatibility; reference |
| tokio-16 | Same Tokio scheduling, explicit 16-message / 50 µs cooperative turns | Yield checks trade throughput for fairness; no running-handler preemption |
| tokio-pool | Process-owned Tokio runtime with fixed worker count | Separates runtime ownership from actor scheduling |
| sharded-16 | Actor assigned to one single-thread runtime; 16-message / 50 µs turns | Locality helps colocated traffic; remote graph edges and hotspots can hurt |
| native-global | Custom actor jobs on a shared FIFO guarded by one mutex; Tokio I/O bridge per job poll | Simple work sharing; queue contention and runtime-entry cost |
| native-local | Local LIFO with periodic oldest stealing, all queues under one mutex | Local continuation preference; shared lock remains |
| native-steal | Separate mutex/deque per worker, local LIFO and oldest stealing, park/unpark | Less queue contention; still custom job/refcount and per-poll bridge overhead |
| native-root | Same split queues, direct job polls inside one root future per native worker; yield root after a configurable poll quantum (default 8) | Amortizes runtime entry; still depends on Tokio for root driving and shared I/O |

Native-runnable replaces the custom job Gate/future mutex with async-task 4.7.1 Runnable tickets, retaining a boxed actor future and separate poll/disposal unwind boundaries. Native-deque adds Crossbeam 0.8.8 owner deques, shared outside injection and batch stealing. The latter changes queue primitive and scheduling policy together, so improvements cannot isolate mutex cost. Both reuse the root driver/shared Tokio I/O; these are not Tokio-free runtimes.

Figure key: N = actor scheduler workers, R1 = one additional shared I/O worker (idle owner/helpers are additional threads), Q = job polls per outer-driver turn, Z = zero-sized metadata schedule callback, W = sleeping-peer precheck, I = generic inline future storage. One root reply covers eight handler invocations; roots/s is not send-only messages/s. For example, 16,128 actors share eight actor worker threads in the scale block; there is no OS thread per actor in these scheduler experiments.

All keep addressed, unbounded private mailboxes, boxed ordinary Rust messages and request/reply. No lossy broadcast ring, Copy-only restriction, public Actor/Handler/Backend API change or default promotion. Native actor jobs are not Tokio tasks. Native-root uses a current-thread root driver per native worker but no additional OS thread for that driver; block_in_place is unsupported and actor task IDs are absent.

## Correctness discoveries and source boundaries

| Version | Scope | Known limitation / disposition |
|---|---|---|
| V1 07f0e11 | Initial global/local native queues; 72 smokes + 360 graph processes | Stronger Loom publication model later failed: a coalesced wake did not publish prior writes. Remaining stages cancelled. Historical observations only. |
| V2 eefc302 | Publication corrected; split queues, caller/counter factorial; 728 processes | Review later found a possible Global idle-shutdown lost notify. Remaining stages cancelled after callers. Historical observations only. |
| V3 097ae02 | Corrected shutdown predicate + serialized teardown; adds amortized root driver | Separate common-source controls and gates; not proof of a supported production executor. |

| V4 09dd194 | Root/Runnable/quanta on stop-corrected source | Offered/resource evidence separate; unstarted 480-case scale sweep deferred for the fairness correction. |
| V5 329006e | Crossbeam owner-deque transport/policy | Review found possible local-oldest starvation; unstarted follow-ups cancelled. |
| V6 0f81b57 | Metadata callback and wake-precheck factorial | Deque retains V5 fairness limitation; Root/Runnable unaffected. |
| V7 acd37a3 | Generic pinned inline/boxed future pairs | Deque retains fairness limitation; inline throughput did not justify complexity. |
| V8 2cec0a6 | Alternating oldest/injection priority, safe hint fallback, concurrent conservation tests | Corrected frozen source; bounded tests remain different from formal/production guarantees. |
| V9 a8471f7 | Fixed-core parked/spinning Tokio shards and constant-compute streaming pipeline | One or 16 actors/core, 1/2/4/8 cores; independent fixture and CPU cost, no Flux transport or support promotion. |
The publication fix requires a release RMW even when wake state remains QUEUED or RUNNING|NOTIFIED. Queue-count-only Loom tests missed this visibility defect. The Global shutdown fix changes its closed predicate under the same mutex as Condvar wait. Concurrent shutdown callers serialize teardown. Older timings are not pooled into V3 medians.

Later PR 187 review found a Deque fairness defect: every periodic oldest turn prioritized injection, allowing sustained external arrivals plus hot local continuations to starve the local bottom. V5/V6/V7 Deque observations remain historical; Root/Runnable are unaffected. Unstarted V5 and V7 optimized follow-ups were cancelled before scoring. V8 (2cec0a6; initial corrected preflight e633e59) alternates local-oldest/injection priority, adds owner-pop fallback after bounded retries and validates TLS worker identity. The strengthened regression fails on old 95b4ca6 and passes on the correction; a preliminary probe missing hot local continuations had passed and is retained as a test-design limitation. No formal hard-latency fairness guarantee follows.

V7 pairs boxed/dynamic actor future storage (I0) with a generic pinned inline future (I1) inside async-task. It uses safe pin projection and guarded in-place disposal, retaining ordinary message/envelope/reply allocations. The comparison changes layout/allocation and monomorphization together. A task allocation can retain inline storage while stale Wakers live; graph RSS is not an isolated actor-memory measurement. Public Actor/Handler/Backend signatures and Tokio default stay unchanged.

V6 (source recorded per block) pairs zero-sized metadata schedule callbacks (Z0/1) and a no-sleeping-peer precheck (W0/1) on one common source. It targets captured-schedule reference traffic and a shared rotating-target RMW. Graph/profile evidence does not establish mixed-load wake tails or production readiness.

V4 (09dd194) includes the production stop fix, shared selector correction and Runnable/quanta controls. V5 (329006e) adds owner deques/batch injection on another common source. Earlier V4 preflights were stopped before scoring: the scale harness initially rejected >256 producers, then inspection found missing owned-runtime/test selectors. Both were corrected, gated and smoked on a new source; old preflight outcomes are not resource-budget rankings.

A separate deterministic stop race was reproduced in both production actor modes and fixed in [PR 185](https://github.com/lambdaclass/spawned/pull/185). V3 timings remain on their recorded pre-PR 185 source; they are not claimed as measurements of current main.

## Machine and measurement

Native Linux fede256 (SSH alias fede), Ryzen 9 9950X3D, 16 physical / 32 logical CPUs, about 251 GiB RAM. Rust 1.93, nice 10. Actor threads inherit CPUs 2–5; producers 6,7,14,15; collectors 8,9,16,17. These are distinct physical cores without own SMT pairing. Workers in V1–V8 are not individually pinned; V9 explicitly pins each sharded worker before actor startup. All actor budgets, including five runtime workers, share the same four actor cores. Shared-host activity and frequency scaling remain uncontrolled.

Four-core blocks use the masks above. Eight-core scale blocks use actor CPUs 2–9, producers 10–13 and collectors 14–17, with 1,024 logical producers on four driver runtime threads; V4 also includes 512 producers. Those 16 CPU IDs are distinct physical cores. The eight-core mask spans two L3 domains: CPUs 2–7 share 96 MiB L3, CPUs 8–9 share 32 MiB L3 withothercores. Four-core actors stay in the 96 MiB domain. This is not a homogeneous-core strong-scaling experiment, and no cache-event cause is inferred. The archived cpu-cache-topology.json records per-CPU domains/SMT siblings. Their two-million-root requests and actor populations remain separate from the four-core blocks. Native N7+R1 versus Tokio8 and N8+R1 versus Tokio9 expose corresponding worker controls on eight actor cores. Native N3+R1 versus Tokio N4 controls total runtime worker count; native N4+R1 versus Tokio N4 controls actor worker count, and versus Tokio N5 controls total runtime workers. The native idle owner thread and lazily created blocking helpers are additional and disclosed. Graphs use 1,920 actors, 256 logical producers, 1,048,576 roots and eight handlers/root with 100 synthetic CPU iterations/handler. Same/cross placement is enforced only for static shards; elsewhere it is a spawn-order control.

Blocking callers use 256 native producer threads; async callers use four current-thread runtimes driving 256 logical producers. Both use the same four producer cores and one outstanding root per producer. Their total thread populations differ. Observation-counter off removes one shared atomic, not clocks, latency storage, allocation or all instrumentation.

Bars/points show medians. Matched graph bars include fresh-process min/max whiskers; all ranges are in SUMMARY.csv, not confidence intervals. Profiled runs, smokes, soak smokes, different versions, caller modes, counters and slices remain separate.

## Completed blocks

| Block | Source | Processes |
|---|---|---:|
| V1-smoke | `07f0e1120e14` | 72 |
| V1-graphs | `07f0e1120e14` | 360 |
| V2-smoke | `eefc302aca9c` | 160 |
| V2-soak-smoke | `eefc302aca9c` | 8 |
| V2-profiles | `eefc302aca9c` | 40 |
| V2-queues | `eefc302aca9c` | 200 |
| V2-callers | `eefc302aca9c` | 320 |
| V3-smoke | `097ae02f6d05` | 52 |
| V3-soak-smoke | `097ae02f6d05` | 10 |
| V3-graphs | `097ae02f6d05` | 520 |
| V3-profiles | `097ae02f6d05` | 24 |
| V3-clock | `097ae02f6d05` | 340 |
| V3-offered | `097ae02f6d05` | 432 |
| V3-soaks | `097ae02f6d05` | 10 |
| V4-smoke | `09dd194b832b` | 136 |
| V4-scale-smoke | `09dd194b832b` | 96 |
| V4-soak-smoke | `09dd194b832b` | 2 |
| V4-graphs | `09dd194b832b` | 680 |
| V4-profiles | `09dd194b832b` | 20 |
| V4-offered | `09dd194b832b` | 252 |
| V4-soaks | `09dd194b832b` | 2 |
| V5-smoke | `329006e8c02e` | 112 |
| V5-scale-smoke | `329006e8c02e` | 28 |
| V5-soak-smoke | `329006e8c02e` | 2 |
| V5-graphs | `329006e8c02e` | 560 |
| V5-profiles | `329006e8c02e` | 16 |
| V6-smoke | `0f81b5769e04` | 48 |
| V6-graphs | `0f81b5769e04` | 240 |
| V6-profiles | `0f81b5769e04` | 16 |
| V7-smoke | `acd37a3b7cfc` | 44 |
| V7-graphs | `acd37a3b7cfc` | 220 |
| V7-profiles | `acd37a3b7cfc` | 16 |
| V7-scale-smoke | `acd37a3b7cfc` | 32 |
| V7-scale | `acd37a3b7cfc` | 96 |
| V8-graph-smoke | `2cec0a6335c1` | 24 |
| V8-scale-smoke | `2cec0a6335c1` | 24 |
| V8-smoke | `2cec0a6335c1` | 44 |
| V8-soak-smoke | `2cec0a6335c1` | 5 |
| V8-graphs | `2cec0a6335c1` | 120 |
| V8-profiles | `2cec0a6335c1` | 12 |
| V8-scale | `2cec0a6335c1` | 72 |
| V8-offered | `2cec0a6335c1` | 176 |
| V8-soaks | `2cec0a6335c1` | 5 |
| V9-smoke | `a8471f74b2b2` | 24 |
| V9-pipeline | `a8471f74b2b2` | 72 |

## Matched graph results

![V1-graphs-native-graphs-blocking-a1920-p256-replies_per_s](graphs/V1-graphs-native-graphs-blocking-a1920-p256-replies_per_s.png)

![V1-graphs-native-graphs-blocking-a1920-p256-observed_p99_ns](graphs/V1-graphs-native-graphs-blocking-a1920-p256-observed_p99_ns.png)

![V2-queues-control-queues-blocking-a1920-p256-replies_per_s](graphs/V2-queues-control-queues-blocking-a1920-p256-replies_per_s.png)

![V2-queues-control-queues-blocking-a1920-p256-observed_p99_ns](graphs/V2-queues-control-queues-blocking-a1920-p256-observed_p99_ns.png)

![V3-graphs-root-graphs-async-a1920-p256-replies_per_s](graphs/V3-graphs-root-graphs-async-a1920-p256-replies_per_s.png)

![V3-graphs-root-graphs-async-a1920-p256-observed_p99_ns](graphs/V3-graphs-root-graphs-async-a1920-p256-observed_p99_ns.png)

![V3-graphs-root-graphs-blocking-a1920-p256-replies_per_s](graphs/V3-graphs-root-graphs-blocking-a1920-p256-replies_per_s.png)

![V3-graphs-root-graphs-blocking-a1920-p256-observed_p99_ns](graphs/V3-graphs-root-graphs-blocking-a1920-p256-observed_p99_ns.png)

![V4-graphs-runnable-graphs-async-a1920-p256-replies_per_s](graphs/V4-graphs-runnable-graphs-async-a1920-p256-replies_per_s.png)

![V4-graphs-runnable-graphs-async-a1920-p256-observed_p99_ns](graphs/V4-graphs-runnable-graphs-async-a1920-p256-observed_p99_ns.png)

![V4-graphs-runnable-graphs-blocking-a1920-p256-replies_per_s](graphs/V4-graphs-runnable-graphs-blocking-a1920-p256-replies_per_s.png)

![V4-graphs-runnable-graphs-blocking-a1920-p256-observed_p99_ns](graphs/V4-graphs-runnable-graphs-blocking-a1920-p256-observed_p99_ns.png)

![V5-graphs-deque-graphs-async-a1920-p256-replies_per_s](graphs/V5-graphs-deque-graphs-async-a1920-p256-replies_per_s.png)

![V5-graphs-deque-graphs-async-a1920-p256-observed_p99_ns](graphs/V5-graphs-deque-graphs-async-a1920-p256-observed_p99_ns.png)

![V5-graphs-deque-graphs-blocking-a1920-p256-replies_per_s](graphs/V5-graphs-deque-graphs-blocking-a1920-p256-replies_per_s.png)

![V5-graphs-deque-graphs-blocking-a1920-p256-observed_p99_ns](graphs/V5-graphs-deque-graphs-blocking-a1920-p256-observed_p99_ns.png)

![V6-graphs-callback-graphs-async-a1920-p256-replies_per_s](graphs/V6-graphs-callback-graphs-async-a1920-p256-replies_per_s.png)

![V6-graphs-callback-graphs-async-a1920-p256-observed_p99_ns](graphs/V6-graphs-callback-graphs-async-a1920-p256-observed_p99_ns.png)

![V7-graphs-inline-graphs-async-a1920-p256-replies_per_s](graphs/V7-graphs-inline-graphs-async-a1920-p256-replies_per_s.png)

![V7-graphs-inline-graphs-async-a1920-p256-observed_p99_ns](graphs/V7-graphs-inline-graphs-async-a1920-p256-observed_p99_ns.png)

![V7-scale-inline-scale-async-a16128-p1024-replies_per_s](graphs/V7-scale-inline-scale-async-a16128-p1024-replies_per_s.png)

![V7-scale-inline-scale-async-a16128-p1024-observed_p99_ns](graphs/V7-scale-inline-scale-async-a16128-p1024-observed_p99_ns.png)

![V8-graphs-fair-graphs-async-a1920-p256-replies_per_s](graphs/V8-graphs-fair-graphs-async-a1920-p256-replies_per_s.png)

![V8-graphs-fair-graphs-async-a1920-p256-observed_p99_ns](graphs/V8-graphs-fair-graphs-async-a1920-p256-observed_p99_ns.png)

![V8-scale-fair-scale-async-a16128-p1024-replies_per_s](graphs/V8-scale-fair-scale-async-a16128-p1024-replies_per_s.png)

![V8-scale-fair-scale-async-a16128-p1024-observed_p99_ns](graphs/V8-scale-fair-scale-async-a16128-p1024-observed_p99_ns.png)

## Graph numbers

| Block | Actors/producers | Shape | Caller/counter | Policy | n | Root replies/s | Root p99 ms | Executor CPU ns/root |
|---|---|---|---|---|---:|---:|---:|---:|
| V1-graphs | 1920/256 | chain/cross | blocking/on | async N3 | 5 | 720,938 | 0.632 | 4,158 |
| V1-graphs | 1920/256 | fanout/cross | blocking/on | async N3 | 5 | 734,035 | 0.639 | 4,084 |
| V1-graphs | 1920/256 | chain/same | blocking/on | async N3 | 5 | 708,632 | 0.642 | 4,223 |
| V1-graphs | 1920/256 | fanout/same | blocking/on | async N3 | 5 | 736,105 | 0.648 | 4,073 |
| V1-graphs | 1920/256 | chain/cross | blocking/on | async N4 | 5 | 936,967 | 0.493 | 4,257 |
| V1-graphs | 1920/256 | fanout/cross | blocking/on | async N4 | 5 | 936,056 | 0.509 | 4,106 |
| V1-graphs | 1920/256 | chain/same | blocking/on | async N4 | 5 | 919,331 | 0.494 | 4,213 |
| V1-graphs | 1920/256 | fanout/same | blocking/on | async N4 | 5 | 955,185 | 0.498 | 4,183 |
| V1-graphs | 1920/256 | chain/cross | blocking/on | async N5 | 5 | 937,803 | 2.784 | 4,260 |
| V1-graphs | 1920/256 | fanout/cross | blocking/on | async N5 | 5 | 922,204 | 2.883 | 4,309 |
| V1-graphs | 1920/256 | chain/same | blocking/on | async N5 | 5 | 943,435 | 2.815 | 4,234 |
| V1-graphs | 1920/256 | fanout/same | blocking/on | async N5 | 5 | 966,285 | 2.775 | 4,097 |
| V1-graphs | 1920/256 | chain/cross | blocking/on | native-global N2+R1 | 5 | 219,635 | 1.981 | 9,088 |
| V1-graphs | 1920/256 | fanout/cross | blocking/on | native-global N2+R1 | 5 | 293,331 | 1.350 | 6,797 |
| V1-graphs | 1920/256 | chain/same | blocking/on | native-global N2+R1 | 5 | 218,612 | 2.025 | 9,125 |
| V1-graphs | 1920/256 | fanout/same | blocking/on | native-global N2+R1 | 5 | 295,210 | 1.240 | 6,755 |
| V1-graphs | 1920/256 | chain/cross | blocking/on | native-global N3+R1 | 5 | 213,796 | 1.994 | 13,209 |
| V1-graphs | 1920/256 | fanout/cross | blocking/on | native-global N3+R1 | 5 | 328,793 | 1.230 | 8,808 |
| V1-graphs | 1920/256 | chain/same | blocking/on | native-global N3+R1 | 5 | 213,863 | 2.006 | 13,206 |
| V1-graphs | 1920/256 | fanout/same | blocking/on | native-global N3+R1 | 5 | 341,169 | 1.053 | 8,529 |
| V1-graphs | 1920/256 | chain/cross | blocking/on | native-global N4+R1 | 5 | 185,389 | 2.070 | 18,625 |
| V1-graphs | 1920/256 | fanout/cross | blocking/on | native-global N4+R1 | 5 | 310,372 | 1.011 | 11,724 |
| V1-graphs | 1920/256 | chain/same | blocking/on | native-global N4+R1 | 5 | 183,956 | 2.111 | 19,004 |
| V1-graphs | 1920/256 | fanout/same | blocking/on | native-global N4+R1 | 5 | 305,170 | 1.102 | 11,867 |
| V1-graphs | 1920/256 | chain/cross | blocking/on | native-local N2+R1 | 5 | 217,268 | 5.481 | 8,995 |
| V1-graphs | 1920/256 | fanout/cross | blocking/on | native-local N2+R1 | 5 | 237,662 | 3.368 | 8,239 |
| V1-graphs | 1920/256 | chain/same | blocking/on | native-local N2+R1 | 5 | 213,733 | 5.596 | 9,117 |
| V1-graphs | 1920/256 | fanout/same | blocking/on | native-local N2+R1 | 5 | 214,426 | 3.628 | 9,070 |
| V1-graphs | 1920/256 | chain/cross | blocking/on | native-local N3+R1 | 5 | 186,462 | 6.795 | 14,463 |
| V1-graphs | 1920/256 | fanout/cross | blocking/on | native-local N3+R1 | 5 | 185,205 | 6.687 | 14,286 |
| V1-graphs | 1920/256 | chain/same | blocking/on | native-local N3+R1 | 5 | 190,418 | 7.206 | 14,215 |
| V1-graphs | 1920/256 | fanout/same | blocking/on | native-local N3+R1 | 5 | 211,190 | 4.680 | 12,950 |
| V1-graphs | 1920/256 | chain/cross | blocking/on | native-local N4+R1 | 5 | 176,040 | 8.027 | 19,125 |
| V1-graphs | 1920/256 | fanout/cross | blocking/on | native-local N4+R1 | 5 | 196,752 | 6.208 | 17,179 |
| V1-graphs | 1920/256 | chain/same | blocking/on | native-local N4+R1 | 5 | 173,209 | 8.102 | 19,403 |
| V1-graphs | 1920/256 | fanout/same | blocking/on | native-local N4+R1 | 5 | 194,415 | 6.249 | 17,093 |
| V1-graphs | 1920/256 | chain/cross | blocking/on | sharded-16 N3 | 5 | 363,460 | 1.135 | 8,239 |
| V1-graphs | 1920/256 | fanout/cross | blocking/on | sharded-16 N3 | 5 | 481,828 | 0.698 | 6,220 |
| V1-graphs | 1920/256 | chain/same | blocking/on | sharded-16 N3 | 5 | 612,753 | 1.726 | 4,802 |
| V1-graphs | 1920/256 | fanout/same | blocking/on | sharded-16 N3 | 5 | 720,968 | 1.723 | 4,119 |
| V1-graphs | 1920/256 | chain/cross | blocking/on | sharded-16 N4 | 5 | 468,996 | 0.732 | 8,427 |
| V1-graphs | 1920/256 | fanout/cross | blocking/on | sharded-16 N4 | 5 | 592,361 | 0.731 | 6,693 |
| V1-graphs | 1920/256 | chain/same | blocking/on | sharded-16 N4 | 5 | 816,753 | 0.420 | 4,722 |
| V1-graphs | 1920/256 | fanout/same | blocking/on | sharded-16 N4 | 5 | 973,597 | 0.275 | 4,040 |
| V1-graphs | 1920/256 | chain/cross | blocking/on | sharded-16 N5 | 5 | 370,966 | 1.234 | 8,941 |
| V1-graphs | 1920/256 | fanout/cross | blocking/on | sharded-16 N5 | 5 | 440,521 | 1.245 | 8,146 |
| V1-graphs | 1920/256 | chain/same | blocking/on | sharded-16 N5 | 5 | 625,958 | 1.374 | 4,618 |
| V1-graphs | 1920/256 | fanout/same | blocking/on | sharded-16 N5 | 5 | 667,163 | 1.224 | 4,271 |
| V1-graphs | 1920/256 | chain/cross | blocking/on | tokio-16 N3 | 5 | 634,165 | 0.868 | 4,721 |
| V1-graphs | 1920/256 | fanout/cross | blocking/on | tokio-16 N3 | 5 | 683,682 | 0.706 | 4,385 |
| V1-graphs | 1920/256 | chain/same | blocking/on | tokio-16 N3 | 5 | 627,962 | 0.896 | 4,717 |
| V1-graphs | 1920/256 | fanout/same | blocking/on | tokio-16 N3 | 5 | 660,836 | 0.727 | 4,536 |
| V1-graphs | 1920/256 | chain/cross | blocking/on | tokio-16 N4 | 5 | 825,876 | 0.701 | 4,739 |
| V1-graphs | 1920/256 | fanout/cross | blocking/on | tokio-16 N4 | 5 | 877,507 | 0.587 | 4,405 |
| V1-graphs | 1920/256 | chain/same | blocking/on | tokio-16 N4 | 5 | 812,718 | 0.922 | 4,721 |
| V1-graphs | 1920/256 | fanout/same | blocking/on | tokio-16 N4 | 5 | 899,838 | 0.563 | 4,440 |
| V1-graphs | 1920/256 | chain/cross | blocking/on | tokio-16 N5 | 5 | 852,205 | 3.548 | 4,682 |
| V1-graphs | 1920/256 | fanout/cross | blocking/on | tokio-16 N5 | 5 | 886,451 | 3.230 | 4,493 |
| V1-graphs | 1920/256 | chain/same | blocking/on | tokio-16 N5 | 5 | 854,118 | 3.443 | 4,659 |
| V1-graphs | 1920/256 | fanout/same | blocking/on | tokio-16 N5 | 5 | 882,916 | 3.242 | 4,525 |
| V1-graphs | 1920/256 | chain/cross | blocking/on | tokio-pool N3 | 5 | 719,472 | 0.636 | 4,167 |
| V1-graphs | 1920/256 | fanout/cross | blocking/on | tokio-pool N3 | 5 | 735,632 | 0.634 | 4,054 |
| V1-graphs | 1920/256 | chain/same | blocking/on | tokio-pool N3 | 5 | 709,331 | 0.635 | 4,201 |
| V1-graphs | 1920/256 | fanout/same | blocking/on | tokio-pool N3 | 5 | 754,571 | 0.625 | 3,972 |
| V1-graphs | 1920/256 | chain/cross | blocking/on | tokio-pool N4 | 5 | 938,498 | 0.488 | 4,236 |
| V1-graphs | 1920/256 | fanout/cross | blocking/on | tokio-pool N4 | 5 | 996,752 | 0.484 | 4,007 |
| V1-graphs | 1920/256 | chain/same | blocking/on | tokio-pool N4 | 5 | 923,755 | 0.505 | 4,324 |
| V1-graphs | 1920/256 | fanout/same | blocking/on | tokio-pool N4 | 5 | 971,502 | 0.499 | 4,112 |
| V1-graphs | 1920/256 | chain/cross | blocking/on | tokio-pool N5 | 5 | 937,979 | 2.666 | 4,257 |
| V1-graphs | 1920/256 | fanout/cross | blocking/on | tokio-pool N5 | 5 | 967,811 | 2.617 | 4,129 |
| V1-graphs | 1920/256 | chain/same | blocking/on | tokio-pool N5 | 5 | 940,695 | 2.731 | 4,225 |
| V1-graphs | 1920/256 | fanout/same | blocking/on | tokio-pool N5 | 5 | 965,912 | 2.979 | 4,132 |
| V2-queues | 1920/256 | chain/cross | blocking/on | async N4 | 5 | 908,156 | 0.511 | 4,399 |
| V2-queues | 1920/256 | fanout/cross | blocking/on | async N4 | 5 | 926,179 | 0.532 | 4,313 |
| V2-queues | 1920/256 | chain/same | blocking/on | async N4 | 5 | 936,991 | 0.501 | 4,264 |
| V2-queues | 1920/256 | fanout/same | blocking/on | async N4 | 5 | 950,768 | 0.508 | 4,189 |
| V2-queues | 1920/256 | chain/cross | blocking/on | native-global N3+R1 | 5 | 217,025 | 1.987 | 13,088 |
| V2-queues | 1920/256 | fanout/cross | blocking/on | native-global N3+R1 | 5 | 325,707 | 1.152 | 8,945 |
| V2-queues | 1920/256 | chain/same | blocking/on | native-global N3+R1 | 5 | 207,229 | 2.044 | 13,631 |
| V2-queues | 1920/256 | fanout/same | blocking/on | native-global N3+R1 | 5 | 272,776 | 1.873 | 10,120 |
| V2-queues | 1920/256 | chain/cross | blocking/on | native-global N4+R1 | 5 | 192,154 | 2.008 | 18,135 |
| V2-queues | 1920/256 | fanout/cross | blocking/on | native-global N4+R1 | 5 | 320,711 | 0.990 | 11,426 |
| V2-queues | 1920/256 | chain/same | blocking/on | native-global N4+R1 | 5 | 186,097 | 2.108 | 18,919 |
| V2-queues | 1920/256 | fanout/same | blocking/on | native-global N4+R1 | 5 | 313,455 | 1.018 | 11,685 |
| V2-queues | 1920/256 | chain/cross | blocking/on | native-local N3+R1 | 5 | 173,329 | 7.969 | 15,215 |
| V2-queues | 1920/256 | fanout/cross | blocking/on | native-local N3+R1 | 5 | 195,466 | 5.003 | 13,700 |
| V2-queues | 1920/256 | chain/same | blocking/on | native-local N3+R1 | 5 | 195,044 | 6.841 | 14,008 |
| V2-queues | 1920/256 | fanout/same | blocking/on | native-local N3+R1 | 5 | 170,683 | 8.382 | 12,938 |
| V2-queues | 1920/256 | chain/cross | blocking/on | native-local N4+R1 | 5 | 158,138 | 10.015 | 20,140 |
| V2-queues | 1920/256 | fanout/cross | blocking/on | native-local N4+R1 | 5 | 172,182 | 6.851 | 17,346 |
| V2-queues | 1920/256 | chain/same | blocking/on | native-local N4+R1 | 5 | 176,196 | 7.851 | 18,920 |
| V2-queues | 1920/256 | fanout/same | blocking/on | native-local N4+R1 | 5 | 176,415 | 8.206 | 18,069 |
| V2-queues | 1920/256 | chain/cross | blocking/on | native-steal N3+R1 | 5 | 325,441 | 3.597 | 9,194 |
| V2-queues | 1920/256 | fanout/cross | blocking/on | native-steal N3+R1 | 5 | 372,230 | 2.121 | 8,058 |
| V2-queues | 1920/256 | chain/same | blocking/on | native-steal N3+R1 | 5 | 323,839 | 3.684 | 9,224 |
| V2-queues | 1920/256 | fanout/same | blocking/on | native-steal N3+R1 | 5 | 345,749 | 2.391 | 8,675 |
| V2-queues | 1920/256 | chain/cross | blocking/on | native-steal N4+R1 | 5 | 382,825 | 3.165 | 10,424 |
| V2-queues | 1920/256 | fanout/cross | blocking/on | native-steal N4+R1 | 5 | 415,859 | 2.070 | 9,590 |
| V2-queues | 1920/256 | chain/same | blocking/on | native-steal N4+R1 | 5 | 378,131 | 3.315 | 10,572 |
| V2-queues | 1920/256 | fanout/same | blocking/on | native-steal N4+R1 | 5 | 406,910 | 2.075 | 9,795 |
| V2-queues | 1920/256 | chain/cross | blocking/on | sharded-16 N4 | 5 | 482,830 | 0.709 | 8,239 |
| V2-queues | 1920/256 | fanout/cross | blocking/on | sharded-16 N4 | 5 | 608,561 | 0.566 | 6,541 |
| V2-queues | 1920/256 | chain/same | blocking/on | sharded-16 N4 | 5 | 816,547 | 0.421 | 4,815 |
| V2-queues | 1920/256 | fanout/same | blocking/on | sharded-16 N4 | 5 | 948,445 | 0.284 | 4,145 |
| V2-queues | 1920/256 | chain/cross | blocking/on | tokio-16 N4 | 5 | 826,268 | 0.717 | 4,747 |
| V2-queues | 1920/256 | fanout/cross | blocking/on | tokio-16 N4 | 5 | 781,356 | 0.707 | 4,743 |
| V2-queues | 1920/256 | chain/same | blocking/on | tokio-16 N4 | 5 | 804,506 | 0.861 | 4,750 |
| V2-queues | 1920/256 | fanout/same | blocking/on | tokio-16 N4 | 5 | 671,443 | 1.204 | 5,573 |
| V2-queues | 1920/256 | chain/cross | blocking/on | tokio-pool N4 | 5 | 938,566 | 0.489 | 4,224 |
| V2-queues | 1920/256 | fanout/cross | blocking/on | tokio-pool N4 | 5 | 968,831 | 0.488 | 4,122 |
| V2-queues | 1920/256 | chain/same | blocking/on | tokio-pool N4 | 5 | 940,495 | 0.488 | 4,237 |
| V2-queues | 1920/256 | fanout/same | blocking/on | tokio-pool N4 | 5 | 842,781 | 0.587 | 4,735 |
| V3-graphs | 1920/256 | chain/cross | async/on | async N4 | 5 | 965,540 | 0.464 | 4,124 |
| V3-graphs | 1920/256 | fanout/cross | async/on | async N4 | 5 | 945,297 | 0.497 | 4,211 |
| V3-graphs | 1920/256 | chain/same | async/on | async N4 | 5 | 955,640 | 0.471 | 4,167 |
| V3-graphs | 1920/256 | fanout/same | async/on | async N4 | 5 | 947,033 | 0.497 | 4,204 |
| V3-graphs | 1920/256 | chain/cross | blocking/on | async N4 | 5 | 931,017 | 0.493 | 4,260 |
| V3-graphs | 1920/256 | fanout/cross | blocking/on | async N4 | 5 | 958,911 | 0.498 | 4,166 |
| V3-graphs | 1920/256 | chain/same | blocking/on | async N4 | 5 | 929,303 | 0.496 | 4,297 |
| V3-graphs | 1920/256 | fanout/same | blocking/on | async N4 | 5 | 951,793 | 0.505 | 4,197 |
| V3-graphs | 1920/256 | chain/cross | async/on | async N5 | 5 | 969,129 | 2.671 | 4,100 |
| V3-graphs | 1920/256 | fanout/cross | async/on | async N5 | 5 | 962,857 | 2.675 | 4,135 |
| V3-graphs | 1920/256 | chain/same | async/on | async N5 | 5 | 953,643 | 2.800 | 4,171 |
| V3-graphs | 1920/256 | fanout/same | async/on | async N5 | 5 | 947,613 | 2.587 | 4,203 |
| V3-graphs | 1920/256 | chain/cross | blocking/on | async N5 | 5 | 934,638 | 2.765 | 4,273 |
| V3-graphs | 1920/256 | fanout/cross | blocking/on | async N5 | 5 | 955,786 | 2.772 | 4,167 |
| V3-graphs | 1920/256 | chain/same | blocking/on | async N5 | 5 | 939,589 | 2.764 | 4,253 |
| V3-graphs | 1920/256 | fanout/same | blocking/on | async N5 | 5 | 947,579 | 2.952 | 4,180 |
| V3-graphs | 1920/256 | chain/cross | async/on | native-global N3+R1 | 5 | 220,438 | 1.923 | 13,024 |
| V3-graphs | 1920/256 | fanout/cross | async/on | native-global N3+R1 | 5 | 349,867 | 1.035 | 8,298 |
| V3-graphs | 1920/256 | chain/same | async/on | native-global N3+R1 | 5 | 218,599 | 1.938 | 13,107 |
| V3-graphs | 1920/256 | fanout/same | async/on | native-global N3+R1 | 5 | 342,355 | 1.065 | 8,533 |
| V3-graphs | 1920/256 | chain/cross | blocking/on | native-global N3+R1 | 5 | 219,792 | 1.980 | 13,049 |
| V3-graphs | 1920/256 | fanout/cross | blocking/on | native-global N3+R1 | 5 | 344,916 | 1.067 | 8,468 |
| V3-graphs | 1920/256 | chain/same | blocking/on | native-global N3+R1 | 5 | 220,722 | 1.977 | 13,008 |
| V3-graphs | 1920/256 | fanout/same | blocking/on | native-global N3+R1 | 5 | 344,790 | 1.014 | 8,477 |
| V3-graphs | 1920/256 | chain/cross | async/on | native-global N4+R1 | 5 | 192,982 | 1.983 | 18,047 |
| V3-graphs | 1920/256 | fanout/cross | async/on | native-global N4+R1 | 5 | 321,651 | 1.066 | 11,467 |
| V3-graphs | 1920/256 | chain/same | async/on | native-global N4+R1 | 5 | 193,067 | 1.983 | 18,200 |
| V3-graphs | 1920/256 | fanout/same | async/on | native-global N4+R1 | 5 | 317,379 | 1.069 | 11,543 |
| V3-graphs | 1920/256 | chain/cross | blocking/on | native-global N4+R1 | 5 | 192,305 | 2.067 | 18,208 |
| V3-graphs | 1920/256 | fanout/cross | blocking/on | native-global N4+R1 | 5 | 320,218 | 1.054 | 11,374 |
| V3-graphs | 1920/256 | chain/same | blocking/on | native-global N4+R1 | 5 | 190,366 | 2.068 | 18,280 |
| V3-graphs | 1920/256 | fanout/same | blocking/on | native-global N4+R1 | 5 | 317,627 | 1.005 | 11,505 |
| V3-graphs | 1920/256 | chain/cross | async/on | native-local N3+R1 | 5 | 184,711 | 7.269 | 14,658 |
| V3-graphs | 1920/256 | fanout/cross | async/on | native-local N3+R1 | 5 | 206,198 | 5.189 | 13,227 |
| V3-graphs | 1920/256 | chain/same | async/on | native-local N3+R1 | 5 | 191,018 | 6.994 | 14,225 |
| V3-graphs | 1920/256 | fanout/same | async/on | native-local N3+R1 | 5 | 204,050 | 5.039 | 13,360 |
| V3-graphs | 1920/256 | chain/cross | blocking/on | native-local N3+R1 | 5 | 189,313 | 6.825 | 14,317 |
| V3-graphs | 1920/256 | fanout/cross | blocking/on | native-local N3+R1 | 5 | 209,344 | 4.852 | 13,022 |
| V3-graphs | 1920/256 | chain/same | blocking/on | native-local N3+R1 | 5 | 191,103 | 6.671 | 14,220 |
| V3-graphs | 1920/256 | fanout/same | blocking/on | native-local N3+R1 | 5 | 207,002 | 4.817 | 13,137 |
| V3-graphs | 1920/256 | chain/cross | async/on | native-local N4+R1 | 5 | 168,953 | 8.719 | 19,870 |
| V3-graphs | 1920/256 | fanout/cross | async/on | native-local N4+R1 | 5 | 184,891 | 6.763 | 18,219 |
| V3-graphs | 1920/256 | chain/same | async/on | native-local N4+R1 | 5 | 167,976 | 8.509 | 19,993 |
| V3-graphs | 1920/256 | fanout/same | async/on | native-local N4+R1 | 5 | 188,396 | 6.875 | 17,898 |
| V3-graphs | 1920/256 | chain/cross | blocking/on | native-local N4+R1 | 5 | 170,563 | 8.515 | 19,728 |
| V3-graphs | 1920/256 | fanout/cross | blocking/on | native-local N4+R1 | 5 | 189,417 | 6.638 | 17,818 |
| V3-graphs | 1920/256 | chain/same | blocking/on | native-local N4+R1 | 5 | 168,696 | 8.561 | 19,807 |
| V3-graphs | 1920/256 | fanout/same | blocking/on | native-local N4+R1 | 5 | 188,137 | 6.572 | 17,877 |
| V3-graphs | 1920/256 | chain/cross | async/on | native-root N3+R1 Q8 | 5 | 392,078 | 2.063 | 7,638 |
| V3-graphs | 1920/256 | fanout/cross | async/on | native-root N3+R1 Q8 | 5 | 406,678 | 2.170 | 7,364 |
| V3-graphs | 1920/256 | chain/same | async/on | native-root N3+R1 Q8 | 5 | 367,661 | 2.201 | 8,139 |
| V3-graphs | 1920/256 | fanout/same | async/on | native-root N3+R1 Q8 | 5 | 409,754 | 2.121 | 7,308 |
| V3-graphs | 1920/256 | chain/cross | blocking/on | native-root N3+R1 Q8 | 5 | 402,277 | 1.791 | 7,455 |
| V3-graphs | 1920/256 | fanout/cross | blocking/on | native-root N3+R1 Q8 | 5 | 430,647 | 1.958 | 6,965 |
| V3-graphs | 1920/256 | chain/same | blocking/on | native-root N3+R1 Q8 | 5 | 399,957 | 2.015 | 7,498 |
| V3-graphs | 1920/256 | fanout/same | blocking/on | native-root N3+R1 Q8 | 5 | 416,453 | 2.137 | 7,195 |
| V3-graphs | 1920/256 | chain/cross | async/on | native-root N4+R1 Q8 | 5 | 472,266 | 2.374 | 8,451 |
| V3-graphs | 1920/256 | fanout/cross | async/on | native-root N4+R1 Q8 | 5 | 507,244 | 1.755 | 7,869 |
| V3-graphs | 1920/256 | chain/same | async/on | native-root N4+R1 Q8 | 5 | 455,456 | 2.544 | 8,629 |
| V3-graphs | 1920/256 | fanout/same | async/on | native-root N4+R1 Q8 | 5 | 501,579 | 1.829 | 7,954 |
| V3-graphs | 1920/256 | chain/cross | blocking/on | native-root N4+R1 Q8 | 5 | 475,720 | 2.460 | 8,328 |
| V3-graphs | 1920/256 | fanout/cross | blocking/on | native-root N4+R1 Q8 | 5 | 508,056 | 1.805 | 7,870 |
| V3-graphs | 1920/256 | chain/same | blocking/on | native-root N4+R1 Q8 | 5 | 475,737 | 2.098 | 8,403 |
| V3-graphs | 1920/256 | fanout/same | blocking/on | native-root N4+R1 Q8 | 5 | 510,380 | 1.879 | 7,834 |
| V3-graphs | 1920/256 | chain/cross | async/on | native-steal N3+R1 | 5 | 323,741 | 3.463 | 9,251 |
| V3-graphs | 1920/256 | fanout/cross | async/on | native-steal N3+R1 | 5 | 335,788 | 2.362 | 8,921 |
| V3-graphs | 1920/256 | chain/same | async/on | native-steal N3+R1 | 5 | 324,125 | 3.371 | 9,240 |
| V3-graphs | 1920/256 | fanout/same | async/on | native-steal N3+R1 | 5 | 334,757 | 2.350 | 8,952 |
| V3-graphs | 1920/256 | chain/cross | blocking/on | native-steal N3+R1 | 5 | 334,522 | 3.268 | 8,964 |
| V3-graphs | 1920/256 | fanout/cross | blocking/on | native-steal N3+R1 | 5 | 363,544 | 2.258 | 8,250 |
| V3-graphs | 1920/256 | chain/same | blocking/on | native-steal N3+R1 | 5 | 332,001 | 3.215 | 9,032 |
| V3-graphs | 1920/256 | fanout/same | blocking/on | native-steal N3+R1 | 5 | 356,006 | 2.126 | 8,424 |
| V3-graphs | 1920/256 | chain/cross | async/on | native-steal N4+R1 | 5 | 382,224 | 3.280 | 10,442 |
| V3-graphs | 1920/256 | fanout/cross | async/on | native-steal N4+R1 | 5 | 413,293 | 1.956 | 9,662 |
| V3-graphs | 1920/256 | chain/same | async/on | native-steal N4+R1 | 5 | 378,759 | 3.191 | 10,522 |
| V3-graphs | 1920/256 | fanout/same | async/on | native-steal N4+R1 | 5 | 410,279 | 2.252 | 9,731 |
| V3-graphs | 1920/256 | chain/cross | blocking/on | native-steal N4+R1 | 5 | 390,471 | 3.221 | 10,239 |
| V3-graphs | 1920/256 | fanout/cross | blocking/on | native-steal N4+R1 | 5 | 418,712 | 2.130 | 9,550 |
| V3-graphs | 1920/256 | chain/same | blocking/on | native-steal N4+R1 | 5 | 384,091 | 3.093 | 10,408 |
| V3-graphs | 1920/256 | fanout/same | blocking/on | native-steal N4+R1 | 5 | 414,292 | 2.098 | 9,623 |
| V3-graphs | 1920/256 | chain/cross | async/on | sharded-16 N4 | 5 | 458,082 | 0.803 | 8,463 |
| V3-graphs | 1920/256 | fanout/cross | async/on | sharded-16 N4 | 5 | 581,987 | 0.576 | 6,584 |
| V3-graphs | 1920/256 | chain/same | async/on | sharded-16 N4 | 5 | 764,566 | 0.431 | 4,673 |
| V3-graphs | 1920/256 | fanout/same | async/on | sharded-16 N4 | 5 | 879,342 | 0.323 | 4,081 |
| V3-graphs | 1920/256 | chain/cross | blocking/on | sharded-16 N4 | 5 | 479,057 | 0.706 | 8,290 |
| V3-graphs | 1920/256 | fanout/cross | blocking/on | sharded-16 N4 | 5 | 609,284 | 0.563 | 6,549 |
| V3-graphs | 1920/256 | chain/same | blocking/on | sharded-16 N4 | 5 | 813,683 | 0.424 | 4,843 |
| V3-graphs | 1920/256 | fanout/same | blocking/on | sharded-16 N4 | 5 | 955,259 | 0.280 | 4,077 |
| V3-graphs | 1920/256 | chain/cross | async/on | tokio-16 N4 | 5 | 857,737 | 0.653 | 4,599 |
| V3-graphs | 1920/256 | fanout/cross | async/on | tokio-16 N4 | 5 | 875,209 | 0.560 | 4,550 |
| V3-graphs | 1920/256 | chain/same | async/on | tokio-16 N4 | 5 | 862,644 | 0.649 | 4,617 |
| V3-graphs | 1920/256 | fanout/same | async/on | tokio-16 N4 | 5 | 868,281 | 0.573 | 4,587 |
| V3-graphs | 1920/256 | chain/cross | blocking/on | tokio-16 N4 | 5 | 831,063 | 0.692 | 4,761 |
| V3-graphs | 1920/256 | fanout/cross | blocking/on | tokio-16 N4 | 5 | 880,474 | 0.576 | 4,538 |
| V3-graphs | 1920/256 | chain/same | blocking/on | tokio-16 N4 | 5 | 825,527 | 0.717 | 4,754 |
| V3-graphs | 1920/256 | fanout/same | blocking/on | tokio-16 N4 | 5 | 878,929 | 0.575 | 4,534 |
| V3-graphs | 1920/256 | chain/cross | async/on | tokio-pool N4 | 5 | 924,994 | 0.485 | 4,275 |
| V3-graphs | 1920/256 | fanout/cross | async/on | tokio-pool N4 | 5 | 970,602 | 0.488 | 4,101 |
| V3-graphs | 1920/256 | chain/same | async/on | tokio-pool N4 | 5 | 977,202 | 0.461 | 4,074 |
| V3-graphs | 1920/256 | fanout/same | async/on | tokio-pool N4 | 5 | 972,406 | 0.481 | 4,092 |
| V3-graphs | 1920/256 | chain/cross | blocking/on | tokio-pool N4 | 5 | 949,319 | 0.484 | 4,208 |
| V3-graphs | 1920/256 | fanout/cross | blocking/on | tokio-pool N4 | 5 | 996,832 | 0.482 | 4,004 |
| V3-graphs | 1920/256 | chain/same | blocking/on | tokio-pool N4 | 5 | 943,398 | 0.486 | 4,221 |
| V3-graphs | 1920/256 | fanout/same | blocking/on | tokio-pool N4 | 5 | 977,498 | 0.492 | 4,087 |
| V4-graphs | 1920/256 | chain/cross | async/on | async N4 | 5 | 979,690 | 0.462 | 4,065 |
| V4-graphs | 1920/256 | fanout/cross | async/on | async N4 | 5 | 867,643 | 0.561 | 4,587 |
| V4-graphs | 1920/256 | chain/same | async/on | async N4 | 5 | 940,525 | 0.480 | 4,233 |
| V4-graphs | 1920/256 | fanout/same | async/on | async N4 | 5 | 836,504 | 0.605 | 4,645 |
| V4-graphs | 1920/256 | chain/cross | blocking/on | async N4 | 5 | 915,372 | 0.502 | 4,364 |
| V4-graphs | 1920/256 | fanout/cross | blocking/on | async N4 | 5 | 824,526 | 0.592 | 4,844 |
| V4-graphs | 1920/256 | chain/same | blocking/on | async N4 | 5 | 696,264 | 1.129 | 5,197 |
| V4-graphs | 1920/256 | fanout/same | blocking/on | async N4 | 5 | 912,968 | 0.530 | 4,263 |
| V4-graphs | 1920/256 | chain/cross | async/on | async N5 | 5 | 808,545 | 3.049 | 4,861 |
| V4-graphs | 1920/256 | fanout/cross | async/on | async N5 | 5 | 836,262 | 2.905 | 4,685 |
| V4-graphs | 1920/256 | chain/same | async/on | async N5 | 5 | 897,209 | 3.097 | 4,408 |
| V4-graphs | 1920/256 | fanout/same | async/on | async N5 | 5 | 933,750 | 2.881 | 4,254 |
| V4-graphs | 1920/256 | chain/cross | blocking/on | async N5 | 5 | 926,688 | 2.896 | 4,310 |
| V4-graphs | 1920/256 | fanout/cross | blocking/on | async N5 | 5 | 923,573 | 2.958 | 4,310 |
| V4-graphs | 1920/256 | chain/same | blocking/on | async N5 | 5 | 850,624 | 3.125 | 4,625 |
| V4-graphs | 1920/256 | fanout/same | blocking/on | async N5 | 5 | 935,002 | 2.854 | 4,256 |
| V4-graphs | 1920/256 | chain/cross | async/on | native-root N3+R1 Q8 | 5 | 388,941 | 1.708 | 7,692 |
| V4-graphs | 1920/256 | fanout/cross | async/on | native-root N3+R1 Q8 | 5 | 413,595 | 2.039 | 7,243 |
| V4-graphs | 1920/256 | chain/same | async/on | native-root N3+R1 Q8 | 5 | 385,698 | 2.027 | 7,706 |
| V4-graphs | 1920/256 | fanout/same | async/on | native-root N3+R1 Q8 | 5 | 400,914 | 2.155 | 7,467 |
| V4-graphs | 1920/256 | chain/cross | blocking/on | native-root N3+R1 Q8 | 5 | 395,529 | 2.566 | 7,581 |
| V4-graphs | 1920/256 | fanout/cross | blocking/on | native-root N3+R1 Q8 | 5 | 383,101 | 2.753 | 7,829 |
| V4-graphs | 1920/256 | chain/same | blocking/on | native-root N3+R1 Q8 | 5 | 362,283 | 2.875 | 8,275 |
| V4-graphs | 1920/256 | fanout/same | blocking/on | native-root N3+R1 Q8 | 5 | 420,152 | 2.214 | 7,126 |
| V4-graphs | 1920/256 | chain/cross | async/on | native-root N4+R1 Q1 | 5 | 428,075 | 2.003 | 9,323 |
| V4-graphs | 1920/256 | chain/cross | async/on | native-root N4+R1 Q128 | 5 | 400,845 | 3.729 | 9,771 |
| V4-graphs | 1920/256 | chain/cross | async/on | native-root N4+R1 Q32 | 5 | 467,388 | 2.300 | 8,511 |
| V4-graphs | 1920/256 | chain/cross | async/on | native-root N4+R1 Q8 | 5 | 466,020 | 2.347 | 8,554 |
| V4-graphs | 1920/256 | fanout/cross | async/on | native-root N4+R1 Q1 | 5 | 368,878 | 3.619 | 9,932 |
| V4-graphs | 1920/256 | fanout/cross | async/on | native-root N4+R1 Q128 | 5 | 474,046 | 3.075 | 8,418 |
| V4-graphs | 1920/256 | fanout/cross | async/on | native-root N4+R1 Q32 | 5 | 456,195 | 3.291 | 8,692 |
| V4-graphs | 1920/256 | fanout/cross | async/on | native-root N4+R1 Q8 | 5 | 502,052 | 1.916 | 7,948 |
| V4-graphs | 1920/256 | chain/same | async/on | native-root N4+R1 Q1 | 5 | 410,228 | 3.116 | 9,726 |
| V4-graphs | 1920/256 | chain/same | async/on | native-root N4+R1 Q128 | 5 | 421,160 | 2.923 | 9,363 |
| V4-graphs | 1920/256 | chain/same | async/on | native-root N4+R1 Q32 | 5 | 483,291 | 2.438 | 8,217 |
| V4-graphs | 1920/256 | chain/same | async/on | native-root N4+R1 Q8 | 5 | 467,899 | 2.215 | 8,533 |
| V4-graphs | 1920/256 | fanout/same | async/on | native-root N4+R1 Q1 | 5 | 452,104 | 1.917 | 8,829 |
| V4-graphs | 1920/256 | fanout/same | async/on | native-root N4+R1 Q128 | 5 | 414,584 | 3.576 | 9,565 |
| V4-graphs | 1920/256 | fanout/same | async/on | native-root N4+R1 Q32 | 5 | 456,726 | 2.466 | 8,697 |
| V4-graphs | 1920/256 | fanout/same | async/on | native-root N4+R1 Q8 | 5 | 503,870 | 1.807 | 7,922 |
| V4-graphs | 1920/256 | chain/cross | blocking/on | native-root N4+R1 Q1 | 5 | 434,725 | 2.400 | 9,197 |
| V4-graphs | 1920/256 | chain/cross | blocking/on | native-root N4+R1 Q128 | 5 | 414,129 | 3.511 | 9,477 |
| V4-graphs | 1920/256 | chain/cross | blocking/on | native-root N4+R1 Q32 | 5 | 454,466 | 2.761 | 8,797 |
| V4-graphs | 1920/256 | chain/cross | blocking/on | native-root N4+R1 Q8 | 5 | 475,341 | 1.958 | 8,402 |
| V4-graphs | 1920/256 | fanout/cross | blocking/on | native-root N4+R1 Q1 | 5 | 460,005 | 2.234 | 8,647 |
| V4-graphs | 1920/256 | fanout/cross | blocking/on | native-root N4+R1 Q128 | 5 | 478,710 | 3.134 | 8,350 |
| V4-graphs | 1920/256 | fanout/cross | blocking/on | native-root N4+R1 Q32 | 5 | 515,272 | 2.193 | 7,759 |
| V4-graphs | 1920/256 | fanout/cross | blocking/on | native-root N4+R1 Q8 | 5 | 509,174 | 1.773 | 7,816 |
| V4-graphs | 1920/256 | chain/same | blocking/on | native-root N4+R1 Q1 | 5 | 387,373 | 3.589 | 10,296 |
| V4-graphs | 1920/256 | chain/same | blocking/on | native-root N4+R1 Q128 | 5 | 466,687 | 2.931 | 8,518 |
| V4-graphs | 1920/256 | chain/same | blocking/on | native-root N4+R1 Q32 | 5 | 472,253 | 2.613 | 8,381 |
| V4-graphs | 1920/256 | chain/same | blocking/on | native-root N4+R1 Q8 | 5 | 480,530 | 1.665 | 8,321 |
| V4-graphs | 1920/256 | fanout/same | blocking/on | native-root N4+R1 Q1 | 5 | 470,792 | 1.740 | 8,485 |
| V4-graphs | 1920/256 | fanout/same | blocking/on | native-root N4+R1 Q128 | 5 | 466,410 | 3.214 | 8,571 |
| V4-graphs | 1920/256 | fanout/same | blocking/on | native-root N4+R1 Q32 | 5 | 507,569 | 2.228 | 7,866 |
| V4-graphs | 1920/256 | fanout/same | blocking/on | native-root N4+R1 Q8 | 5 | 508,116 | 1.928 | 7,836 |
| V4-graphs | 1920/256 | chain/cross | async/on | native-runnable N3+R1 Q8 | 5 | 336,248 | 3.998 | 8,852 |
| V4-graphs | 1920/256 | fanout/cross | async/on | native-runnable N3+R1 Q8 | 5 | 423,565 | 2.293 | 7,072 |
| V4-graphs | 1920/256 | chain/same | async/on | native-runnable N3+R1 Q8 | 5 | 333,689 | 4.012 | 8,961 |
| V4-graphs | 1920/256 | fanout/same | async/on | native-runnable N3+R1 Q8 | 5 | 406,030 | 2.745 | 7,354 |
| V4-graphs | 1920/256 | chain/cross | blocking/on | native-runnable N3+R1 Q8 | 5 | 400,518 | 2.891 | 7,488 |
| V4-graphs | 1920/256 | fanout/cross | blocking/on | native-runnable N3+R1 Q8 | 5 | 425,526 | 2.168 | 7,031 |
| V4-graphs | 1920/256 | chain/same | blocking/on | native-runnable N3+R1 Q8 | 5 | 362,143 | 3.686 | 8,279 |
| V4-graphs | 1920/256 | fanout/same | blocking/on | native-runnable N3+R1 Q8 | 5 | 417,926 | 2.102 | 7,154 |
| V4-graphs | 1920/256 | chain/cross | async/on | native-runnable N4+R1 Q1 | 5 | 415,702 | 3.097 | 9,581 |
| V4-graphs | 1920/256 | chain/cross | async/on | native-runnable N4+R1 Q128 | 5 | 475,644 | 2.782 | 8,387 |
| V4-graphs | 1920/256 | chain/cross | async/on | native-runnable N4+R1 Q32 | 5 | 400,854 | 3.734 | 9,825 |
| V4-graphs | 1920/256 | chain/cross | async/on | native-runnable N4+R1 Q8 | 5 | 461,100 | 2.713 | 8,647 |
| V4-graphs | 1920/256 | fanout/cross | async/on | native-runnable N4+R1 Q1 | 5 | 461,251 | 1.784 | 8,655 |
| V4-graphs | 1920/256 | fanout/cross | async/on | native-runnable N4+R1 Q128 | 5 | 472,930 | 3.227 | 8,404 |
| V4-graphs | 1920/256 | fanout/cross | async/on | native-runnable N4+R1 Q32 | 5 | 490,241 | 3.025 | 8,042 |
| V4-graphs | 1920/256 | fanout/cross | async/on | native-runnable N4+R1 Q8 | 5 | 507,807 | 1.861 | 7,854 |
| V4-graphs | 1920/256 | chain/same | async/on | native-runnable N4+R1 Q1 | 5 | 424,627 | 2.799 | 9,399 |
| V4-graphs | 1920/256 | chain/same | async/on | native-runnable N4+R1 Q128 | 5 | 423,357 | 3.051 | 9,364 |
| V4-graphs | 1920/256 | chain/same | async/on | native-runnable N4+R1 Q32 | 5 | 448,726 | 2.940 | 8,855 |
| V4-graphs | 1920/256 | chain/same | async/on | native-runnable N4+R1 Q8 | 5 | 421,907 | 2.986 | 9,008 |
| V4-graphs | 1920/256 | fanout/same | async/on | native-runnable N4+R1 Q1 | 5 | 394,854 | 3.727 | 9,977 |
| V4-graphs | 1920/256 | fanout/same | async/on | native-runnable N4+R1 Q128 | 5 | 465,489 | 3.247 | 8,574 |
| V4-graphs | 1920/256 | fanout/same | async/on | native-runnable N4+R1 Q32 | 5 | 507,179 | 2.482 | 7,873 |
| V4-graphs | 1920/256 | fanout/same | async/on | native-runnable N4+R1 Q8 | 5 | 497,453 | 1.723 | 8,020 |
| V4-graphs | 1920/256 | chain/cross | blocking/on | native-runnable N4+R1 Q1 | 5 | 439,177 | 2.778 | 9,051 |
| V4-graphs | 1920/256 | chain/cross | blocking/on | native-runnable N4+R1 Q128 | 5 | 485,462 | 2.790 | 8,198 |
| V4-graphs | 1920/256 | chain/cross | blocking/on | native-runnable N4+R1 Q32 | 5 | 480,992 | 2.690 | 8,313 |
| V4-graphs | 1920/256 | chain/cross | blocking/on | native-runnable N4+R1 Q8 | 5 | 471,178 | 2.497 | 8,440 |
| V4-graphs | 1920/256 | fanout/cross | blocking/on | native-runnable N4+R1 Q1 | 5 | 402,496 | 3.176 | 9,820 |
| V4-graphs | 1920/256 | fanout/cross | blocking/on | native-runnable N4+R1 Q128 | 5 | 434,441 | 3.608 | 9,025 |
| V4-graphs | 1920/256 | fanout/cross | blocking/on | native-runnable N4+R1 Q32 | 5 | 525,584 | 2.385 | 7,605 |
| V4-graphs | 1920/256 | fanout/cross | blocking/on | native-runnable N4+R1 Q8 | 5 | 506,648 | 1.940 | 7,860 |
| V4-graphs | 1920/256 | chain/same | blocking/on | native-runnable N4+R1 Q1 | 5 | 442,345 | 2.550 | 9,015 |
| V4-graphs | 1920/256 | chain/same | blocking/on | native-runnable N4+R1 Q128 | 5 | 482,285 | 2.825 | 8,287 |
| V4-graphs | 1920/256 | chain/same | blocking/on | native-runnable N4+R1 Q32 | 5 | 473,367 | 2.728 | 8,446 |
| V4-graphs | 1920/256 | chain/same | blocking/on | native-runnable N4+R1 Q8 | 5 | 456,807 | 2.819 | 8,752 |
| V4-graphs | 1920/256 | fanout/same | blocking/on | native-runnable N4+R1 Q1 | 5 | 479,159 | 1.660 | 8,342 |
| V4-graphs | 1920/256 | fanout/same | blocking/on | native-runnable N4+R1 Q128 | 5 | 450,675 | 3.383 | 8,821 |
| V4-graphs | 1920/256 | fanout/same | blocking/on | native-runnable N4+R1 Q32 | 5 | 503,869 | 2.450 | 7,920 |
| V4-graphs | 1920/256 | fanout/same | blocking/on | native-runnable N4+R1 Q8 | 5 | 520,965 | 1.811 | 7,641 |
| V4-graphs | 1920/256 | chain/cross | async/on | native-steal N3+R1 | 5 | 317,475 | 2.340 | 9,404 |
| V4-graphs | 1920/256 | fanout/cross | async/on | native-steal N3+R1 | 5 | 347,043 | 2.653 | 8,634 |
| V4-graphs | 1920/256 | chain/same | async/on | native-steal N3+R1 | 5 | 325,984 | 3.269 | 9,189 |
| V4-graphs | 1920/256 | fanout/same | async/on | native-steal N3+R1 | 5 | 343,950 | 2.329 | 8,707 |
| V4-graphs | 1920/256 | chain/cross | blocking/on | native-steal N3+R1 | 5 | 335,367 | 2.948 | 8,942 |
| V4-graphs | 1920/256 | fanout/cross | blocking/on | native-steal N3+R1 | 5 | 353,995 | 2.268 | 8,468 |
| V4-graphs | 1920/256 | chain/same | blocking/on | native-steal N3+R1 | 5 | 330,905 | 3.418 | 9,063 |
| V4-graphs | 1920/256 | fanout/same | blocking/on | native-steal N3+R1 | 5 | 356,952 | 2.262 | 8,403 |
| V4-graphs | 1920/256 | chain/cross | async/on | native-steal N4+R1 | 5 | 343,926 | 4.203 | 11,334 |
| V4-graphs | 1920/256 | fanout/cross | async/on | native-steal N4+R1 | 5 | 402,017 | 2.118 | 9,870 |
| V4-graphs | 1920/256 | chain/same | async/on | native-steal N4+R1 | 5 | 329,408 | 4.430 | 11,464 |
| V4-graphs | 1920/256 | fanout/same | async/on | native-steal N4+R1 | 5 | 410,009 | 2.250 | 9,716 |
| V4-graphs | 1920/256 | chain/cross | blocking/on | native-steal N4+R1 | 5 | 360,519 | 3.703 | 10,783 |
| V4-graphs | 1920/256 | fanout/cross | blocking/on | native-steal N4+R1 | 5 | 393,750 | 2.840 | 9,935 |
| V4-graphs | 1920/256 | chain/same | blocking/on | native-steal N4+R1 | 5 | 355,120 | 3.896 | 11,192 |
| V4-graphs | 1920/256 | fanout/same | blocking/on | native-steal N4+R1 | 5 | 386,356 | 2.630 | 10,287 |
| V4-graphs | 1920/256 | chain/cross | async/on | sharded-16 N4 | 5 | 468,395 | 0.810 | 8,346 |
| V4-graphs | 1920/256 | fanout/cross | async/on | sharded-16 N4 | 5 | 527,242 | 0.848 | 7,186 |
| V4-graphs | 1920/256 | chain/same | async/on | sharded-16 N4 | 5 | 745,433 | 0.434 | 4,724 |
| V4-graphs | 1920/256 | fanout/same | async/on | sharded-16 N4 | 5 | 858,832 | 0.326 | 4,297 |
| V4-graphs | 1920/256 | chain/cross | blocking/on | sharded-16 N4 | 5 | 479,460 | 0.702 | 8,301 |
| V4-graphs | 1920/256 | fanout/cross | blocking/on | sharded-16 N4 | 5 | 592,636 | 0.576 | 6,703 |
| V4-graphs | 1920/256 | chain/same | blocking/on | sharded-16 N4 | 5 | 793,132 | 0.427 | 4,842 |
| V4-graphs | 1920/256 | fanout/same | blocking/on | sharded-16 N4 | 5 | 951,573 | 0.282 | 4,129 |
| V4-graphs | 1920/256 | chain/cross | async/on | tokio-16 N4 | 5 | 738,584 | 0.828 | 5,364 |
| V4-graphs | 1920/256 | fanout/cross | async/on | tokio-16 N4 | 5 | 866,623 | 0.575 | 4,596 |
| V4-graphs | 1920/256 | chain/same | async/on | tokio-16 N4 | 5 | 845,301 | 0.664 | 4,705 |
| V4-graphs | 1920/256 | fanout/same | async/on | tokio-16 N4 | 5 | 847,718 | 0.587 | 4,698 |
| V4-graphs | 1920/256 | chain/cross | blocking/on | tokio-16 N4 | 5 | 624,868 | 2.843 | 5,804 |
| V4-graphs | 1920/256 | fanout/cross | blocking/on | tokio-16 N4 | 5 | 863,781 | 0.592 | 4,570 |
| V4-graphs | 1920/256 | chain/same | blocking/on | tokio-16 N4 | 5 | 812,079 | 0.733 | 4,829 |
| V4-graphs | 1920/256 | fanout/same | blocking/on | tokio-16 N4 | 5 | 868,444 | 0.574 | 4,598 |
| V4-graphs | 1920/256 | chain/cross | async/on | tokio-pool N4 | 5 | 829,576 | 0.665 | 4,763 |
| V4-graphs | 1920/256 | fanout/cross | async/on | tokio-pool N4 | 5 | 960,949 | 0.496 | 4,143 |
| V4-graphs | 1920/256 | chain/same | async/on | tokio-pool N4 | 5 | 943,105 | 0.477 | 4,208 |
| V4-graphs | 1920/256 | fanout/same | async/on | tokio-pool N4 | 5 | 941,794 | 0.509 | 4,228 |
| V4-graphs | 1920/256 | chain/cross | blocking/on | tokio-pool N4 | 5 | 769,333 | 0.731 | 4,894 |
| V4-graphs | 1920/256 | fanout/cross | blocking/on | tokio-pool N4 | 5 | 875,992 | 0.584 | 4,151 |
| V4-graphs | 1920/256 | chain/same | blocking/on | tokio-pool N4 | 5 | 674,429 | 1.569 | 5,412 |
| V4-graphs | 1920/256 | fanout/same | blocking/on | tokio-pool N4 | 5 | 889,383 | 0.541 | 4,391 |
| V5-graphs | 1920/256 | chain/cross | async/on | async N4 | 5 | 920,500 | 0.490 | 4,321 |
| V5-graphs | 1920/256 | fanout/cross | async/on | async N4 | 5 | 916,158 | 0.520 | 4,347 |
| V5-graphs | 1920/256 | chain/same | async/on | async N4 | 5 | 925,632 | 0.487 | 4,302 |
| V5-graphs | 1920/256 | fanout/same | async/on | async N4 | 5 | 914,315 | 0.515 | 4,356 |
| V5-graphs | 1920/256 | chain/cross | blocking/on | async N4 | 5 | 911,451 | 0.499 | 4,385 |
| V5-graphs | 1920/256 | fanout/cross | blocking/on | async N4 | 5 | 923,776 | 0.524 | 4,309 |
| V5-graphs | 1920/256 | chain/same | blocking/on | async N4 | 5 | 904,284 | 0.508 | 4,420 |
| V5-graphs | 1920/256 | fanout/same | blocking/on | async N4 | 5 | 927,100 | 0.521 | 4,289 |
| V5-graphs | 1920/256 | chain/cross | async/on | async N5 | 5 | 918,929 | 2.820 | 4,303 |
| V5-graphs | 1920/256 | fanout/cross | async/on | async N5 | 5 | 946,593 | 2.811 | 4,205 |
| V5-graphs | 1920/256 | chain/same | async/on | async N5 | 5 | 911,339 | 2.804 | 4,360 |
| V5-graphs | 1920/256 | fanout/same | async/on | async N5 | 5 | 919,887 | 2.911 | 4,326 |
| V5-graphs | 1920/256 | chain/cross | blocking/on | async N5 | 5 | 912,146 | 2.761 | 4,374 |
| V5-graphs | 1920/256 | fanout/cross | blocking/on | async N5 | 5 | 943,970 | 2.779 | 4,233 |
| V5-graphs | 1920/256 | chain/same | blocking/on | async N5 | 5 | 911,455 | 2.705 | 4,384 |
| V5-graphs | 1920/256 | fanout/same | blocking/on | async N5 | 5 | 925,771 | 2.832 | 4,316 |
| V5-graphs | 1920/256 | chain/cross | async/on | native-deque N3+R1 Q8 | 5 | 429,816 | 2.351 | 6,955 |
| V5-graphs | 1920/256 | fanout/cross | async/on | native-deque N3+R1 Q8 | 5 | 488,374 | 2.788 | 6,129 |
| V5-graphs | 1920/256 | chain/same | async/on | native-deque N3+R1 Q8 | 5 | 450,533 | 2.273 | 6,636 |
| V5-graphs | 1920/256 | fanout/same | async/on | native-deque N3+R1 Q8 | 5 | 487,456 | 2.772 | 6,140 |
| V5-graphs | 1920/256 | chain/cross | blocking/on | native-deque N3+R1 Q8 | 5 | 458,303 | 2.285 | 6,523 |
| V5-graphs | 1920/256 | fanout/cross | blocking/on | native-deque N3+R1 Q8 | 5 | 499,973 | 2.718 | 5,987 |
| V5-graphs | 1920/256 | chain/same | blocking/on | native-deque N3+R1 Q8 | 5 | 470,046 | 2.128 | 6,377 |
| V5-graphs | 1920/256 | fanout/same | blocking/on | native-deque N3+R1 Q8 | 5 | 497,944 | 2.703 | 6,023 |
| V5-graphs | 1920/256 | chain/cross | async/on | native-deque N4+R1 Q1 | 5 | 501,674 | 1.587 | 7,946 |
| V5-graphs | 1920/256 | chain/cross | async/on | native-deque N4+R1 Q128 | 5 | 587,082 | 1.791 | 6,784 |
| V5-graphs | 1920/256 | chain/cross | async/on | native-deque N4+R1 Q32 | 5 | 596,426 | 1.772 | 6,675 |
| V5-graphs | 1920/256 | chain/cross | async/on | native-deque N4+R1 Q8 | 5 | 569,700 | 1.780 | 6,984 |
| V5-graphs | 1920/256 | fanout/cross | async/on | native-deque N4+R1 Q1 | 5 | 536,207 | 1.858 | 7,430 |
| V5-graphs | 1920/256 | fanout/cross | async/on | native-deque N4+R1 Q128 | 5 | 562,079 | 2.186 | 7,097 |
| V5-graphs | 1920/256 | fanout/cross | async/on | native-deque N4+R1 Q32 | 5 | 608,121 | 2.381 | 6,456 |
| V5-graphs | 1920/256 | fanout/cross | async/on | native-deque N4+R1 Q8 | 5 | 552,691 | 2.464 | 7,189 |
| V5-graphs | 1920/256 | chain/same | async/on | native-deque N4+R1 Q1 | 5 | 505,161 | 1.582 | 7,881 |
| V5-graphs | 1920/256 | chain/same | async/on | native-deque N4+R1 Q128 | 5 | 585,233 | 1.740 | 6,810 |
| V5-graphs | 1920/256 | chain/same | async/on | native-deque N4+R1 Q32 | 5 | 584,236 | 1.748 | 6,820 |
| V5-graphs | 1920/256 | chain/same | async/on | native-deque N4+R1 Q8 | 5 | 565,952 | 1.894 | 7,036 |
| V5-graphs | 1920/256 | fanout/same | async/on | native-deque N4+R1 Q1 | 5 | 543,343 | 1.824 | 7,344 |
| V5-graphs | 1920/256 | fanout/same | async/on | native-deque N4+R1 Q128 | 5 | 558,916 | 2.162 | 7,135 |
| V5-graphs | 1920/256 | fanout/same | async/on | native-deque N4+R1 Q32 | 5 | 624,084 | 2.222 | 6,388 |
| V5-graphs | 1920/256 | fanout/same | async/on | native-deque N4+R1 Q8 | 5 | 614,958 | 2.285 | 6,484 |
| V5-graphs | 1920/256 | chain/cross | blocking/on | native-deque N4+R1 Q1 | 5 | 518,215 | 1.557 | 7,675 |
| V5-graphs | 1920/256 | chain/cross | blocking/on | native-deque N4+R1 Q128 | 5 | 590,531 | 1.724 | 6,758 |
| V5-graphs | 1920/256 | chain/cross | blocking/on | native-deque N4+R1 Q32 | 5 | 592,953 | 1.929 | 6,708 |
| V5-graphs | 1920/256 | chain/cross | blocking/on | native-deque N4+R1 Q8 | 5 | 582,925 | 1.837 | 6,841 |
| V5-graphs | 1920/256 | fanout/cross | blocking/on | native-deque N4+R1 Q1 | 5 | 560,033 | 1.723 | 7,139 |
| V5-graphs | 1920/256 | fanout/cross | blocking/on | native-deque N4+R1 Q128 | 5 | 562,548 | 2.204 | 7,099 |
| V5-graphs | 1920/256 | fanout/cross | blocking/on | native-deque N4+R1 Q32 | 5 | 631,479 | 2.260 | 6,328 |
| V5-graphs | 1920/256 | fanout/cross | blocking/on | native-deque N4+R1 Q8 | 5 | 616,966 | 2.342 | 6,476 |
| V5-graphs | 1920/256 | chain/same | blocking/on | native-deque N4+R1 Q1 | 5 | 511,266 | 1.563 | 7,812 |
| V5-graphs | 1920/256 | chain/same | blocking/on | native-deque N4+R1 Q128 | 5 | 590,217 | 1.713 | 6,749 |
| V5-graphs | 1920/256 | chain/same | blocking/on | native-deque N4+R1 Q32 | 5 | 583,494 | 1.717 | 6,809 |
| V5-graphs | 1920/256 | chain/same | blocking/on | native-deque N4+R1 Q8 | 5 | 576,534 | 2.021 | 6,915 |
| V5-graphs | 1920/256 | fanout/same | blocking/on | native-deque N4+R1 Q1 | 5 | 554,506 | 1.737 | 7,112 |
| V5-graphs | 1920/256 | fanout/same | blocking/on | native-deque N4+R1 Q128 | 5 | 550,882 | 2.287 | 7,177 |
| V5-graphs | 1920/256 | fanout/same | blocking/on | native-deque N4+R1 Q32 | 5 | 624,048 | 2.262 | 6,406 |
| V5-graphs | 1920/256 | fanout/same | blocking/on | native-deque N4+R1 Q8 | 5 | 622,028 | 2.052 | 6,423 |
| V5-graphs | 1920/256 | chain/cross | async/on | native-root N4+R1 Q32 | 5 | 517,037 | 1.762 | 7,720 |
| V5-graphs | 1920/256 | chain/cross | async/on | native-root N4+R1 Q8 | 5 | 512,202 | 1.575 | 7,792 |
| V5-graphs | 1920/256 | fanout/cross | async/on | native-root N4+R1 Q32 | 5 | 547,004 | 1.894 | 7,294 |
| V5-graphs | 1920/256 | fanout/cross | async/on | native-root N4+R1 Q8 | 5 | 548,678 | 1.504 | 7,274 |
| V5-graphs | 1920/256 | chain/same | async/on | native-root N4+R1 Q32 | 5 | 510,293 | 2.000 | 7,820 |
| V5-graphs | 1920/256 | chain/same | async/on | native-root N4+R1 Q8 | 5 | 509,203 | 1.629 | 7,838 |
| V5-graphs | 1920/256 | fanout/same | async/on | native-root N4+R1 Q32 | 5 | 542,335 | 1.850 | 7,358 |
| V5-graphs | 1920/256 | fanout/same | async/on | native-root N4+R1 Q8 | 5 | 544,319 | 1.772 | 7,329 |
| V5-graphs | 1920/256 | chain/cross | blocking/on | native-root N4+R1 Q32 | 5 | 516,276 | 1.567 | 7,744 |
| V5-graphs | 1920/256 | chain/cross | blocking/on | native-root N4+R1 Q8 | 5 | 518,432 | 1.446 | 7,711 |
| V5-graphs | 1920/256 | fanout/cross | blocking/on | native-root N4+R1 Q32 | 5 | 555,732 | 1.955 | 7,174 |
| V5-graphs | 1920/256 | fanout/cross | blocking/on | native-root N4+R1 Q8 | 5 | 551,893 | 1.642 | 7,226 |
| V5-graphs | 1920/256 | chain/same | blocking/on | native-root N4+R1 Q32 | 5 | 508,987 | 2.212 | 7,855 |
| V5-graphs | 1920/256 | chain/same | blocking/on | native-root N4+R1 Q8 | 5 | 514,342 | 1.168 | 7,770 |
| V5-graphs | 1920/256 | fanout/same | blocking/on | native-root N4+R1 Q32 | 5 | 545,771 | 1.940 | 7,279 |
| V5-graphs | 1920/256 | fanout/same | blocking/on | native-root N4+R1 Q8 | 5 | 555,178 | 1.550 | 7,202 |
| V5-graphs | 1920/256 | chain/cross | async/on | native-runnable N4+R1 Q32 | 5 | 529,014 | 2.314 | 7,545 |
| V5-graphs | 1920/256 | chain/cross | async/on | native-runnable N4+R1 Q8 | 5 | 515,977 | 2.274 | 7,703 |
| V5-graphs | 1920/256 | fanout/cross | async/on | native-runnable N4+R1 Q32 | 5 | 557,853 | 1.907 | 7,148 |
| V5-graphs | 1920/256 | fanout/cross | async/on | native-runnable N4+R1 Q8 | 5 | 552,726 | 1.589 | 7,218 |
| V5-graphs | 1920/256 | chain/same | async/on | native-runnable N4+R1 Q32 | 5 | 515,145 | 2.334 | 7,735 |
| V5-graphs | 1920/256 | chain/same | async/on | native-runnable N4+R1 Q8 | 5 | 510,755 | 2.274 | 7,809 |
| V5-graphs | 1920/256 | fanout/same | async/on | native-runnable N4+R1 Q32 | 5 | 550,657 | 1.927 | 7,220 |
| V5-graphs | 1920/256 | fanout/same | async/on | native-runnable N4+R1 Q8 | 5 | 550,872 | 1.650 | 7,241 |
| V5-graphs | 1920/256 | chain/cross | blocking/on | native-runnable N4+R1 Q32 | 5 | 528,669 | 2.250 | 7,554 |
| V5-graphs | 1920/256 | chain/cross | blocking/on | native-runnable N4+R1 Q8 | 5 | 527,684 | 2.220 | 7,550 |
| V5-graphs | 1920/256 | fanout/cross | blocking/on | native-runnable N4+R1 Q32 | 5 | 573,761 | 1.678 | 6,968 |
| V5-graphs | 1920/256 | fanout/cross | blocking/on | native-runnable N4+R1 Q8 | 5 | 569,095 | 1.542 | 6,986 |
| V5-graphs | 1920/256 | chain/same | blocking/on | native-runnable N4+R1 Q32 | 5 | 520,902 | 2.318 | 7,651 |
| V5-graphs | 1920/256 | chain/same | blocking/on | native-runnable N4+R1 Q8 | 5 | 526,306 | 1.675 | 7,567 |
| V5-graphs | 1920/256 | fanout/same | blocking/on | native-runnable N4+R1 Q32 | 5 | 572,284 | 1.773 | 6,948 |
| V5-graphs | 1920/256 | fanout/same | blocking/on | native-runnable N4+R1 Q8 | 5 | 561,434 | 1.687 | 7,119 |
| V5-graphs | 1920/256 | chain/cross | async/on | native-steal N4+R1 | 5 | 424,285 | 2.760 | 9,394 |
| V5-graphs | 1920/256 | fanout/cross | async/on | native-steal N4+R1 | 5 | 453,973 | 1.791 | 8,794 |
| V5-graphs | 1920/256 | chain/same | async/on | native-steal N4+R1 | 5 | 411,287 | 2.953 | 9,703 |
| V5-graphs | 1920/256 | fanout/same | async/on | native-steal N4+R1 | 5 | 451,461 | 1.781 | 8,842 |
| V5-graphs | 1920/256 | chain/cross | blocking/on | native-steal N4+R1 | 5 | 427,460 | 1.561 | 9,354 |
| V5-graphs | 1920/256 | fanout/cross | blocking/on | native-steal N4+R1 | 5 | 459,816 | 1.735 | 8,669 |
| V5-graphs | 1920/256 | chain/same | blocking/on | native-steal N4+R1 | 5 | 421,845 | 2.033 | 9,449 |
| V5-graphs | 1920/256 | fanout/same | blocking/on | native-steal N4+R1 | 5 | 465,854 | 1.809 | 8,565 |
| V5-graphs | 1920/256 | chain/cross | async/on | sharded-16 N4 | 5 | 452,882 | 0.834 | 8,621 |
| V5-graphs | 1920/256 | fanout/cross | async/on | sharded-16 N4 | 5 | 581,490 | 0.588 | 6,628 |
| V5-graphs | 1920/256 | chain/same | async/on | sharded-16 N4 | 5 | 762,352 | 0.426 | 4,661 |
| V5-graphs | 1920/256 | fanout/same | async/on | sharded-16 N4 | 5 | 859,677 | 0.318 | 4,146 |
| V5-graphs | 1920/256 | chain/cross | blocking/on | sharded-16 N4 | 5 | 481,395 | 0.692 | 8,272 |
| V5-graphs | 1920/256 | fanout/cross | blocking/on | sharded-16 N4 | 5 | 603,537 | 0.563 | 6,606 |
| V5-graphs | 1920/256 | chain/same | blocking/on | sharded-16 N4 | 5 | 800,749 | 0.420 | 4,781 |
| V5-graphs | 1920/256 | fanout/same | blocking/on | sharded-16 N4 | 5 | 956,127 | 0.280 | 4,115 |
| V5-graphs | 1920/256 | chain/cross | async/on | tokio-pool N4 | 5 | 949,907 | 0.472 | 4,191 |
| V5-graphs | 1920/256 | fanout/cross | async/on | tokio-pool N4 | 5 | 952,688 | 0.491 | 4,174 |
| V5-graphs | 1920/256 | chain/same | async/on | tokio-pool N4 | 5 | 948,266 | 0.478 | 4,199 |
| V5-graphs | 1920/256 | fanout/same | async/on | tokio-pool N4 | 5 | 945,023 | 0.501 | 4,213 |
| V5-graphs | 1920/256 | chain/cross | blocking/on | tokio-pool N4 | 5 | 926,291 | 0.494 | 4,313 |
| V5-graphs | 1920/256 | fanout/cross | blocking/on | tokio-pool N4 | 5 | 952,538 | 0.507 | 4,195 |
| V5-graphs | 1920/256 | chain/same | blocking/on | tokio-pool N4 | 5 | 936,465 | 0.489 | 4,267 |
| V5-graphs | 1920/256 | fanout/same | blocking/on | tokio-pool N4 | 5 | 954,922 | 0.510 | 4,135 |
| V6-graphs | 1920/256 | chain/cross | async/on | async N4 | 5 | 943,042 | 0.472 | 4,222 |
| V6-graphs | 1920/256 | fanout/cross | async/on | async N4 | 5 | 922,771 | 0.512 | 4,315 |
| V6-graphs | 1920/256 | chain/same | async/on | async N4 | 5 | 932,061 | 0.480 | 4,274 |
| V6-graphs | 1920/256 | fanout/same | async/on | async N4 | 5 | 921,518 | 0.508 | 4,320 |
| V6-graphs | 1920/256 | chain/cross | async/on | async N5 | 5 | 938,251 | 2.643 | 4,242 |
| V6-graphs | 1920/256 | fanout/cross | async/on | async N5 | 5 | 929,597 | 2.881 | 4,280 |
| V6-graphs | 1920/256 | chain/same | async/on | async N5 | 5 | 938,376 | 2.607 | 4,236 |
| V6-graphs | 1920/256 | fanout/same | async/on | async N5 | 5 | 927,406 | 2.830 | 4,286 |
| V6-graphs | 1920/256 | chain/cross | async/on | native-deque N4+R1 Q8 Z0 W0 | 5 | 575,154 | 1.838 | 6,911 |
| V6-graphs | 1920/256 | chain/cross | async/on | native-deque N4+R1 Q8 Z1 W0 | 5 | 594,271 | 1.816 | 6,702 |
| V6-graphs | 1920/256 | chain/cross | async/on | native-deque N4+R1 Q8 Z0 W1 | 5 | 643,794 | 1.650 | 6,180 |
| V6-graphs | 1920/256 | chain/cross | async/on | native-deque N4+R1 Q8 Z1 W1 | 5 | 672,043 | 1.585 | 5,926 |
| V6-graphs | 1920/256 | fanout/cross | async/on | native-deque N4+R1 Q8 Z0 W0 | 5 | 618,511 | 2.192 | 6,449 |
| V6-graphs | 1920/256 | fanout/cross | async/on | native-deque N4+R1 Q8 Z1 W0 | 5 | 628,210 | 2.313 | 6,333 |
| V6-graphs | 1920/256 | fanout/cross | async/on | native-deque N4+R1 Q8 Z0 W1 | 5 | 699,707 | 2.043 | 5,701 |
| V6-graphs | 1920/256 | fanout/cross | async/on | native-deque N4+R1 Q8 Z1 W1 | 5 | 707,733 | 2.070 | 5,633 |
| V6-graphs | 1920/256 | chain/same | async/on | native-deque N4+R1 Q8 Z0 W0 | 5 | 565,007 | 1.813 | 7,039 |
| V6-graphs | 1920/256 | chain/same | async/on | native-deque N4+R1 Q8 Z1 W0 | 5 | 586,689 | 1.940 | 6,796 |
| V6-graphs | 1920/256 | chain/same | async/on | native-deque N4+R1 Q8 Z0 W1 | 5 | 641,400 | 1.769 | 6,203 |
| V6-graphs | 1920/256 | chain/same | async/on | native-deque N4+R1 Q8 Z1 W1 | 5 | 647,743 | 1.786 | 6,138 |
| V6-graphs | 1920/256 | fanout/same | async/on | native-deque N4+R1 Q8 Z0 W0 | 5 | 614,145 | 2.379 | 6,486 |
| V6-graphs | 1920/256 | fanout/same | async/on | native-deque N4+R1 Q8 Z1 W0 | 5 | 624,205 | 2.267 | 6,388 |
| V6-graphs | 1920/256 | fanout/same | async/on | native-deque N4+R1 Q8 Z0 W1 | 5 | 687,779 | 2.123 | 5,797 |
| V6-graphs | 1920/256 | fanout/same | async/on | native-deque N4+R1 Q8 Z1 W1 | 5 | 675,280 | 2.146 | 5,900 |
| V6-graphs | 1920/256 | chain/cross | async/on | native-root N4+R1 Q8 W0 | 5 | 517,754 | 1.399 | 7,705 |
| V6-graphs | 1920/256 | chain/cross | async/on | native-root N4+R1 Q8 W1 | 5 | 596,682 | 1.185 | 6,685 |
| V6-graphs | 1920/256 | fanout/cross | async/on | native-root N4+R1 Q8 W0 | 5 | 552,782 | 1.612 | 7,214 |
| V6-graphs | 1920/256 | fanout/cross | async/on | native-root N4+R1 Q8 W1 | 5 | 627,601 | 1.338 | 6,334 |
| V6-graphs | 1920/256 | chain/same | async/on | native-root N4+R1 Q8 W0 | 5 | 512,718 | 1.426 | 7,787 |
| V6-graphs | 1920/256 | chain/same | async/on | native-root N4+R1 Q8 W1 | 5 | 602,325 | 1.284 | 6,610 |
| V6-graphs | 1920/256 | fanout/same | async/on | native-root N4+R1 Q8 W0 | 5 | 549,302 | 1.603 | 7,266 |
| V6-graphs | 1920/256 | fanout/same | async/on | native-root N4+R1 Q8 W1 | 5 | 587,248 | 1.551 | 6,703 |
| V6-graphs | 1920/256 | chain/cross | async/on | native-runnable N4+R1 Q8 Z0 W0 | 5 | 529,822 | 2.207 | 7,518 |
| V6-graphs | 1920/256 | chain/cross | async/on | native-runnable N4+R1 Q8 Z1 W0 | 5 | 539,675 | 2.171 | 7,392 |
| V6-graphs | 1920/256 | chain/cross | async/on | native-runnable N4+R1 Q8 Z0 W1 | 5 | 606,817 | 1.667 | 6,570 |
| V6-graphs | 1920/256 | chain/cross | async/on | native-runnable N4+R1 Q8 Z1 W1 | 5 | 623,903 | 1.569 | 6,391 |
| V6-graphs | 1920/256 | fanout/cross | async/on | native-runnable N4+R1 Q8 Z0 W0 | 5 | 561,506 | 1.551 | 7,107 |
| V6-graphs | 1920/256 | fanout/cross | async/on | native-runnable N4+R1 Q8 Z1 W0 | 5 | 570,963 | 1.605 | 6,989 |
| V6-graphs | 1920/256 | fanout/cross | async/on | native-runnable N4+R1 Q8 Z0 W1 | 5 | 648,136 | 1.292 | 6,155 |
| V6-graphs | 1920/256 | fanout/cross | async/on | native-runnable N4+R1 Q8 Z1 W1 | 5 | 661,232 | 1.384 | 6,030 |
| V6-graphs | 1920/256 | chain/same | async/on | native-runnable N4+R1 Q8 Z0 W0 | 5 | 517,480 | 2.327 | 7,704 |
| V6-graphs | 1920/256 | chain/same | async/on | native-runnable N4+R1 Q8 Z1 W0 | 5 | 532,234 | 2.118 | 7,492 |
| V6-graphs | 1920/256 | chain/same | async/on | native-runnable N4+R1 Q8 Z0 W1 | 5 | 601,909 | 1.165 | 6,624 |
| V6-graphs | 1920/256 | chain/same | async/on | native-runnable N4+R1 Q8 Z1 W1 | 5 | 619,165 | 1.448 | 6,413 |
| V6-graphs | 1920/256 | fanout/same | async/on | native-runnable N4+R1 Q8 Z0 W0 | 5 | 559,920 | 1.420 | 7,130 |
| V6-graphs | 1920/256 | fanout/same | async/on | native-runnable N4+R1 Q8 Z1 W0 | 5 | 541,164 | 1.618 | 7,377 |
| V6-graphs | 1920/256 | fanout/same | async/on | native-runnable N4+R1 Q8 Z0 W1 | 5 | 645,296 | 1.321 | 6,178 |
| V6-graphs | 1920/256 | fanout/same | async/on | native-runnable N4+R1 Q8 Z1 W1 | 5 | 644,023 | 1.332 | 6,174 |
| V7-graphs | 1920/256 | chain/cross | async/on | async N4 | 5 | 922,031 | 0.487 | 4,319 |
| V7-graphs | 1920/256 | fanout/cross | async/on | async N4 | 5 | 916,521 | 0.514 | 4,343 |
| V7-graphs | 1920/256 | chain/same | async/on | async N4 | 5 | 926,715 | 0.485 | 4,298 |
| V7-graphs | 1920/256 | fanout/same | async/on | async N4 | 5 | 910,829 | 0.517 | 4,371 |
| V7-graphs | 1920/256 | chain/cross | async/on | async N5 | 5 | 925,372 | 2.808 | 4,303 |
| V7-graphs | 1920/256 | fanout/cross | async/on | async N5 | 5 | 923,248 | 2.952 | 4,310 |
| V7-graphs | 1920/256 | chain/same | async/on | async N5 | 5 | 928,959 | 2.759 | 4,281 |
| V7-graphs | 1920/256 | fanout/same | async/on | async N5 | 5 | 928,997 | 2.818 | 4,286 |
| V7-graphs | 1920/256 | chain/cross | async/on | native-deque N4+R1 Q8 Z0 W0 I0 | 5 | 576,448 | 1.951 | 6,908 |
| V7-graphs | 1920/256 | chain/cross | async/on | native-deque N4+R1 Q8 Z1 W1 I0 | 5 | 657,656 | 1.622 | 6,051 |
| V7-graphs | 1920/256 | chain/cross | async/on | native-deque N4+R1 Q8 Z0 W0 I1 | 5 | 580,839 | 1.737 | 6,846 |
| V7-graphs | 1920/256 | chain/cross | async/on | native-deque N4+R1 Q8 Z1 W1 I1 | 5 | 659,978 | 1.590 | 6,029 |
| V7-graphs | 1920/256 | fanout/cross | async/on | native-deque N4+R1 Q8 Z0 W0 I0 | 5 | 624,226 | 2.334 | 6,389 |
| V7-graphs | 1920/256 | fanout/cross | async/on | native-deque N4+R1 Q8 Z1 W1 I0 | 5 | 711,242 | 1.915 | 5,608 |
| V7-graphs | 1920/256 | fanout/cross | async/on | native-deque N4+R1 Q8 Z0 W0 I1 | 5 | 625,362 | 2.360 | 6,379 |
| V7-graphs | 1920/256 | fanout/cross | async/on | native-deque N4+R1 Q8 Z1 W1 I1 | 5 | 710,892 | 1.878 | 5,604 |
| V7-graphs | 1920/256 | chain/same | async/on | native-deque N4+R1 Q8 Z0 W0 I0 | 5 | 570,768 | 1.846 | 6,977 |
| V7-graphs | 1920/256 | chain/same | async/on | native-deque N4+R1 Q8 Z1 W1 I0 | 5 | 663,260 | 1.420 | 6,006 |
| V7-graphs | 1920/256 | chain/same | async/on | native-deque N4+R1 Q8 Z0 W0 I1 | 5 | 583,991 | 1.721 | 6,826 |
| V7-graphs | 1920/256 | chain/same | async/on | native-deque N4+R1 Q8 Z1 W1 I1 | 5 | 651,330 | 1.639 | 6,108 |
| V7-graphs | 1920/256 | fanout/same | async/on | native-deque N4+R1 Q8 Z0 W0 I0 | 5 | 616,301 | 2.207 | 6,472 |
| V7-graphs | 1920/256 | fanout/same | async/on | native-deque N4+R1 Q8 Z1 W1 I0 | 5 | 693,840 | 2.123 | 5,743 |
| V7-graphs | 1920/256 | fanout/same | async/on | native-deque N4+R1 Q8 Z0 W0 I1 | 5 | 603,920 | 2.439 | 6,591 |
| V7-graphs | 1920/256 | fanout/same | async/on | native-deque N4+R1 Q8 Z1 W1 I1 | 5 | 698,962 | 2.112 | 5,702 |
| V7-graphs | 1920/256 | chain/cross | async/on | native-root N4+R1 Q8 W0 | 5 | 492,915 | 1.813 | 8,100 |
| V7-graphs | 1920/256 | fanout/cross | async/on | native-root N4+R1 Q8 W0 | 5 | 547,044 | 1.618 | 7,279 |
| V7-graphs | 1920/256 | chain/same | async/on | native-root N4+R1 Q8 W0 | 5 | 504,700 | 2.144 | 7,883 |
| V7-graphs | 1920/256 | fanout/same | async/on | native-root N4+R1 Q8 W0 | 5 | 541,465 | 1.631 | 7,343 |
| V7-graphs | 1920/256 | chain/cross | async/on | native-runnable N4+R1 Q8 Z0 W0 I0 | 5 | 529,087 | 2.064 | 7,543 |
| V7-graphs | 1920/256 | chain/cross | async/on | native-runnable N4+R1 Q8 Z1 W1 I0 | 5 | 622,911 | 1.759 | 6,400 |
| V7-graphs | 1920/256 | chain/cross | async/on | native-runnable N4+R1 Q8 Z0 W0 I1 | 5 | 541,170 | 1.930 | 7,370 |
| V7-graphs | 1920/256 | chain/cross | async/on | native-runnable N4+R1 Q8 Z1 W1 I1 | 5 | 629,457 | 1.164 | 6,337 |
| V7-graphs | 1920/256 | fanout/cross | async/on | native-runnable N4+R1 Q8 Z0 W0 I0 | 5 | 560,120 | 1.641 | 7,125 |
| V7-graphs | 1920/256 | fanout/cross | async/on | native-runnable N4+R1 Q8 Z1 W1 I0 | 5 | 654,439 | 1.308 | 6,097 |
| V7-graphs | 1920/256 | fanout/cross | async/on | native-runnable N4+R1 Q8 Z0 W0 I1 | 5 | 569,573 | 1.595 | 7,009 |
| V7-graphs | 1920/256 | fanout/cross | async/on | native-runnable N4+R1 Q8 Z1 W1 I1 | 5 | 661,575 | 1.331 | 6,032 |
| V7-graphs | 1920/256 | chain/same | async/on | native-runnable N4+R1 Q8 Z0 W0 I0 | 5 | 524,843 | 2.210 | 7,604 |
| V7-graphs | 1920/256 | chain/same | async/on | native-runnable N4+R1 Q8 Z1 W1 I0 | 5 | 616,367 | 1.158 | 6,466 |
| V7-graphs | 1920/256 | chain/same | async/on | native-runnable N4+R1 Q8 Z0 W0 I1 | 5 | 534,242 | 1.950 | 7,466 |
| V7-graphs | 1920/256 | chain/same | async/on | native-runnable N4+R1 Q8 Z1 W1 I1 | 5 | 616,129 | 1.137 | 6,470 |
| V7-graphs | 1920/256 | fanout/same | async/on | native-runnable N4+R1 Q8 Z0 W0 I0 | 5 | 555,255 | 1.616 | 7,187 |
| V7-graphs | 1920/256 | fanout/same | async/on | native-runnable N4+R1 Q8 Z1 W1 I0 | 5 | 650,711 | 1.310 | 6,127 |
| V7-graphs | 1920/256 | fanout/same | async/on | native-runnable N4+R1 Q8 Z0 W0 I1 | 5 | 560,315 | 1.634 | 7,118 |
| V7-graphs | 1920/256 | fanout/same | async/on | native-runnable N4+R1 Q8 Z1 W1 I1 | 5 | 653,275 | 1.339 | 6,104 |
| V7-scale | 16128/1024 | chain/cross | async/on | async N8 | 3 | 1,408,876 | 1.930 | 5,143 |
| V7-scale | 16128/1024 | fanout/cross | async/on | async N8 | 3 | 1,474,577 | 1.456 | 5,115 |
| V7-scale | 16128/1024 | chain/same | async/on | async N8 | 3 | 1,563,235 | 1.099 | 5,048 |
| V7-scale | 16128/1024 | fanout/same | async/on | async N8 | 3 | 1,479,994 | 1.315 | 5,186 |
| V7-scale | 16128/1024 | chain/cross | async/on | async N9 | 3 | 1,391,763 | 2.982 | 5,372 |
| V7-scale | 16128/1024 | fanout/cross | async/on | async N9 | 3 | 1,358,539 | 2.387 | 5,465 |
| V7-scale | 16128/1024 | chain/same | async/on | async N9 | 3 | 1,492,430 | 2.317 | 5,263 |
| V7-scale | 16128/1024 | fanout/same | async/on | async N9 | 3 | 1,434,194 | 2.169 | 5,448 |
| V7-scale | 16128/1024 | chain/cross | async/on | native-deque N7+R1 Q8 Z1 W1 I1 | 3 | 664,812 | 11.130 | 10,415 |
| V7-scale | 16128/1024 | fanout/cross | async/on | native-deque N7+R1 Q8 Z1 W1 I1 | 3 | 742,892 | 9.177 | 9,362 |
| V7-scale | 16128/1024 | chain/same | async/on | native-deque N7+R1 Q8 Z1 W1 I1 | 3 | 655,484 | 11.359 | 10,430 |
| V7-scale | 16128/1024 | fanout/same | async/on | native-deque N7+R1 Q8 Z1 W1 I1 | 3 | 718,340 | 12.045 | 9,579 |
| V7-scale | 16128/1024 | chain/cross | async/on | native-deque N8+R1 Q8 Z1 W1 I0 | 3 | 756,131 | 9.977 | 10,435 |
| V7-scale | 16128/1024 | chain/cross | async/on | native-deque N8+R1 Q8 Z1 W1 I1 | 3 | 754,472 | 9.144 | 10,520 |
| V7-scale | 16128/1024 | fanout/cross | async/on | native-deque N8+R1 Q8 Z1 W1 I0 | 3 | 828,578 | 10.657 | 9,506 |
| V7-scale | 16128/1024 | fanout/cross | async/on | native-deque N8+R1 Q8 Z1 W1 I1 | 3 | 827,115 | 11.121 | 9,581 |
| V7-scale | 16128/1024 | chain/same | async/on | native-deque N8+R1 Q8 Z1 W1 I0 | 3 | 744,531 | 10.288 | 10,600 |
| V7-scale | 16128/1024 | chain/same | async/on | native-deque N8+R1 Q8 Z1 W1 I1 | 3 | 733,148 | 9.614 | 10,737 |
| V7-scale | 16128/1024 | fanout/same | async/on | native-deque N8+R1 Q8 Z1 W1 I0 | 3 | 821,838 | 8.562 | 9,614 |
| V7-scale | 16128/1024 | fanout/same | async/on | native-deque N8+R1 Q8 Z1 W1 I1 | 3 | 820,896 | 11.178 | 9,649 |
| V7-scale | 16128/1024 | chain/cross | async/on | native-root N8+R1 Q8 W1 | 3 | 657,592 | 4.533 | 12,018 |
| V7-scale | 16128/1024 | fanout/cross | async/on | native-root N8+R1 Q8 W1 | 3 | 714,219 | 8.610 | 11,025 |
| V7-scale | 16128/1024 | chain/same | async/on | native-root N8+R1 Q8 W1 | 3 | 641,030 | 7.798 | 12,405 |
| V7-scale | 16128/1024 | fanout/same | async/on | native-root N8+R1 Q8 W1 | 3 | 683,342 | 8.393 | 11,474 |
| V7-scale | 16128/1024 | chain/cross | async/on | native-runnable N8+R1 Q8 Z1 W1 I0 | 3 | 689,489 | 7.684 | 11,456 |
| V7-scale | 16128/1024 | chain/cross | async/on | native-runnable N8+R1 Q8 Z1 W1 I1 | 3 | 679,968 | 7.942 | 11,728 |
| V7-scale | 16128/1024 | fanout/cross | async/on | native-runnable N8+R1 Q8 Z1 W1 I0 | 3 | 758,769 | 8.081 | 10,389 |
| V7-scale | 16128/1024 | fanout/cross | async/on | native-runnable N8+R1 Q8 Z1 W1 I1 | 3 | 749,284 | 7.412 | 10,625 |
| V7-scale | 16128/1024 | chain/same | async/on | native-runnable N8+R1 Q8 Z1 W1 I0 | 3 | 636,084 | 7.492 | 12,320 |
| V7-scale | 16128/1024 | chain/same | async/on | native-runnable N8+R1 Q8 Z1 W1 I1 | 3 | 659,756 | 7.697 | 12,049 |
| V7-scale | 16128/1024 | fanout/same | async/on | native-runnable N8+R1 Q8 Z1 W1 I0 | 3 | 716,112 | 8.027 | 11,011 |
| V7-scale | 16128/1024 | fanout/same | async/on | native-runnable N8+R1 Q8 Z1 W1 I1 | 3 | 711,175 | 7.673 | 11,154 |
| V8-graphs | 1920/256 | chain/cross | async/on | async N4 | 5 | 885,316 | 0.533 | 4,483 |
| V8-graphs | 1920/256 | fanout/cross | async/on | async N4 | 5 | 904,793 | 0.528 | 4,361 |
| V8-graphs | 1920/256 | chain/same | async/on | async N4 | 5 | 821,364 | 0.601 | 4,759 |
| V8-graphs | 1920/256 | fanout/same | async/on | async N4 | 5 | 909,876 | 0.525 | 4,375 |
| V8-graphs | 1920/256 | chain/cross | async/on | async N5 | 5 | 900,905 | 2.888 | 4,409 |
| V8-graphs | 1920/256 | fanout/cross | async/on | async N5 | 5 | 822,158 | 3.114 | 4,829 |
| V8-graphs | 1920/256 | chain/same | async/on | async N5 | 5 | 910,794 | 2.958 | 4,371 |
| V8-graphs | 1920/256 | fanout/same | async/on | async N5 | 5 | 894,443 | 2.892 | 4,443 |
| V8-graphs | 1920/256 | chain/cross | async/on | native-deque N4+R1 Q8 Z0 W0 | 5 | 517,990 | 1.996 | 7,655 |
| V8-graphs | 1920/256 | chain/cross | async/on | native-deque N4+R1 Q8 Z1 W1 | 5 | 603,716 | 1.704 | 6,585 |
| V8-graphs | 1920/256 | fanout/cross | async/on | native-deque N4+R1 Q8 Z0 W0 | 5 | 568,914 | 2.554 | 6,949 |
| V8-graphs | 1920/256 | fanout/cross | async/on | native-deque N4+R1 Q8 Z1 W1 | 5 | 641,552 | 2.177 | 6,195 |
| V8-graphs | 1920/256 | chain/same | async/on | native-deque N4+R1 Q8 Z0 W0 | 5 | 541,895 | 1.823 | 7,323 |
| V8-graphs | 1920/256 | chain/same | async/on | native-deque N4+R1 Q8 Z1 W1 | 5 | 586,206 | 1.794 | 6,685 |
| V8-graphs | 1920/256 | fanout/same | async/on | native-deque N4+R1 Q8 Z0 W0 | 5 | 595,891 | 2.343 | 6,680 |
| V8-graphs | 1920/256 | fanout/same | async/on | native-deque N4+R1 Q8 Z1 W1 | 5 | 698,264 | 2.044 | 5,693 |
| V8-graphs | 1920/256 | chain/cross | async/on | native-root N4+R1 Q8 W1 | 5 | 539,815 | 2.021 | 7,313 |
| V8-graphs | 1920/256 | fanout/cross | async/on | native-root N4+R1 Q8 W1 | 5 | 621,894 | 1.692 | 6,411 |
| V8-graphs | 1920/256 | chain/same | async/on | native-root N4+R1 Q8 W1 | 5 | 571,990 | 1.772 | 6,972 |
| V8-graphs | 1920/256 | fanout/same | async/on | native-root N4+R1 Q8 W1 | 5 | 576,957 | 2.107 | 6,899 |
| V8-graphs | 1920/256 | chain/cross | async/on | native-runnable N4+R1 Q8 Z1 W1 | 5 | 577,898 | 2.303 | 6,895 |
| V8-graphs | 1920/256 | fanout/cross | async/on | native-runnable N4+R1 Q8 Z1 W1 | 5 | 647,312 | 1.510 | 6,140 |
| V8-graphs | 1920/256 | chain/same | async/on | native-runnable N4+R1 Q8 Z1 W1 | 5 | 579,084 | 2.058 | 6,866 |
| V8-graphs | 1920/256 | fanout/same | async/on | native-runnable N4+R1 Q8 Z1 W1 | 5 | 628,352 | 1.943 | 6,294 |
| V8-scale | 16128/1024 | chain/cross | async/on | async N8 | 3 | 1,399,374 | 1.569 | 5,431 |
| V8-scale | 16128/1024 | fanout/cross | async/on | async N8 | 3 | 1,437,310 | 1.445 | 5,299 |
| V8-scale | 16128/1024 | chain/same | async/on | async N8 | 3 | 1,471,793 | 1.216 | 5,226 |
| V8-scale | 16128/1024 | fanout/same | async/on | async N8 | 3 | 1,329,729 | 1.698 | 5,799 |
| V8-scale | 16128/1024 | chain/cross | async/on | async N9 | 3 | 1,142,224 | 3.617 | 6,432 |
| V8-scale | 16128/1024 | fanout/cross | async/on | async N9 | 3 | 1,288,628 | 2.931 | 5,785 |
| V8-scale | 16128/1024 | chain/same | async/on | async N9 | 3 | 1,362,887 | 2.810 | 5,707 |
| V8-scale | 16128/1024 | fanout/same | async/on | async N9 | 3 | 1,275,473 | 2.819 | 5,963 |
| V8-scale | 16128/1024 | chain/cross | async/on | native-deque N8+R1 Q8 Z0 W0 | 3 | 508,283 | 14.908 | 15,505 |
| V8-scale | 16128/1024 | chain/cross | async/on | native-deque N8+R1 Q8 Z1 W1 | 3 | 731,672 | 10.823 | 10,749 |
| V8-scale | 16128/1024 | fanout/cross | async/on | native-deque N8+R1 Q8 Z0 W0 | 3 | 556,462 | 15.605 | 14,090 |
| V8-scale | 16128/1024 | fanout/cross | async/on | native-deque N8+R1 Q8 Z1 W1 | 3 | 758,893 | 11.522 | 10,202 |
| V8-scale | 16128/1024 | chain/same | async/on | native-deque N8+R1 Q8 Z0 W0 | 3 | 526,318 | 15.136 | 14,889 |
| V8-scale | 16128/1024 | chain/same | async/on | native-deque N8+R1 Q8 Z1 W1 | 3 | 728,194 | 10.639 | 10,862 |
| V8-scale | 16128/1024 | fanout/same | async/on | native-deque N8+R1 Q8 Z0 W0 | 3 | 573,745 | 15.332 | 13,798 |
| V8-scale | 16128/1024 | fanout/same | async/on | native-deque N8+R1 Q8 Z1 W1 | 3 | 695,494 | 12.143 | 11,329 |
| V8-scale | 16128/1024 | chain/cross | async/on | native-root N8+R1 Q8 W1 | 3 | 589,997 | 8.794 | 13,063 |
| V8-scale | 16128/1024 | fanout/cross | async/on | native-root N8+R1 Q8 W1 | 3 | 693,989 | 9.237 | 11,339 |
| V8-scale | 16128/1024 | chain/same | async/on | native-root N8+R1 Q8 W1 | 3 | 615,086 | 9.613 | 12,820 |
| V8-scale | 16128/1024 | fanout/same | async/on | native-root N8+R1 Q8 W1 | 3 | 651,530 | 10.035 | 11,897 |
| V8-scale | 16128/1024 | chain/cross | async/on | native-runnable N8+R1 Q8 Z1 W1 | 3 | 640,152 | 9.077 | 12,100 |
| V8-scale | 16128/1024 | fanout/cross | async/on | native-runnable N8+R1 Q8 Z1 W1 | 3 | 703,477 | 8.121 | 11,293 |
| V8-scale | 16128/1024 | chain/same | async/on | native-runnable N8+R1 Q8 Z1 W1 | 3 | 617,887 | 10.705 | 12,630 |
| V8-scale | 16128/1024 | fanout/same | async/on | native-runnable N8+R1 Q8 Z1 W1 | 3 | 673,234 | 9.559 | 11,726 |

Executor CPU sums all recorded runtime threads, including I/O/owner/blocking helpers; producer threads are excluded. It includes the start gate and drain; timed root elapsed excludes start padding. CPU/root can exceed wall ns/root. Whole-process CPU/RSS additionally include producers, samples, startup and sorting. Kernel schedstat is not a migration/cache measurement.

## Caller and counter controls

![V2-callers-factorial](graphs/V2-callers-factorial.png)

## Count-only versus time-limited turns

Slice 0 uses only the message-count budget; 25/50/200 µs also read the clock between completed handlers. Neither preempts a running handler. Tokio cooperative budgeting is separate. Empty mailboxes may never reach the explicit turn limit.

![V3-clock-slice](graphs/V3-clock-slice.png)

## Offered-load fairness

240 actors; four hot actors receive 80% of traffic, hot work 5,000 iterations and cold work 100. Spread/concentrated are hot actor ID layouts: static shards enforce worker placement, while pooled schedulers can move actors. Common absolute rates 500k, 900k and 1.2M requests/s; two-second injection windows; caller deadline two seconds; all admitted work drains. Steady and 1,000-request bursts remain separate. × means at least one repetition exceeded the unchanged validity threshold (>1% requests >1 ms late). Faint crosses retain those diagnostic aggregates; lines/circles use fully valid groups and break at invalid groups. Invalid schedules cannot be ranked as equivalent offered traffic. Survivor completion p99 excludes timeouts; observed deadline miss fractions accompany censored observations.

![V3-offered-control-offered-spread-burst1](graphs/V3-offered-control-offered-spread-burst1.png)

![V3-offered-control-offered-spread-burst1000](graphs/V3-offered-control-offered-spread-burst1000.png)

![V3-offered-control-offered-spread-timeseries](graphs/V3-offered-control-offered-spread-timeseries.png)

![V3-offered-control-offered-concentrated-burst1](graphs/V3-offered-control-offered-concentrated-burst1.png)

![V3-offered-control-offered-concentrated-burst1000](graphs/V3-offered-control-offered-concentrated-burst1000.png)

![V3-offered-control-offered-concentrated-timeseries](graphs/V3-offered-control-offered-concentrated-timeseries.png)

Valid schedules: **316/432**. All invalid runs remain in raw data and the per-cell summary.

![V4-offered-runnable-offered-spread-burst1](graphs/V4-offered-runnable-offered-spread-burst1.png)

![V4-offered-runnable-offered-spread-burst1000](graphs/V4-offered-runnable-offered-spread-burst1000.png)

![V4-offered-runnable-offered-spread-timeseries](graphs/V4-offered-runnable-offered-spread-timeseries.png)

![V4-offered-runnable-offered-concentrated-burst1](graphs/V4-offered-runnable-offered-concentrated-burst1.png)

![V4-offered-runnable-offered-concentrated-burst1000](graphs/V4-offered-runnable-offered-concentrated-burst1000.png)

![V4-offered-runnable-offered-concentrated-timeseries](graphs/V4-offered-runnable-offered-concentrated-timeseries.png)

Valid schedules: **177/252**. All invalid runs remain in raw data and the per-cell summary.

![V8-offered-frontier-offered-spread-burst1](graphs/V8-offered-frontier-offered-spread-burst1.png)

![V8-offered-frontier-offered-spread-burst1000](graphs/V8-offered-frontier-offered-spread-burst1000.png)

![V8-offered-frontier-offered-spread-timeseries](graphs/V8-offered-frontier-offered-spread-timeseries.png)

![V8-offered-frontier-offered-concentrated-burst1](graphs/V8-offered-frontier-offered-concentrated-burst1.png)

![V8-offered-frontier-offered-concentrated-burst1000](graphs/V8-offered-frontier-offered-concentrated-burst1000.png)

![V8-offered-frontier-offered-concentrated-timeseries](graphs/V8-offered-frontier-offered-concentrated-timeseries.png)

Valid schedules: **130/176**. All invalid runs remain in raw data and the per-cell summary.

## Pinned pipeline: small fixed deployments

V9 is a separate streaming fixture: N sequential actor stages on N actor cores, with either one pipeline (one actor/core) or 16 pipelines (16 actors/core). Every root performs the same 800-step LCG computation, divided across stages, and carries one end-to-end reply channel. Each stage forwards an ordinary owned message through an addressed boxed mailbox. There are 256 logical producers on four fixed driver cores 10–13. The three variants share this fixture and masks: Tokio N unpinned workers within CPUs 2..N+1, or individually pinned current-thread Tokio shards, parked/spinning. Pinned worker assignment is audited before warmup. Spin keeps a yielding owner runnable; it retains Tokio scheduling and is not Flux broadcast/overwrite transport. The new fixture does not isolate affinity from batching/sharding/spin and is not pooled with earlier eight-hop recursive request/reply graphs. N=8 crosses asymmetric L3 domains; compute is fixed but inter-stage messages grow with N, so core scaling also changes communication. Three fresh processes/cell; whiskers are min/max, not confidence intervals. Whole-process CPU includes driver and runtime threads, not isolated actor CPU or electrical power. The idle interval is one second after warmup; process CPU tick resolution limits tiny parked values.

![V9-pinned-groups1](graphs/V9-pinned-groups1.png)

![V9-pinned-groups16](graphs/V9-pinned-groups16.png)

| Actors/core | Actor cores | Profile | Median roots/s | Observed p99 ms | Idle process busy-core equivalents | Active process CPU µs/root |
|---:|---:|---|---:|---:|---:|---:|
| 1 | 1 | async | 684,098 | 0.476 | 0.00 | 3.15 |
| 1 | 1 | pinned-park | 648,524 | 0.495 | 0.00 | 3.31 |
| 1 | 1 | pinned-spin | 650,342 | 0.563 | 1.00 | 3.38 |
| 1 | 2 | async | 1,029,757 | 0.306 | 0.00 | 2.97 |
| 1 | 2 | pinned-park | 950,600 | 0.328 | 0.00 | 3.40 |
| 1 | 2 | pinned-spin | 1,027,693 | 0.446 | 2.00 | 3.21 |
| 1 | 4 | async | 1,226,625 | 0.331 | 0.00 | 2.61 |
| 1 | 4 | pinned-park | 1,162,851 | 0.255 | 0.00 | 3.76 |
| 1 | 4 | pinned-spin | 1,211,613 | 0.261 | 4.00 | 4.60 |
| 1 | 8 | async | 1,012,110 | 0.537 | 0.00 | 3.38 |
| 1 | 8 | pinned-park | 1,877,006 | 0.185 | 0.00 | 4.21 |
| 1 | 8 | pinned-spin | 2,105,494 | 0.180 | 8.00 | 4.57 |
| 16 | 1 | async | 631,190 | 5.280 | 0.00 | 3.34 |
| 16 | 1 | pinned-park | 653,609 | 0.772 | 0.00 | 3.36 |
| 16 | 1 | pinned-spin | 645,267 | 0.781 | 1.00 | 3.40 |
| 16 | 2 | async | 1,036,544 | 1.041 | 0.00 | 3.05 |
| 16 | 2 | pinned-park | 900,958 | 1.420 | 0.00 | 3.37 |
| 16 | 2 | pinned-spin | 980,374 | 0.542 | 2.00 | 3.84 |
| 16 | 4 | async | 1,279,623 | 0.418 | 0.00 | 3.99 |
| 16 | 4 | pinned-park | 1,277,991 | 0.721 | 0.00 | 3.40 |
| 16 | 4 | pinned-spin | 1,206,873 | 0.762 | 4.00 | 4.63 |
| 16 | 8 | async | 901,193 | 0.771 | 0.00 | 6.78 |
| 16 | 8 | pinned-park | 2,038,631 | 0.415 | 0.00 | 3.83 |
| 16 | 8 | pinned-spin | 2,403,235 | 0.134 | 8.01 | 3.95 |

## Lifecycle and resources

Closed-loop lifecycle churn maintains 1,024 idle residents and creates/stops 128 actors per cycle, with 16 stateful requests per actor, helper/timer awaits, exact monitor IDs/reasons and periodic intentional handler panic. Completed V3/V4 resource configurations and the corrected-source V8 follow-up request 300-second windows, chosen before their resource outcomes; exact requested durations remain in metadata. Unstarted V5 windows were cancelled after review found a fairness defect. Histories are bounded per cycle; 1 Hz resource samples stream to disk. The final resource sample precedes terminal executor shutdown, so descriptor/thread counts include still-live runtime infrastructure. Successful process exit also requires the subsequent explicit shutdown; sampled counts do not show the post-shutdown baseline. Tracked worker starts/stops/drops must match, including the seed worker. The separate monitoring actor is stopped and awaited explicitly; it is not included in those worker counters. These runs assess finite lifecycle/resource behavior, not production leak freedom, open-loop fairness, SLOs or comparable throughput. The first V3 resource attempt is excluded: pausing its Python pipe reader blocked diagnostic output. Its process/thread evidence is archived as a harness error; all ten cases are rerun in soaks-3 at nice 10 with the reader continuously active. An intermediate nested-nice 19 launch was cancelled before a completed case; both attempts remain harness diagnostics.

![V3-soaks-resources](graphs/V3-soaks-resources.png)

![V4-soaks-resources](graphs/V4-soaks-resources.png)

![V8-soaks-resources](graphs/V8-soaks-resources.png)

| Block / policy | Requested s | Cycles | Starts = stops = drops | Sampled RSS first / last / max MB | FD last | Threads last |
|---|---:|---:|---:|---:|---:|---:|
| V3-soaks / tokio-16 N4 | 300 | 86,457 | 11,067,521 | 5.0 / 10.6 / 10.6 | 11 | 6 |
| V3-soaks / native-local N3+R1 | 300 | 64,079 | 8,203,137 | 5.1 / 11.6 / 11.6 | 15 | 7 |
| V3-soaks / native-root N4+R1 Q8 | 300 | 71,112 | 9,103,361 | 5.5 / 11.8 / 11.8 | 15 | 8 |
| V3-soaks / sharded-16 N4 | 300 | 69,343 | 8,876,929 | 4.9 / 11.3 / 11.3 | 27 | 6 |
| V3-soaks / async N4 | 300 | 99,444 | 12,729,857 | 5.2 / 10.5 / 10.5 | 11 | 6 |
| V3-soaks / native-steal N3+R1 | 300 | 71,442 | 9,145,601 | 5.2 / 11.6 / 11.7 | 15 | 7 |
| V3-soaks / native-root N3+R1 Q8 | 300 | 67,945 | 8,697,985 | 9.2 / 11.6 / 11.6 | 15 | 7 |
| V3-soaks / tokio-pool N4 | 300 | 101,954 | 13,051,137 | 5.4 / 12.0 / 12.0 | 15 | 7 |
| V3-soaks / native-global N3+R1 | 300 | 65,498 | 8,384,769 | 5.4 / 11.6 / 11.6 | 15 | 7 |
| V3-soaks / native-steal N4+R1 | 300 | 76,170 | 9,750,785 | 5.5 / 11.9 / 11.9 | 15 | 8 |
| V4-soaks / native-runnable N4+R1 Q8 | 300 | 65,459 | 8,379,777 | 8.2 / 12.0 / 12.0 | 15 | 8 |
| V4-soaks / native-runnable N4+R1 Q32 | 300 | 67,734 | 8,670,977 | 9.6 / 11.9 / 11.9 | 15 | 8 |
| V8-soaks / native-runnable N4+R1 Q8 Z0 W0 | 300 | 73,199 | 9,370,497 | 6.1 / 11.9 / 11.9 | 15 | 8 |
| V8-soaks / async N4 | 300 | 97,594 | 12,493,057 | 8.3 / 10.8 / 10.8 | 11 | 6 |
| V8-soaks / native-deque N4+R1 Q8 Z0 W0 | 300 | 63,234 | 8,094,977 | 6.0 / 12.1 / 12.1 | 15 | 8 |
| V8-soaks / native-runnable N4+R1 Q8 Z1 W1 | 300 | 62,511 | 8,002,433 | 6.1 / 12.0 / 12.0 | 15 | 8 |
| V8-soaks / native-deque N4+R1 Q8 Z1 W1 | 300 | 47,609 | 6,094,977 | 5.2 / 12.1 / 12.1 | 15 | 8 |

## Qualitative CPU profiles

SIGPROF at 199 Hz, with libc/libgcc/pthread/vdso blocklisted because hardware perf is restricted. Leaf samples describe captured user-space worker stacks, not off-CPU wait, all process CPU or causal mutex cost. Inclusive category shares would overlap, so exact leaf counters are provided separately. Profiled processes are excluded from timed medians.

| Block / policy / placement | Captured executor samples | Other samples | Contended-mutex leaf samples | Flamegraph |
|---|---:|---:|---:|---|
| V2-profiles / native-steal N4+R1 / cross | 2,247 | 114 | 29 | [SVG](../data/profiles/V2-profiles/0000-profile-native-steal-w4-0.svg) |
| V2-profiles / native-global N4+R1 / same | 2,711 | 224 | 589 | [SVG](../data/profiles/V2-profiles/0001-profile-native-global-w4-0.svg) |
| V2-profiles / async N4 / same | 968 | 65 | 0 | [SVG](../data/profiles/V2-profiles/0002-profile-async-w4-0.svg) |
| V2-profiles / tokio-16 N4 / cross | 1,090 | 148 | 0 | [SVG](../data/profiles/V2-profiles/0003-profile-tokio-16-w4-0.svg) |
| V2-profiles / native-steal N3+R1 / cross | 1,815 | 114 | 22 | [SVG](../data/profiles/V2-profiles/0004-profile-native-steal-w3-0.svg) |
| V2-profiles / tokio-pool N4 / same | 1,172 | 160 | 0 | [SVG](../data/profiles/V2-profiles/0005-profile-tokio-pool-w4-0.svg) |
| V2-profiles / native-local N3+R1 / same | 2,191 | 222 | 441 | [SVG](../data/profiles/V2-profiles/0006-profile-native-local-w3-0.svg) |
| V2-profiles / tokio-pool N4 / cross | 904 | 78 | 0 | [SVG](../data/profiles/V2-profiles/0007-profile-tokio-pool-w4-0.svg) |
| V2-profiles / native-local N3+R1 / same | 2,351 | 244 | 544 | [SVG](../data/profiles/V2-profiles/0008-profile-native-local-w3-0.svg) |
| V2-profiles / native-local N4+R1 / same | 2,361 | 230 | 605 | [SVG](../data/profiles/V2-profiles/0009-profile-native-local-w4-0.svg) |
| V2-profiles / native-global N4+R1 / cross | 2,547 | 172 | 653 | [SVG](../data/profiles/V2-profiles/0010-profile-native-global-w4-0.svg) |
| V2-profiles / native-global N3+R1 / same | 1,533 | 128 | 330 | [SVG](../data/profiles/V2-profiles/0011-profile-native-global-w3-0.svg) |
| V2-profiles / native-steal N3+R1 / same | 2,039 | 171 | 28 | [SVG](../data/profiles/V2-profiles/0012-profile-native-steal-w3-0.svg) |
| V2-profiles / native-global N3+R1 / cross | 1,480 | 166 | 245 | [SVG](../data/profiles/V2-profiles/0013-profile-native-global-w3-0.svg) |
| V2-profiles / tokio-16 N4 / same | 960 | 78 | 0 | [SVG](../data/profiles/V2-profiles/0014-profile-tokio-16-w4-0.svg) |
| V2-profiles / sharded-16 N4 / same | 764 | 59 | 0 | [SVG](../data/profiles/V2-profiles/0015-profile-sharded-16-w4-0.svg) |
| V2-profiles / async N4 / cross | 1,017 | 98 | 0 | [SVG](../data/profiles/V2-profiles/0016-profile-async-w4-0.svg) |
| V2-profiles / native-local N4+R1 / cross | 2,455 | 242 | 717 | [SVG](../data/profiles/V2-profiles/0017-profile-native-local-w4-0.svg) |
| V2-profiles / tokio-16 N4 / cross | 1,015 | 93 | 0 | [SVG](../data/profiles/V2-profiles/0018-profile-tokio-16-w4-0.svg) |
| V2-profiles / native-local N3+R1 / cross | 2,083 | 182 | 490 | [SVG](../data/profiles/V2-profiles/0019-profile-native-local-w3-0.svg) |
| V2-profiles / sharded-16 N4 / cross | 1,119 | 90 | 0 | [SVG](../data/profiles/V2-profiles/0020-profile-sharded-16-w4-0.svg) |
| V2-profiles / native-global N4+R1 / cross | 1,796 | 168 | 392 | [SVG](../data/profiles/V2-profiles/0021-profile-native-global-w4-0.svg) |
| V2-profiles / native-steal N4+R1 / same | 2,200 | 109 | 41 | [SVG](../data/profiles/V2-profiles/0022-profile-native-steal-w4-0.svg) |
| V2-profiles / native-steal N3+R1 / cross | 2,132 | 146 | 34 | [SVG](../data/profiles/V2-profiles/0023-profile-native-steal-w3-0.svg) |
| V2-profiles / native-steal N3+R1 / same | 2,054 | 127 | 39 | [SVG](../data/profiles/V2-profiles/0024-profile-native-steal-w3-0.svg) |
| V2-profiles / native-local N4+R1 / cross | 2,298 | 199 | 606 | [SVG](../data/profiles/V2-profiles/0025-profile-native-local-w4-0.svg) |
| V2-profiles / native-steal N4+R1 / cross | 2,188 | 96 | 39 | [SVG](../data/profiles/V2-profiles/0026-profile-native-steal-w4-0.svg) |
| V2-profiles / native-local N4+R1 / same | 2,599 | 242 | 733 | [SVG](../data/profiles/V2-profiles/0027-profile-native-local-w4-0.svg) |
| V2-profiles / async N4 / cross | 1,048 | 95 | 0 | [SVG](../data/profiles/V2-profiles/0028-profile-async-w4-0.svg) |
| V2-profiles / sharded-16 N4 / same | 1,082 | 101 | 0 | [SVG](../data/profiles/V2-profiles/0029-profile-sharded-16-w4-0.svg) |
| V2-profiles / sharded-16 N4 / cross | 1,377 | 109 | 0 | [SVG](../data/profiles/V2-profiles/0030-profile-sharded-16-w4-0.svg) |
| V2-profiles / native-global N4+R1 / same | 1,804 | 154 | 416 | [SVG](../data/profiles/V2-profiles/0031-profile-native-global-w4-0.svg) |
| V2-profiles / native-steal N4+R1 / same | 2,113 | 98 | 34 | [SVG](../data/profiles/V2-profiles/0032-profile-native-steal-w4-0.svg) |
| V2-profiles / native-local N3+R1 / cross | 2,300 | 253 | 540 | [SVG](../data/profiles/V2-profiles/0033-profile-native-local-w3-0.svg) |
| V2-profiles / native-global N3+R1 / cross | 2,245 | 134 | 543 | [SVG](../data/profiles/V2-profiles/0034-profile-native-global-w3-0.svg) |
| V2-profiles / tokio-16 N4 / same | 968 | 93 | 0 | [SVG](../data/profiles/V2-profiles/0035-profile-tokio-16-w4-0.svg) |
| V2-profiles / tokio-pool N4 / cross | 954 | 88 | 0 | [SVG](../data/profiles/V2-profiles/0036-profile-tokio-pool-w4-0.svg) |
| V2-profiles / async N4 / same | 924 | 82 | 0 | [SVG](../data/profiles/V2-profiles/0037-profile-async-w4-0.svg) |
| V2-profiles / native-global N3+R1 / same | 2,253 | 145 | 502 | [SVG](../data/profiles/V2-profiles/0038-profile-native-global-w3-0.svg) |
| V2-profiles / tokio-pool N4 / same | 970 | 96 | 0 | [SVG](../data/profiles/V2-profiles/0039-profile-tokio-pool-w4-0.svg) |
| V3-profiles / native-root N4+R1 Q8 / same | 1,535 | 111 | 7 | [SVG](../data/profiles/V3-profiles/0000-profile-native-root-w4-0.svg) |
| V3-profiles / native-steal N4+R1 / cross | 2,007 | 110 | 41 | [SVG](../data/profiles/V3-profiles/0001-profile-native-steal-w4-0.svg) |
| V3-profiles / native-global N3+R1 / cross | 1,925 | 97 | 458 | [SVG](../data/profiles/V3-profiles/0002-profile-native-global-w3-0.svg) |
| V3-profiles / tokio-pool N4 / same | 709 | 74 | 0 | [SVG](../data/profiles/V3-profiles/0003-profile-tokio-pool-w4-0.svg) |
| V3-profiles / native-root N3+R1 Q8 / same | 1,400 | 89 | 5 | [SVG](../data/profiles/V3-profiles/0004-profile-native-root-w3-0.svg) |
| V3-profiles / native-global N4+R1 / cross | 2,336 | 98 | 665 | [SVG](../data/profiles/V3-profiles/0005-profile-native-global-w4-0.svg) |
| V3-profiles / native-global N3+R1 / same | 1,948 | 85 | 476 | [SVG](../data/profiles/V3-profiles/0006-profile-native-global-w3-0.svg) |
| V3-profiles / native-local N3+R1 / cross | 2,026 | 170 | 566 | [SVG](../data/profiles/V3-profiles/0007-profile-native-local-w3-0.svg) |
| V3-profiles / native-global N4+R1 / same | 2,342 | 104 | 637 | [SVG](../data/profiles/V3-profiles/0008-profile-native-global-w4-0.svg) |
| V3-profiles / native-steal N4+R1 / same | 1,982 | 92 | 30 | [SVG](../data/profiles/V3-profiles/0009-profile-native-steal-w4-0.svg) |
| V3-profiles / tokio-16 N4 / cross | 725 | 83 | 0 | [SVG](../data/profiles/V3-profiles/0010-profile-tokio-16-w4-0.svg) |
| V3-profiles / sharded-16 N4 / cross | 1,219 | 83 | 0 | [SVG](../data/profiles/V3-profiles/0011-profile-sharded-16-w4-0.svg) |
| V3-profiles / sharded-16 N4 / same | 792 | 77 | 0 | [SVG](../data/profiles/V3-profiles/0012-profile-sharded-16-w4-0.svg) |
| V3-profiles / async N4 / same | 757 | 71 | 0 | [SVG](../data/profiles/V3-profiles/0013-profile-async-w4-0.svg) |
| V3-profiles / native-local N4+R1 / same | 2,366 | 177 | 690 | [SVG](../data/profiles/V3-profiles/0014-profile-native-local-w4-0.svg) |
| V3-profiles / tokio-pool N4 / cross | 742 | 57 | 0 | [SVG](../data/profiles/V3-profiles/0015-profile-tokio-pool-w4-0.svg) |
| V3-profiles / async N4 / cross | 747 | 69 | 0 | [SVG](../data/profiles/V3-profiles/0016-profile-async-w4-0.svg) |
| V3-profiles / native-local N4+R1 / cross | 2,306 | 167 | 620 | [SVG](../data/profiles/V3-profiles/0017-profile-native-local-w4-0.svg) |
| V3-profiles / native-local N3+R1 / same | 1,945 | 164 | 555 | [SVG](../data/profiles/V3-profiles/0018-profile-native-local-w3-0.svg) |
| V3-profiles / tokio-16 N4 / same | 757 | 62 | 0 | [SVG](../data/profiles/V3-profiles/0019-profile-tokio-16-w4-0.svg) |
| V3-profiles / native-root N3+R1 Q8 / cross | 1,376 | 102 | 4 | [SVG](../data/profiles/V3-profiles/0020-profile-native-root-w3-0.svg) |
| V3-profiles / native-root N4+R1 Q8 / cross | 1,523 | 126 | 5 | [SVG](../data/profiles/V3-profiles/0021-profile-native-root-w4-0.svg) |
| V3-profiles / native-steal N3+R1 / cross | 1,753 | 108 | 11 | [SVG](../data/profiles/V3-profiles/0022-profile-native-steal-w3-0.svg) |
| V3-profiles / native-steal N3+R1 / same | 1,719 | 81 | 25 | [SVG](../data/profiles/V3-profiles/0023-profile-native-steal-w3-0.svg) |
| V4-profiles / native-runnable N4+R1 Q1 / same | 1,563 | 206 | 2 | [SVG](../data/profiles/V4-profiles/0000-profile-native-runnable-w4-0.svg) |
| V4-profiles / native-root N4+R1 Q8 / same | 1,548 | 190 | 5 | [SVG](../data/profiles/V4-profiles/0001-profile-native-root-w4-0.svg) |
| V4-profiles / native-runnable N4+R1 Q128 / cross | 1,527 | 207 | 2 | [SVG](../data/profiles/V4-profiles/0002-profile-native-runnable-w4-0.svg) |
| V4-profiles / native-root N4+R1 Q128 / same | 1,509 | 186 | 8 | [SVG](../data/profiles/V4-profiles/0003-profile-native-root-w4-0.svg) |
| V4-profiles / native-root N4+R1 Q128 / cross | 1,388 | 176 | 2 | [SVG](../data/profiles/V4-profiles/0004-profile-native-root-w4-0.svg) |
| V4-profiles / native-runnable N4+R1 Q32 / same | 1,716 | 256 | 13 | [SVG](../data/profiles/V4-profiles/0005-profile-native-runnable-w4-0.svg) |
| V4-profiles / async N4 / cross | 707 | 108 | 0 | [SVG](../data/profiles/V4-profiles/0006-profile-async-w4-0.svg) |
| V4-profiles / native-runnable N4+R1 Q1 / cross | 1,438 | 190 | 1 | [SVG](../data/profiles/V4-profiles/0007-profile-native-runnable-w4-0.svg) |
| V4-profiles / native-runnable N4+R1 Q128 / same | 1,484 | 260 | 4 | [SVG](../data/profiles/V4-profiles/0008-profile-native-runnable-w4-0.svg) |
| V4-profiles / native-root N4+R1 Q8 / cross | 1,509 | 208 | 3 | [SVG](../data/profiles/V4-profiles/0009-profile-native-root-w4-0.svg) |
| V4-profiles / native-steal N4+R1 / same | 1,957 | 185 | 33 | [SVG](../data/profiles/V4-profiles/0010-profile-native-steal-w4-0.svg) |
| V4-profiles / native-root N4+R1 Q1 / same | 1,593 | 166 | 5 | [SVG](../data/profiles/V4-profiles/0011-profile-native-root-w4-0.svg) |
| V4-profiles / async N4 / same | 743 | 86 | 0 | [SVG](../data/profiles/V4-profiles/0012-profile-async-w4-0.svg) |
| V4-profiles / native-steal N4+R1 / cross | 2,028 | 170 | 44 | [SVG](../data/profiles/V4-profiles/0013-profile-native-steal-w4-0.svg) |
| V4-profiles / native-runnable N4+R1 Q8 / cross | 1,679 | 273 | 12 | [SVG](../data/profiles/V4-profiles/0014-profile-native-runnable-w4-0.svg) |
| V4-profiles / native-root N4+R1 Q32 / same | 1,555 | 215 | 8 | [SVG](../data/profiles/V4-profiles/0015-profile-native-root-w4-0.svg) |
| V4-profiles / native-root N4+R1 Q32 / cross | 1,334 | 126 | 0 | [SVG](../data/profiles/V4-profiles/0016-profile-native-root-w4-0.svg) |
| V4-profiles / native-root N4+R1 Q1 / cross | 1,551 | 171 | 3 | [SVG](../data/profiles/V4-profiles/0017-profile-native-root-w4-0.svg) |
| V4-profiles / native-runnable N4+R1 Q8 / same | 1,474 | 208 | 5 | [SVG](../data/profiles/V4-profiles/0018-profile-native-runnable-w4-0.svg) |
| V4-profiles / native-runnable N4+R1 Q32 / cross | 1,419 | 195 | 3 | [SVG](../data/profiles/V4-profiles/0019-profile-native-runnable-w4-0.svg) |
| V5-profiles / native-deque N4+R1 Q32 / cross | 1,036 | 157 | 0 | [SVG](../data/profiles/V5-profiles/0000-profile-native-deque-w4-0.svg) |
| V5-profiles / native-deque N4+R1 Q1 / same | 1,470 | 185 | 0 | [SVG](../data/profiles/V5-profiles/0001-profile-native-deque-w4-0.svg) |
| V5-profiles / native-runnable N4+R1 Q8 / same | 1,362 | 202 | 5 | [SVG](../data/profiles/V5-profiles/0002-profile-native-runnable-w4-0.svg) |
| V5-profiles / native-runnable N4+R1 Q8 / cross | 1,162 | 122 | 0 | [SVG](../data/profiles/V5-profiles/0003-profile-native-runnable-w4-0.svg) |
| V5-profiles / native-root N4+R1 Q8 / cross | 1,369 | 192 | 1 | [SVG](../data/profiles/V5-profiles/0004-profile-native-root-w4-0.svg) |
| V5-profiles / native-deque N4+R1 Q8 / same | 1,368 | 196 | 0 | [SVG](../data/profiles/V5-profiles/0005-profile-native-deque-w4-0.svg) |
| V5-profiles / native-root N4+R1 Q8 / same | 1,312 | 140 | 1 | [SVG](../data/profiles/V5-profiles/0006-profile-native-root-w4-0.svg) |
| V5-profiles / native-deque N4+R1 Q32 / same | 1,104 | 211 | 0 | [SVG](../data/profiles/V5-profiles/0007-profile-native-deque-w4-0.svg) |
| V5-profiles / async N4 / cross | 700 | 127 | 0 | [SVG](../data/profiles/V5-profiles/0008-profile-async-w4-0.svg) |
| V5-profiles / native-deque N4+R1 Q1 / cross | 1,232 | 194 | 0 | [SVG](../data/profiles/V5-profiles/0009-profile-native-deque-w4-0.svg) |
| V5-profiles / native-steal N4+R1 / cross | 1,659 | 169 | 39 | [SVG](../data/profiles/V5-profiles/0010-profile-native-steal-w4-0.svg) |
| V5-profiles / native-deque N4+R1 Q8 / cross | 1,089 | 223 | 0 | [SVG](../data/profiles/V5-profiles/0011-profile-native-deque-w4-0.svg) |
| V5-profiles / native-deque N4+R1 Q128 / cross | 1,069 | 238 | 0 | [SVG](../data/profiles/V5-profiles/0012-profile-native-deque-w4-0.svg) |
| V5-profiles / native-deque N4+R1 Q128 / same | 1,092 | 226 | 0 | [SVG](../data/profiles/V5-profiles/0013-profile-native-deque-w4-0.svg) |
| V5-profiles / native-steal N4+R1 / same | 1,776 | 202 | 33 | [SVG](../data/profiles/V5-profiles/0014-profile-native-steal-w4-0.svg) |
| V5-profiles / async N4 / same | 734 | 122 | 0 | [SVG](../data/profiles/V5-profiles/0015-profile-async-w4-0.svg) |
| V6-profiles / native-deque N4+R1 Q8 Z0 W0 / cross | 1,050 | 247 | 0 | [SVG](../data/profiles/V6-profiles/0000-profile-native-deque-w4-0.svg) |
| V6-profiles / native-deque N4+R1 Q8 Z0 W0 / same | 1,191 | 244 | 0 | [SVG](../data/profiles/V6-profiles/0001-profile-native-deque-w4-0.svg) |
| V6-profiles / native-runnable N4+R1 Q8 Z0 W1 / cross | 1,109 | 187 | 6 | [SVG](../data/profiles/V6-profiles/0002-profile-native-runnable-w4-0.svg) |
| V6-profiles / native-deque N4+R1 Q8 Z1 W0 / cross | 1,053 | 233 | 0 | [SVG](../data/profiles/V6-profiles/0003-profile-native-deque-w4-0.svg) |
| V6-profiles / native-deque N4+R1 Q8 Z1 W1 / same | 884 | 241 | 0 | [SVG](../data/profiles/V6-profiles/0004-profile-native-deque-w4-0.svg) |
| V6-profiles / native-runnable N4+R1 Q8 Z0 W0 / cross | 1,308 | 209 | 6 | [SVG](../data/profiles/V6-profiles/0005-profile-native-runnable-w4-0.svg) |
| V6-profiles / native-runnable N4+R1 Q8 Z0 W1 / same | 1,119 | 208 | 11 | [SVG](../data/profiles/V6-profiles/0006-profile-native-runnable-w4-0.svg) |
| V6-profiles / native-runnable N4+R1 Q8 Z1 W0 / cross | 1,297 | 188 | 5 | [SVG](../data/profiles/V6-profiles/0007-profile-native-runnable-w4-0.svg) |
| V6-profiles / native-runnable N4+R1 Q8 Z1 W1 / cross | 1,082 | 184 | 5 | [SVG](../data/profiles/V6-profiles/0008-profile-native-runnable-w4-0.svg) |
| V6-profiles / native-runnable N4+R1 Q8 Z0 W0 / same | 1,371 | 200 | 5 | [SVG](../data/profiles/V6-profiles/0009-profile-native-runnable-w4-0.svg) |
| V6-profiles / native-deque N4+R1 Q8 Z1 W0 / same | 1,077 | 182 | 0 | [SVG](../data/profiles/V6-profiles/0010-profile-native-deque-w4-0.svg) |
| V6-profiles / native-deque N4+R1 Q8 Z0 W1 / same | 932 | 230 | 0 | [SVG](../data/profiles/V6-profiles/0011-profile-native-deque-w4-0.svg) |
| V6-profiles / native-runnable N4+R1 Q8 Z1 W0 / same | 1,332 | 236 | 4 | [SVG](../data/profiles/V6-profiles/0012-profile-native-runnable-w4-0.svg) |
| V6-profiles / native-runnable N4+R1 Q8 Z1 W1 / same | 1,102 | 183 | 4 | [SVG](../data/profiles/V6-profiles/0013-profile-native-runnable-w4-0.svg) |
| V6-profiles / native-deque N4+R1 Q8 Z1 W1 / cross | 1,022 | 202 | 0 | [SVG](../data/profiles/V6-profiles/0014-profile-native-deque-w4-0.svg) |
| V6-profiles / native-deque N4+R1 Q8 Z0 W1 / cross | 959 | 213 | 0 | [SVG](../data/profiles/V6-profiles/0015-profile-native-deque-w4-0.svg) |
| V7-profiles / native-runnable N4+R1 Q8 Z1 W1 I1 / cross | 1,087 | 194 | 11 | [SVG](../data/profiles/V7-profiles/0000-profile-native-runnable-w4-0.svg) |
| V7-profiles / native-deque N4+R1 Q8 Z0 W0 I1 / same | 1,112 | 222 | 0 | [SVG](../data/profiles/V7-profiles/0001-profile-native-deque-w4-0.svg) |
| V7-profiles / native-runnable N4+R1 Q8 Z0 W0 I1 / same | 1,326 | 221 | 5 | [SVG](../data/profiles/V7-profiles/0002-profile-native-runnable-w4-0.svg) |
| V7-profiles / native-runnable N4+R1 Q8 Z0 W0 I0 / cross | 1,331 | 228 | 5 | [SVG](../data/profiles/V7-profiles/0003-profile-native-runnable-w4-0.svg) |
| V7-profiles / native-runnable N4+R1 Q8 Z1 W1 I1 / same | 1,075 | 215 | 11 | [SVG](../data/profiles/V7-profiles/0004-profile-native-runnable-w4-0.svg) |
| V7-profiles / native-deque N4+R1 Q8 Z0 W0 I0 / cross | 1,106 | 234 | 0 | [SVG](../data/profiles/V7-profiles/0005-profile-native-deque-w4-0.svg) |
| V7-profiles / native-deque N4+R1 Q8 Z0 W0 I1 / cross | 1,146 | 216 | 0 | [SVG](../data/profiles/V7-profiles/0006-profile-native-deque-w4-0.svg) |
| V7-profiles / native-runnable N4+R1 Q8 Z1 W1 I0 / same | 1,066 | 196 | 2 | [SVG](../data/profiles/V7-profiles/0007-profile-native-runnable-w4-0.svg) |
| V7-profiles / native-deque N4+R1 Q8 Z1 W1 I1 / same | 908 | 199 | 0 | [SVG](../data/profiles/V7-profiles/0008-profile-native-deque-w4-0.svg) |
| V7-profiles / native-deque N4+R1 Q8 Z0 W0 I0 / same | 1,091 | 220 | 0 | [SVG](../data/profiles/V7-profiles/0009-profile-native-deque-w4-0.svg) |
| V7-profiles / native-deque N4+R1 Q8 Z1 W1 I0 / same | 936 | 199 | 0 | [SVG](../data/profiles/V7-profiles/0010-profile-native-deque-w4-0.svg) |
| V7-profiles / native-runnable N4+R1 Q8 Z0 W0 I1 / cross | 1,309 | 210 | 6 | [SVG](../data/profiles/V7-profiles/0011-profile-native-runnable-w4-0.svg) |
| V7-profiles / native-runnable N4+R1 Q8 Z0 W0 I0 / same | 1,412 | 266 | 9 | [SVG](../data/profiles/V7-profiles/0012-profile-native-runnable-w4-0.svg) |
| V7-profiles / native-runnable N4+R1 Q8 Z1 W1 I0 / cross | 1,062 | 165 | 5 | [SVG](../data/profiles/V7-profiles/0013-profile-native-runnable-w4-0.svg) |
| V7-profiles / native-deque N4+R1 Q8 Z1 W1 I1 / cross | 927 | 202 | 0 | [SVG](../data/profiles/V7-profiles/0014-profile-native-deque-w4-0.svg) |
| V7-profiles / native-deque N4+R1 Q8 Z1 W1 I0 / cross | 828 | 158 | 0 | [SVG](../data/profiles/V7-profiles/0015-profile-native-deque-w4-0.svg) |
| V8-profiles / native-root N4+R1 Q8 W1 / cross | 1,248 | 84 | 4 | [SVG](../data/profiles/V8-profiles/0000-profile-native-root-w4-0.svg) |
| V8-profiles / async N5 / cross | 800 | 117 | 0 | [SVG](../data/profiles/V8-profiles/0001-profile-async-w5-0.svg) |
| V8-profiles / async N4 / cross | 871 | 87 | 0 | [SVG](../data/profiles/V8-profiles/0002-profile-async-w4-0.svg) |
| V8-profiles / native-deque N4+R1 Q8 Z1 W1 / same | 1,123 | 152 | 0 | [SVG](../data/profiles/V8-profiles/0003-profile-native-deque-w4-0.svg) |
| V8-profiles / native-runnable N4+R1 Q8 Z1 W1 / cross | 1,202 | 144 | 11 | [SVG](../data/profiles/V8-profiles/0004-profile-native-runnable-w4-0.svg) |
| V8-profiles / async N4 / same | 847 | 77 | 0 | [SVG](../data/profiles/V8-profiles/0005-profile-async-w4-0.svg) |
| V8-profiles / native-deque N4+R1 Q8 Z1 W1 / cross | 1,153 | 155 | 0 | [SVG](../data/profiles/V8-profiles/0006-profile-native-deque-w4-0.svg) |
| V8-profiles / native-root N4+R1 Q8 W1 / same | 1,410 | 127 | 13 | [SVG](../data/profiles/V8-profiles/0007-profile-native-root-w4-0.svg) |
| V8-profiles / native-deque N4+R1 Q8 Z0 W0 / cross | 1,433 | 122 | 0 | [SVG](../data/profiles/V8-profiles/0008-profile-native-deque-w4-0.svg) |
| V8-profiles / async N5 / same | 854 | 95 | 0 | [SVG](../data/profiles/V8-profiles/0009-profile-async-w5-0.svg) |
| V8-profiles / native-deque N4+R1 Q8 Z0 W0 / same | 1,360 | 185 | 0 | [SVG](../data/profiles/V8-profiles/0010-profile-native-deque-w4-0.svg) |
| V8-profiles / native-runnable N4+R1 Q8 Z1 W1 / same | 1,270 | 179 | 9 | [SVG](../data/profiles/V8-profiles/0011-profile-native-runnable-w4-0.svg) |

## Limits and next decisions

Private prototypes are tools for locating costs. A faster synthetic graph does not establish better real applications or justify removing Tokio. Wake publication, cancellation, destructor ordering, helpers, timers/TCP, blocking behavior and graceful teardown are separate obligations. There is no CPU preemption for an ordinary Rust handler. Unbounded queues can still exhaust memory under overload.

Repeat on an otherwise idle host and workload-representative payloads/I/O before promoting any design. Add hardware/off-CPU evidence only where host permissions permit; do not infer cache or kernel causes from user-space profiles. Archive negative results and failed correctness gates alongside improvements.
