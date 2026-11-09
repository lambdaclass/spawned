# Spawned roadmap

Updated 2026-11-08. This is the implementation order, not a release schedule.

Business handlers stay ordinary Rust. The framework owns dispatch and lifecycle.
Keep typed messages/replies, dynamic actors, addressed delivery, cooperative
handlers and the existing default behavior. New policies are explicit and opt-in.

## Available in v0.6.0

Typed protocols, async/blocking/thread execution, actor identities, monitors,
links and trapped exits, linked startup, shutdown fixes, optional framework
metrics, and reproducible backend research. v0.6.0 does not include automatic
supervision. Thread actors need no active Tokio executor but still compile Tokio.

## 1. Execution groups plus supervision

The parked async + OneForOne MVP is implemented on main and unreleased.
See the [usage guide](EXECUTION-GROUPS.md) and the
[MVP design and acceptance criteria](design/execution-groups-supervision.md).

### 1a. ActorSystem and execution groups — implemented

- Named parked async pools with explicit worker budgets and `start_on(&group)`.
  Ordinary `.start()` remains ambient and unmanaged.
- Ownership of admitted actors, registered async/blocking helpers and runtime
  teardown. Managed timers/listeners/monitors use the assigned group.
- Readiness, closed admission, reverse shutdown and incomplete reports that
  retain actual cleanup without overlapping replacement generations.

Still to add: dedicated/pinned groups, sized blocking-actor pools and additional
platform/resource contracts. Spinning remains experimental. Worker budgets count
async workers, not all OS threads or reserved CPU cores.

### 1b. Automatic supervision — implemented OneForOne

- Fresh-instance ChildSpec factories with fallible startup, readiness and retained
  group placement; Permanent, Transient and Temporary restart modes.
- Shared rolling retry limits, per-child capped backoff/jitter, ordered startup
  and reverse shutdown. Lifecycle observation bypasses the user-message backlog.
- Instance-bound references and explicit replacement lookup. Queued work is
  discarded; no replay, automatic durable state recovery or exactly-once effects.
- Runnable examples and failure tests for startup/cleanup deadlines, constructor
  cancellation, restart storms and concurrent shutdown. Blocking code remains
  cooperative; helper admission closes at shutdown start, with no drain policy.

Still to add: nested supervisor child specs, OneForAll and RestForOne after the
basic recovery lifecycle is proven in applications.

Full Ethrex/Ethlambda replay or a same-node production canary remains a validation
gate. Existing local EngineClient probes and synthetic isolation/pipeline studies
are different evidence levels; do not transfer their ratios to a production client.
See [backend usage](BACKENDS.md), the [runtime-isolation study](../research/backend-experiments/2026-11-06-runtime-isolation/README.md)
and the [implementation review](reviews/execution-groups-supervision-implementation.md).

## 2. Genuinely Tokio-free thread builds

Separate features and synchronization so a thread-only application does not
compile or transitively depend on Tokio. Preserve the existing default build.

- Address shared Response, completion, monitoring, timers and macro-generated
  code; removing one Cargo dependency is insufficient.
- Design milestone 1's public ownership types with this feature boundary in mind;
  keep Tokio-specific handles behind the async feature rather than requiring
  Tokio types in the thread-only API.
- Require a feature-matrix CI check showing no Tokio in the thread-only graph,
  with working protocols, links, monitors, timers, metrics and shutdown.

See [Silver/flux Project 1](research/silver-flux.md) for the dependency boundaries.
This does not commit Spawned to replacing Tokio for async applications.

## Then, guided by application needs

- Deferred typed replies, delegation and pipe-to-self first: distinguish reply
  ownership from dispatch, bound pending work and define late/owner-exit results.
- Bounded admission, pre-allocation byte credits and opt-in inherited request
  deadlines as one design package. Credits work independently of optional metrics.
- Typed streams with flow control, completion watermarks and explicit success
  acknowledgement; ownership release alone does not establish business success.
- Causal tracing, bounded role/cohort metrics and runtime inspection.
- Explicit batching/coalescing and targeted dispatch optimizations.

Start client profiling in parallel from the first milestone, alongside failure
testing, simulations and stress campaigns. A representative workload must establish
the performance case before promoting a policy. The
[local Engine-client probes](../research/client-observations/2026-11-06-ethlambda-ab/server-probe/README.md)
are useful scheduling evidence, not a full-client performance result. Improve
existing backends before adding names.

The [ranked backlog](research/idea-priorities.md) records the wider proposals and
tradeoffs. [Detailed acceptance criteria](design/roadmap-acceptance-2026-11-06.md)
and [research conclusions](research/commonware.md) remain reference material;
this page defines the current order. No speculative feature is promised by a date.
A broader brainstorm catalog with design notes is in
[framework ideas](research/framework-ideas.md); the formal-model and testing-tool
plan is in [verification strategy](research/verification-strategy.md).
