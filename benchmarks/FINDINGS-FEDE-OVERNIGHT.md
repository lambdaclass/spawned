## Findings from the completed controlled graph studies

**Keep Tokio as the default.** The experiments found useful costs to remove in
custom scheduling, while the completed examples below still favor the existing
Tokio reference. These are workload-specific comparisons, not a universal backend
ranking. Overload/resource results are kept in their source-matched blocks below.

### A small wakeup change mattered more than replacing task state

In V6 (`0f81b57`), the same-source chain/cross case used 1,920 actors, 256 logical
producers on four caller runtimes, 1,048,576 roots, eight handlers/root and five fresh
process repetitions. All share four physical actor cores. Native policies add one
shared I/O worker and an idle owner; Tokio5 is an oversubscription control, not the
primary reference.

| Policy / settings | Median roots/s | Median observed p99 ms |
|---|---:|---:|
| Tokio4 | 943,042 | 0.472 |
| Custom Root, baseline | 517,754 | 1.399 |
| Custom Root, wake precheck | 596,682 | 1.185 |
| Runnable, baseline | 529,822 | 2.207 |
| Runnable, metadata callback + wake precheck | 623,903 | 1.569 |
| Deque, baseline | 575,154 | 1.838 |
| Deque, metadata callback + wake precheck | 672,043 | 1.585 |

This Deque source was later found to allow local-oldest starvation under
sustained injection plus hot local continuations. These graph observations remain
historical evidence, not evidence of readiness; V8 tests a corrected policy.

The combined Deque settings improve the displayed median rate by 16.8%, while
remaining below Tokio4. Wake precheck is the larger observed change in this case.
It skips a shared rotating-target RMW when no peer is observed asleep. This is a
wake hint with register/recheck and a requested timer fallback, not a hard latency
bound. Smaller metadata-callback gains are descriptive median ratios; significance
or additivity is not established by these tables.

### Generic inline futures did not close this throughput gap

V7 (`acd37a3`) compares future representation within one source. Allocation/layout
and monomorphization change together. All other settings in each pair match. In
its chain/cross case, at joint Z1W1 scheduling settings:

| Policy / future storage | Four-core roots/s, n=5 | Eight-core roots/s, n=3 |
|---|---:|---:|
| Tokio reference, 4 / 8 workers | 922,031 | 1,408,876 |
| Runnable, boxed/dynamic | 622,911 | 689,489 |
| Runnable, generic inline | 629,457 | 679,968 |
| Deque, boxed/dynamic | 657,656 | 756,131 |
| Deque, generic inline | 659,978 | 754,472 |

The eight-core block uses 16,128 actors, 1,024 logical producers and 2,097,152 roots;
it is a separate workload/resource budget. Its actor mask spans 96 MiB and 32 MiB L3
domains, so this is not homogeneous-core strong scaling. Deque rows inherit the historical fairness limitation; Runnable is unaffected.
The observed inline
rate differences are small and do not justify its pin/drop complexity on throughput
alone. Inline task storage can also remain allocated while stale Wakers live;
ordinary envelope/reply allocations remain in either case. Do not infer isolated
actor memory use from whole-process RSS.

### Clock and count limits are different policies

V3 (`097ae02`) completed a separate five-repetition clock study. For chain/cross,
Tokio16 count-only yielded 944,187 roots/s and 0.465 ms observed p99; its 50 µs turn
control yielded 863,051/s and 0.643 ms. Ordinary Tokio yielded 966,168/s and 0.467 ms.
The toggle changes clock reads and time-based yielding. Yield counts were not
recorded, so the difference cannot be attributed specifically to frequent slice
expiration. Count-only removes the time limit between handlers; neither policy
preempts a running Rust handler. Mixed-load fairness still matters.

### Fast cold replies can coexist with a growing hot queue

V3's concentrated, steady 500k/s sharded16 case had three valid schedules.
The median cold other-cohort observed p99 was 0.011 ms, while median peak hot
queue depth was 410,074 and in-window queue growth was about 202k messages/s.
This is a finite two-second experiment with all work subsequently drained, not
sustainable capacity. The fixed placement isolates cold actors from a hot shard;
quick cold replies alone do not establish that the system is keeping up.

Across V3's full offered matrix, 316 of 432 admission schedules met the unchanged
validity threshold; 116 remain labelled diagnostics. No equivalent-offered-load
ranking is made from invalid schedules or mixed-validity group medians.

### Correctness work produced a production fix

Deterministic tests reproduced an idle-stop race in both actor modes. The fix is
merged in [PR185](https://github.com/lambdaclass/spawned/pull/185); current handlers
still finish, and cooperative cancellation remains non-atomic with dispatch.
Prototype wake-publication, idle-shutdown and deque fairness defects were also found and fixed
before further scoring on new source versions. Historical observations remain
archived; passing the current tests is not proof that every schedule is correct.

Full per-cell min/max ranges, all graph cases, source/binary/controller hashes,
invalid offered schedules and bounded resource outcomes remain in the appendices
and machine-readable artifacts. Keep slower designs as research evidence rather
than treating every prototype as a supported production backend.
