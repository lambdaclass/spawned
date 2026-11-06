# Spawned roadmap

Updated 2026-11-06. This is the implementation order, not a release schedule.

Business handlers stay ordinary Rust. The framework owns dispatch and lifecycle.
Keep typed messages/replies, dynamic actors, addressed delivery, cooperative
handlers and the existing default behavior. New policies are explicit and opt-in.

## Available in v0.6.0

Typed protocols, async/blocking/thread execution, actor identities, monitors,
links and trapped exits, linked startup, shutdown fixes, optional framework
metrics, and reproducible backend research. Automatic supervision is not yet
implemented. Thread actors need no active Tokio executor but still compile Tokio.

## 1. Public runtime and placement API

Introduce explicit runtime handles and execution groups through a supported API.
Start with a parked shared/isolated pool, bounded workers and explicit ownership.
Keep `.start()` unchanged; placement API names are still to be designed. Pinned
placement, registry namespaces and a general resource scheduler can graduate
separately rather than expanding the first deliverable.

- Own runtime/helper lifetimes and define failed spawn, stop/join and shutdown.
  Construction must unwind partial worker startup without waiting for workers
  that failed to start; admit actors only after successful runtime construction.
- Design helper-registration closure, cancellation and joined completion before
  freezing the public ownership API. Begin with explicit stop-and-join; bounded
  draining is a separate opt-in policy that reports unfinished work at expiry.
- Start with parked pools and explicit thread/CPU budgets; affinity is optional
  with documented platform support. Spinning remains experimental.
- Validate communicating runtimes and one Ethrex/Ethlambda workload under equal
  resource budgets. Synthetic pipeline/isolation gains are workload-specific.
  No actual Ethrex/Ethlambda performance run has been completed in this research.

See [backend usage](BACKENDS.md) and the [runtime-isolation study](../research/backend-experiments/2026-11-06-runtime-isolation/README.md).

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

## 3. Supervision and recovery

Build supervisors on the existing lifecycle primitives: child factories,
OneForOne/OneForAll/RestForOne, restart limits, backoff and escalation.

- Design fallible startup, lifecycle-owned registry entries, control-event
  ordering and coordinated shutdown alongside supervision.
  Specify a minimal fair lifecycle-control path, including boundedness and
  notification ordering; a generic public priority-mailbox API can wait.
- Define replacement identities, stale references, state recovery and pending
  requests. No implicit rebinding, exactly-once effects or automatic retries.
- Test startup failures, restart storms and cleanup across execution modes.
  Running blocking Rust code remains cooperative.

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
the performance case before promoting a policy; none has yet supplied an
Ethrex/Ethlambda performance result. Improve existing backends before adding names.

The [ranked backlog](research/idea-priorities.md) records the wider proposals and
tradeoffs. [Detailed acceptance criteria](design/roadmap-acceptance-2026-11-06.md)
and [research conclusions](research/commonware.md) remain reference material;
this page defines the current order. No speculative feature is promised by a date.
