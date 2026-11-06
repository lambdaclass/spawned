# Archived roadmap acceptance criteria

This document preserves the earlier acceptance criteria, not the current
implementation order. See the [current roadmap](../ROADMAP.md) for public runtime
placement, Tokio-free threads and supervision in that order.

Updated 2026-11-06. This is a proposed implementation order, not a release-date
commitment. Each milestone needs its own design, tests, and review.

## Principles to preserve

- Application handlers express application logic; the framework owns messaging,
  dispatch, and lifecycle machinery.
- Typed protocols and ordinary Rust message/result types remain the interface.
- Dynamic actor creation, addressed delivery, and both async and synchronous
  thread execution remain supported.
- Overload, cancellation, shutdown, and restart policies must be explicit.
  Enqueue success is not a guarantee of processing or durable delivery.
- Instrumentation must not require changes to business handlers or alter their
  ordering, reply, or failure semantics.

## Foundation already available

On main after [PR166](https://github.com/lambdaclass/spawned/pull/166) and
[PR168](https://github.com/lambdaclass/spawned/pull/168) (available in v0.6.0):

- `Actor` lifecycle: `started()`, per-message `Handler<M>`, and `stopped()`.
- Protocol macros, typed replies, type-erased protocol references and recipients.
- Tokio tasks, blocking-pool and dedicated-thread task backends, plus synchronous
  thread actors that do not need a running Tokio executor.
- Registry, timers, stream/iterator listeners, exit reasons and monitors.
- Bidirectional links, registration before child dispatch with `start_linked`,
  `trap_exit`, and the `#[exit_received]` lifecycle callback in both modes.
- Explicit wakeup for stopping synchronous thread actors without timeout polling;
  cancellation still waits for a running handler to finish.
- Generated lifecycle scenarios, cleanup regressions, concurrent producers,
  macro compile tests, cancellation Loom models, and nightly churn tests.

The thread mode still uses Tokio synchronization primitives through `spawned-rt`;
not needing an executor is different from having no Tokio dependency. Full
supervision and promotion of experimental schedulers remain future work. The
initial metrics feature is described below.

## 1. Optional framework-level observability

The initial `metrics` feature in v0.6.0 implements fixed aggregate counters,
mailbox-depth access, sampled timing and pull-based Prometheus text output;
see [the metrics guide](../METRICS.md). Oldest-message age, active-monitor counts,
causal context and additional exporters remain future work. An application
benchmark must still establish useful labels, collection cost and sampling.

Instrument the mailbox, dispatch, request, and lifecycle boundaries first.
Start with an exporter-independent collection interface and one optional exporter;
do not begin with a shared-memory protocol or a dashboard.

| Signal | Definition / question |
|---|---|
| Mailbox depth | Accepted messages waiting to begin handling; exclude the active handler. |
| Oldest-message age | Time since the oldest still-queued message was enqueued; distinguish an exact measurement from an estimate. |
| Queue wait | Enqueue to handler start; includes scheduling delay. |
| Handler elapsed time | Handler start to completion, including awaited time; not equivalent to CPU time. |
| Request latency and outcomes | Caller-observed completion, timeout, or actor-stop result. |
| Actor exits and active monitors | Lifecycle outcomes and retained monitoring resources. |

Define behavior for cancelled callers, dropped queued messages, panic paths, and
shutdown before choosing an implementation. Use monotonic time within a process;
propagated ingress timestamps require an explicit origin/correlation mechanism.
Do not claim end-to-end latency from enqueue timestamps alone.

**Acceptance:** no handler edits; an instrumentation-off path; bounded metric
cardinality and exporter buffering; tests for accounting across failure paths;
measured disabled/enabled/sampled overhead. Exporter slowness must not block actor
handlers. Specify whether per-actor detail is opt-in to avoid unlimited labels.

## 2. Representative benchmarks and one client workload

Measure the existing implementation before selecting a new transport or runtime.

- Many small actors, a few busy actors, and mixed I/O/CPU workloads.
- Send-only traffic and typed requests; different message sizes and fan-out.
- Bursts and sustained overload, not just steady-state ping-pong.
- Compare all relevant existing backends with equal CPU budgets and a plain
  Tokio baseline where the application semantics can be matched.
- Record throughput, p50/p95/p99 request and queue latency, allocations, RSS,
  CPU utilization, and instrumentation overhead. Include offered load and errors.
- Pin revisions, compiler/build flags, hardware, core placement, load-generation
  method, and warmup. Use an external arrival schedule for overload experiments
  so a slow system does not hide its own tail latency by slowing the generator.

**First deliverable:** one instrumented Ethrex or Ethlambda workload showing
where messages wait, where processing time goes, and what happens under overload.
Use a reproducible fixture or approved sanitized trace; no client integration or
performance result is implied by this roadmap. Define a target improvement and
acceptable regressions before implementing an optimization.

## 3. Choose improvements from the measurements

| Evidence | Candidate experiment | Required guardrail |
|---|---|---|
| Sustained queue growth | Bounded mailboxes and admission control | Explicit wait/reject behavior, shutdown wakeup, and cyclic-send deadlock tests; never silently overwrite requests. |
| Dispatch/allocation dominates | Batching, allocation reduction, envelope reuse | Preserve message ownership, drop behavior, ordering, fairness, and panic cleanup. |
| Scheduling/locality dominates | Optional affinity for dedicated actors | Same CPU budget, oversubscription tests, platform fallback, and power/idle-CPU measurements. |
| Diagnostics dominate | Sampling or cheaper aggregate collection | Preserve interpretable measurements; quantify missing samples and export loss. |

These are alternatives to evaluate, not an instruction to implement all of them.
Keep unbounded mailboxes available where applications deliberately choose them.
A message-count bound alone does not bound payload memory or blocked senders.

## 4. Supervision as the next major recovery capability

Build on exit reasons, monitors and links while keeping recovery policy outside ordinary
application handlers:

- Explicit restart policies, limits within a time window, backoff, and escalation.
- Child construction/state recovery and ordered startup/shutdown.
- Defined identity and reference behavior across actor instances.
- Defined outcomes for queued messages and in-flight requests on failure.
- Equivalent policy semantics across async and synchronous thread modes, with
  honest limits on interrupting blocking Rust code.

**Acceptance:** a written lifecycle contract, failure-injection scenarios,
restart-storm tests, and examples that do not imply exactly-once effects or
transparent state recovery. A client recovery need can justify advancing this
milestone; optimization is not a prerequisite for supervision.

## Backend graduation policy

Keep the existing supported execution modes and Tokio default. Retain alternative
queue/scheduler/runtime-ownership designs behind the private experiment feature
until a defined application need justifies support. Archive designs whose benefit
does not justify their complexity, with source/data available for reproduction.

A proposed supported backend needs a named workload and target metric, repeated
controlled measurements, an explicit budget for latency/CPU/memory/fairness
tradeoffs, lifecycle/I/O/helper/failure coverage, and a reviewed maintenance/API
contract. A win in one synthetic case is evidence to investigate. Promotion is a
separate decision; adding more backend names is not a milestone.

## Research, not committed replacements

The [Silver/flux research](../research/silver-flux.md) evaluates timing propagation,
external diagnostics, preallocated transport, affinity, and thread execution.
It separates three possible future projects:

1. Make thread-only builds independent of Tokio through feature/dependency
   separation and runtime-neutral synchronization interfaces.
2. Prototype a specialized low-latency thread backend with explicit CPU budgets
   and waiting policies, while preserving the actor/protocol contract.
3. Replace the async runtime only if measured requirements justify owning an
   executor, I/O reactor, timers, wakeups, cancellation, and ecosystem adapters.

There is no current commitment to remove Tokio. Busy spinning, shared-memory
messaging, Copy-only messages, and lossy overwrite queues are not new defaults.
Preemptive scheduling, deterministic simulation/replay, virtual/distributed
actors, and a project website remain separate research or product work.

Deterministic testing is distinct from replacing the production runtime:
current generated seeds reproduce operations, not thread schedules. Continue
improving [tests](../TESTING.md) throughout every milestone.

## Backend research checkpoint

The [overnight scheduler research](../../research/backend-experiments/2026-11-06-fede/SCHEDULER-RESEARCH.md)
keeps Tokio as the default and treats custom drivers as private experiments.
Independent requests, actor-to-actor graphs, hot/cold load and lifecycle churn
answer different questions; a fast result in one does not select a runtime for
all applications. Larger actor populations need realistic traffic, not only
more idle actors.

Before promoting a backend, require lifecycle/helper/I/O parity, wake-publication
models, progress tests with both outside arrivals and internal continuations,
overload admission/deadline evidence and bounded resource runs. Benchmark success
alone missed real prototype publication and fairness defects. Targeted regression
work also produced the production idle-stop fix in
[PR185](https://github.com/lambdaclass/spawned/pull/185).

Use the research to choose one client workload and optional framework metrics
before expanding the backend count. Task scheduling, runtime ownership and an
io_uring/epoll/kqueue I/O engine remain separate design decisions. Preserve
ordinary message ownership, unbounded-mailbox availability, addressed delivery,
typed replies and cooperative handler behavior throughout.

## Recovery building blocks and remaining work

Exit reasons ([PR163](https://github.com/lambdaclass/spawned/pull/163)), typed
child handles/identities ([PR164](https://github.com/lambdaclass/spawned/pull/164)),
monitors ([PR165](https://github.com/lambdaclass/spawned/pull/165)), and links
([PR166](https://github.com/lambdaclass/spawned/pull/166)) are implemented. See the
[usage guide](../API-GUIDE.md#links-and-trapped-exits) and
[lifecycle review](../reviews/pr-166-links.md) for current behavior. Normal linked
exits are ignored unless trapped; abnormal exits cancel non-trapping peers;
`Kill` cannot be trapped but remains cooperative. The current exit-reason variants
are `Normal`, `Shutdown`, `Panic(String)` and `Kill`. `Shutdown` is reserved for
future ordered supervision shutdown; it is not currently produced by actor stops.

Child specifications, restart strategies (`OneForOne`, `OneForAll`, `RestForOne`),
restart limits/backoff, escalation and dynamic supervision still need design and
implementation. Recovery must define replacement identity, stale references,
state restoration and pending requests explicitly. These are not implied by
having links. No release date or v1.0 commitment is made here.

## Historical context

The [previous plan](roadmap-before-observability.md) is archived because it
predates the current APIs and contains outdated status and competitor claims.

Other contract-focused candidates are ranked in the [concurrency ideas research](../research/concurrency-ideas.md).

The [Commonware and capability review](../research/commonware.md) assesses actor systems/execution groups, supervision, admission control and typed streams as research capabilities. The [current roadmap](../ROADMAP.md) defines implementation order; these do not change the current release contract.
