# Overnight native-executor study: 6 November 2026

The study follows the production actor contract: dynamic actors, ordinary Rust
messages, one addressed unbounded mailbox per actor, serial handlers, request/reply,
cooperative stop and lifecycle/monitor cleanup. The existing Tokio backend remains
the default. Private feature/environment selectors isolate prototype backends.

## Separate evidence blocks

V1 (07f0e11) completed 72 smokes and 360 graph processes before a stronger Loom
model proved a coalesced-wake publication defect. Its remaining stages were
cancelled. V2 (eefc302) corrected publication and completed 160 smokes, eight short
lifecycle smokes, 40 profiles, 200 queue graphs and 320 caller/counter controls.
Review then found a Global Condvar shutdown lost-notify window; further V2 stages
were cancelled. Preserve both as historical observations, not correctness claims.

V3 (097ae02) corrects the closed predicate under its Condvar mutex, serializes
concurrent teardown, and adds the amortized native-root driver. Eight executor unit
tests cover all five policies, selected full actor suites and all-target/all-feature
Clippy pass on fede before scoring. Four publication/queue Loom models pass on the
publication-corrected source; the Gate is unchanged in V3. Exact commands, logs and
source revisions must accompany final results. PR185's separate production stop fix
was merged after V3 froze: do not present V3 timings as current-main measurements.

Each block is immutable once scoring starts. A parent coordinator can be paused
between phases for validation, with the current phase completing uninterrupted.
No benchmark, build, plot or correctness test runs alongside our scored processes.
A source change requires a new source checkout, target directory and campaign.
Deadline for new launches is 14:30 UTC; the authorized overnight window ends
14:47:43 UTC. Existing host security, scheduler settings and other users' work
are left alone.

## Runtime resources

On fede (fede256), actor workers inherit CPUs2–5, producers CPUs6,7,14,15 and
collectors CPUs8,9,16,17. These are distinct physical cores without own SMT pairs.
Every four-core comparison uses the same masks; workers are not individually
pinned. Native N3+R1 is an equal-runtime-worker control for Tokio4. Native N4+R1
versus Tokio4 is an equal-actor-worker control; versus Tokio5 it controls total
runtime workers. Five runtime workers still share four physical cores. The
native idle runtime owner and lazy blocking helpers are additional threads.
Shared-host/frequency effects remain uncontrolled. Rust1.93 release binaries
include debug information for SIGPROF profiles; source, binaries, settings,
controller hashes, schedules and CPU masks are retained per block.

## Workloads

- Graphs:1,920 actors,256 logical producers,1,048,576 roots,eight handlers/root,
  100 synthetic CPU iterations/handler. Chain and fan-out, same/cross placement,
  five fresh process repetitions. Only static shards enforce placement. Native
  callers use256 OS threads; async callers use four current-thread runtimes
  driving256 logical producers. Same producer cores, different thread populations.
- Caller/counter factorial:observation shared atomic on/off crossed with both
  caller modes. Counter-off retains clocks, latency storage, boxing and synthetic
  work; it is not instrumentation-free.
- Clock controls:explicit turn16 with count-only(slice0) or25/50/200µs clock
  bounds, async caller/counter-on. Empty mailboxes may not reach the turn budget.
  Neither a turn nor a scheduler can preempt an ordinary running Rust handler.
- Offered load:240 actors,four hot,80/20 traffic,hot work5000/cold100 iterations,
  common absolute rates500k/900k/1.2M per second, two-second injection windows,
  steady or1000-request bursts, three repetitions. Deadline2s; all admitted work
  drains. The generator is invalid if >1% admissions are >1ms late. Retain and
  label invalid cells, with no equal-load ranking. Survivor completion quantiles
  omit timeouts; observed deadline misses and timeouts must accompany tails.
- Lifecycle soaks:1024 idle residents,128 actors/churn cycle,16 sequential
  stateful requests/actor, helpers/timers, periodic expected handler panic,
  exact monitor IDs/reasons and exact starts/stops/drops/requests/Down counts.
  Five minutes per configuration; bounded per-cycle histories, streamed1Hz RSS,
  descriptors, threads and progress. Initial warm-up remains visible; resource
  high-water behavior is not leak proof or a throughput/SLO benchmark.

Graph profile rows are independent of timing medians. SIGPROF199Hz, with
libc/libgcc/pthread/vdso blocklisted, measures captured user-space stacks. Exact
leaf counts do not establish off-CPU wait, cache misses or causal CPU shares.
Per-thread scheduler counters sum recorded runtime threads, including I/O/owner
and helpers; they cover start padding and drain. Timed elapsed excludes padding.
Whole-process CPU/RSS include drivers and sample storage, not isolated actor cost.

## Analysis and reproduction

`collect_fede_overnight.py` accepts repeated `--run name=directory`, requires each
block to have finished cleanly, preserves original CSV/metadata, archives logs,
copies series/profiles/counters and adds source bundles plus a SHA256 manifest.
The combined view adds dataset/source/original-stem columns and prefixes artifact
stems; numeric values and config/experiment JSON stay unchanged.

`render_fede_overnight.py` groups by dataset,phase,policy,workers,all workload
settings(except output paths),and all experiment settings. No source/caller/
counter/slice/worker-budget pooling. Medians and min/max are descriptive, not
confidence intervals. Invalid offered schedules remain marked in group plots and
individual time-series legends with admission lag; set axis bounds after all
curves so later peaks cannot be clipped. Resource summaries retain post-warm-up
ranges. Standalone HTML embeds PNG figures; SVG and raw CSV remain available.

