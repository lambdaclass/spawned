# Spawned roadmap

Updated 2026-11-08. This is the implementation order, not a release schedule.

Business handlers stay ordinary Rust. The framework owns dispatch and lifecycle.
Keep typed messages/replies, dynamic actors, addressed delivery, cooperative
handlers and the existing default behavior. New policies are explicit and opt-in.

## Available in v0.6.0

Typed protocols, async/blocking/thread execution, actor identities, monitors,
links and trapped exits, linked startup, shutdown fixes, optional framework
metrics, and reproducible backend research. Automatic supervision is not yet
implemented. Thread actors need no active Tokio executor but still compile Tokio.

## 1. Execution groups plus supervision

This is the next combined capability milestone. Design ownership and recovery
together, then ship the following two increments. See the
[MVP design and acceptance criteria](design/execution-groups-supervision.md).

### 1a. ActorSystem and execution groups

Introduce explicit runtime handles and execution groups through a supported API.
Start with a parked shared/isolated pool, bounded workers and explicit ownership.
An optional ActorSystem owns named groups, admitted actors and registered helper
scopes. Keep `.start()` unchanged; placement API names are still proposals. Start
with parked async pools. Sized blocking pools, dedicated/pinned profiles and a
general resource scheduler can graduate in subsequent increments with their own
resource and shutdown contracts.

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
  Local Ethlambda EngineClient/actor probes have run, including concurrent RPCs
  and idle gaps; a full-client replay or same-node production canary is still
  outstanding. These are different validation levels.

### 1b. Automatic supervision and recovery

Start with ChildSpec factories and **OneForOne**, with Permanent, Transient and
Temporary modes. Remember each child's execution group across restarts.

- Define fallible startup/readiness, declaration-order startup and reverse-order
  shutdown, restart intensity/window, capped exponential backoff and jitter.
- Suppress restarts before system/supervisor shutdown starts. Use an internal
  lifecycle-control path that cannot be buried behind application traffic;
  bound/coalesce it and preserve fairness. No public user-message priorities yet.
- A restarted child is a fresh actor with a new identity. Existing ActorRef
  values remain instance-bound; an explicit supervised-child handle can resolve
  the current instance. Queued work is discarded and unanswered requests fail
  when an instance exits. No automatic replay, state restoration or effect retry.
- Add OneForAll and RestForOne after the basic lifecycle and cleanup are proven.
  Non-cooperative blocking code still cannot be forcibly terminated.
- Ship runnable recovery/placement examples and deterministic failure scenarios
  with the API, including restart storms and shutdown during backoff/startup.

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
