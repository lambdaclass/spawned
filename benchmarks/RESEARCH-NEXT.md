# Decision-driven follow-up experiments

Keep Tokio as the default after the full-actor comparison.
The next experiment should resolve a specific engineering decision.

| Priority | Performance experiment | Decision it informs | Evidence required |
|---|---|---|---|
| 1 | CPU/wakeup/reactor profiling of the full-actor macOS loss and Linux gain | Improve the default executor path or invest in sharding? | Quiet target runs, actual profiles of active1024/hotspot/TCP; distinguish allocations, scheduling and reactor contention before attributing causes. |
| 2 | Actor-to-actor chains/fan-out with controlled placement and hot-shard concentration | Does locality justify sharding, and where do cross-shard hops erase it? | Identical logical graph and traffic; same/cross-shard routes, fixed worker budget, cold-actor tails and dynamic churn. |
| 3 | Fixed offered-rate load sweep with intended-arrival accounting and explicit producer lag | Does a batching policy improve application tails below/near/above saturation? | Arrival-to-completion and caller-observed tails, queue/RSS time series, validated submissions/timeouts and driver schedule checks. |

**Separate resource/API track:** compare blocking-worker leasing per active handler
with persistent `Backend::Blocking` and dedicated threads. This targets the known
finite-pool idle-actor limit; it does not explain the scheduler throughput results.
Test 1,024+ mostly idle actors, bursts above pool capacity, bounded active handlers,
panic/stop/monitor contracts, helper I/O and shutdown. Do not silently change the
scheduling contract. This separation follows agy's review of the completed results.

Per-handler blocking dispatch is a proposed experiment, not an implemented change.
It may move actor state between pool threads and changes which work owns runtime
leases. Existing public APIs must stay usable; migration, TLS assumptions, panic
cleanup, async suspension and stop semantics need explicit review.

The current blocking benchmark requests a 100 µs OS sleep, but actual timer
behavior is measured in handler_p99_ns and varies sharply by host/VM. It models
blocking rather than calibrated CPU work; do not treat its requested interval as
observed service time or compare OS speeds from it.

Closed-loop RPC throttles itself while responses are delayed. Use intended arrival
times in the offered-rate follow-up and account for missing/late submissions.
[wrk2 methodology](https://github.com/giltene/wrk2) and
[HDR Histogram](https://github.com/HdrHistogram/HdrHistogram) explain the
coordinated-omission problem.

Tokio documents a large, bounded blocking pool, warns that already-running
blocking jobs cannot be aborted, and describes cooperative scheduling. These
contracts make idle-worker retention and shutdown better immediate questions than
a kernel substitution. [spawn_blocking](https://docs.rs/tokio/1.48.0/tokio/task/fn.spawn_blocking.html),
[bridging synchronous code](https://tokio.rs/tokio/topics/bridging).

The Seastar locality argument motivates graph/placement tests, while task quotas
motivate fairness and I/O-poll-delay tests. An explicit yield is a scheduler hint,
not a preemptive guarantee. [Shared nothing](https://seastar.io/shared-nothing/),
[task quotas](https://docs.seastar.io/master/tutorial.html#preemption-and-task-quota),
[Tokio yielding](https://docs.rs/tokio/latest/tokio/task/fn.yield_now.html).

Only add work stealing, affinity, spinning or another I/O backend when one of
these tests supplies a concrete reason. Independent agy result reviews challenged the speed claims and this ordering;
profiling, topology and offered-rate tests now lead the performance track.

## A concrete acceptance protocol

Balanced closed-loop RPC often leaves individual actor mailboxes empty. Include
finite bursts and record whether explicit turn limits are actually reached before
crediting a throughput difference to batching.

For offered-rate tests, choose one absolute rate grid from the baseline's measured
capacity and apply it to every candidate (for example 25%, 50%, 75%, 90%, 105%).
Do not normalize each backend to a different arrival rate. Measure from intended
arrival time, retain timeouts/late work, and publish producer enqueue lag beside
completion and observation latency. Test both steady arrivals and bounded bursts.
Include queue/outstanding-work and RSS time series; a single process peak cannot
establish stability. Bound the run and driver sample storage so driver allocations
do not masquerade as mailbox growth. Investigate missed schedules before ranking
scheduler p99s. Longer runs on a quiet target are needed before changing defaults.

For the actor-graph test, hold actor count, logical graph, payload and request count
constant. Use acyclic chains and fan-out/fan-in, then change only edge placement
(same/cross shard), skew and worker budget. Track how many actor handlers and
network operations one logical request represents. Include cold actors sharing a
hot shard. A layout-dependent gain does not justify a general-purpose replacement.

For blocking-worker leasing, keep sequencing and lifecycle assertions identical
and cap the executor at two workers. Start more actors than capacity while most
are idle, then drive bursts with known concurrency. Verify that all actors can
start without reserving idle workers, that active handlers never exceed the
configured budget, and that queued cancellation, started/stopped panics, monitors,
async child work and shutdown still have defined results. Compare overhead for
short work and usefulness for genuinely blocking work separately. Never infer a
production win from the existing sleep-only misuse test; Tokio's documented
`spawn_blocking` lifetime restrictions remain relevant.