See [native queue/caller/clock protocol](NATIVE-CONTROLS-PROTOCOL.md),
[amortized driver](AMORTIZED-DRIVER-PROTOCOL.md) and
[initial profiling/graphs](RESEARCH-FEDE-PROTOCOL.md) for component details.

## Cache-domain disclosure

Sysfs cache topology confirms CPUs2–7 share L3domain0(98304K), while CPUs8–9
share domain1(32768K) withcores8–15. Thus four-core actors2–5 stay in one domain,
while eight-core actors2–9 span both. Physicalcores0/1 are sampled through
logical CPUs16/17 without pairing their siblings0/1. Archive per-CPU cache IDs,
sizes and shared/SMT lists as cpu-cache-topology.json. Within-budget comparisons
share the same mask; cross-budget differences are not homogeneous-core strong
scaling, and stack sampling cannot attribute cache/frequency causes.

## Resource-duration allocation before observations

Before any long-soak resource outcome was observed, the fixed overnight budget
was allocated to consistent300-second lifecycle windows for all14V3/V4/V5
resource configurations, leaving room for the paired scheduling controls and
report/review work. The frozen CLI default remains600seconds; orchestration
passes --seconds300 explicitly and exact arguments remain in each metadata
file. Five-minute windows do not establish production leak freedom, and no
result is described as a completed ten-minute run. Source-matched benchmark
graph/profile/overload plans and validity thresholds remain unchanged.

## Frozen-source scheduling follow-up

Before observing any new optimized-scheduler mixed-load/resource outcomes, an
external controller fixes a V7 (`acd37a3`) boxed-future (`I0`) follow-up. It uses
Tokio4, Root W0/W1 and the full Z/W 00/01/10/11 factorial for both Runnable and
Deque. The offered matrix uses common 500k and 1.2M requests/s, spread/concentrated
hot actors, steady/1,000-request bursts and two fresh repetitions: 176 cases.
Forty-four small offered smokes precede it. Existing validity rules, deadlines,
CPU masks and full admitted-work drain are unchanged.

Five additional resource cases pair Tokio4 with Runnable and Deque Z0W0/Z1W1.
Each requests 300 seconds, fixed before resource outcomes; five two-second
resource smokes come first. These are within-source comparisons of combined
settings, not isolated atomic-cost measurements or production memory/SLO claims.
I1 is not carried forward: graph medians did not show enough throughput benefit
to justify more storage-complexity experiments in the remaining budget.

The controller imports the immutable source's original Campaign/process helpers,
records its own SHA256 and asserts the measured source revision/clean worktree.
It runs only after the existing V3/V4/V5 queue completes, never alongside another
scored process. Each block records its planned count and deadline status. Partial
or failed blocks remain diagnostic evidence and are not called complete results.
The hard new-launch deadline remains 14:30 UTC.

## Review-driven fairness correction and harness exclusion

PR187 review found that the V5/V6/V7 Deque transport always favored injection on
periodic oldest turns. Sustained outside arrivals plus hot local continuations
could hide the oldest local ticket indefinitely at one worker. Those Deque timing
observations remain historical; this does not affect Root or Runnable. Unstarted
V5 scale/offered/resource and V7 optimized follow-up blocks were cancelled before
any case, with cancellation states archived.

V8 (final source1dcfea1; initial corrected preflight e633e59) alternates oldest-local/injection priority, falls back to owner pop
after bounded self-steal Retry, validates TLS worker identity and corrects shared
buffer lifetime comments. A strengthened regression fails on old95b4ca6 with the
starvation assertion and passes on V8. The preliminary probe omitted hot local
continuations and passed on the old source; retain it as a test-design limitation.
Reverse progress, cross-pool injection and four-slot peer batch conservation also
pass. These are targeted regressions, not a hard wall-time fairness proof.

The corrected frozen boxed-only source replaces the cancelled follow-up. Its
six graph configurations are TokioN/N+1, RootW1, RunnableZ1W1 and DequeZ0W0/Z1W1,
all with root quantum8. Four-core graphs use120 processes (five repetitions),
eight-core scale uses72 (three repetitions);12 profiles remain separate. First
run24 four-core and24 eight-core graph smokes,44 offered smokes and5 two-second
resource smokes. Then the same full176-case Z/W offered factorial and five300s
resource windows defined above execute only after V4 completes. No inline-future
selector is present. Start before13:30 UTC, stop launches early enough for the
14:20 controller cutoff; exact source/controller/binary hashes accompany blocks.

The first V3 resource attempt is excluded from results. Our SIGSTOP of its Python
coordinator stopped draining a diagnostic pipe and blocked an actor-worker in
pipe_write. Process/thread snapshots and the interrupted series are archived as
a harness error, not a library failure. All ten cases rerun in soaks-3 with the
reader continuously active. No scored child should run while its pipe-draining
coordinator is paused; only a parent shell may wait at a completed phase boundary.


An intermediate resource restart inherited a second nice adjustment, yielding
nice19 instead of the planned nice10. It was cancelled before any case completed,
with metadata retained as launch-diagnostic evidence. The valid ten-case rerun
uses soaks-3, nice10, the original source/binary and uninterrupted pipe drainage.
Neither interrupted resource attempt is counted as a library failure or completed
resource block.

The unstarted480-case V4 scale/quantum sweep is deferred before any scores to
reserve time for the review-discovered deque fairness correction. V4's offered252
and two resource cases remain unchanged. Existing V7 eight-core evidence and
V8's new source-matched72-case scale block remain separate. Cancellation/defer
decisions and exact timestamps are archived; no V4 scale outcome selected this
change.
