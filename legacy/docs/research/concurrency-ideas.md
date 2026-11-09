# Ideas from other concurrency frameworks

Researched 2026-11-06 by an independent research agent against current primary
sources and Spawned's roadmap, backend guide and Silver/flux research. Proposed
experiments below are our inference, not source performance claims. This page is
reference material; section numbers are identifiers, not ranks. Order and
priority live only in [idea priorities](idea-priorities.md).

Keep Tokio as the default, typed ordinary Rust payloads, addressed delivery,
dynamic actors, request/reply and cooperative handlers. Existing backend research
already covers affinity, parking/spinning, stealing, batching, runtime isolation
and I/O reactors; the following contracts address different problems.

## 1. Propagated request budgets and explicit cancellation

[Orleans cancellation](https://learn.microsoft.com/en-us/dotnet/orleans/grains/cancellation-tokens)
supports cooperative request cancellation and skipping cancelled work before
invocation. [Ractor factory job options](https://docs.rs/ractor/0.16.5/ractor/factory/job/struct.JobOptions.html)
include a queue TTL. Spawned currently times out the caller's reply wait; it
intentionally keeps accepted work running, and nested calls get independent waits.

Test an opt-in process-local monotonic deadline carried in envelope metadata and
expose its remaining budget through Context. Start by skipping work expired before
dispatch. Compare a three-actor chain with one slow stage: count handlers starting
after expiry, useful completions and caller outcomes. Do not alter ordinary
request semantics or forcibly drop active handlers. Timeout cannot prove no side
effect occurred; retries can duplicate effects. Define expiry-before-execution
separately. Implementation cost: medium.

## 2. Seeded delivery simulation with an audit digest

[Commonware's deterministic runtime](https://docs.rs/commonware-runtime/2026.9.0/commonware_runtime/deterministic/index.html)
controls simulated scheduling/environment facilities; its
[Auditor](https://docs.rs/commonware-runtime/2026.9.0/commonware_runtime/deterministic/struct.Auditor.html)
helps check deterministic interactions. Spawned's generated seeds currently
reproduce operations, not Tokio/OS schedules.

Begin with test-only gates at actual actor delivery boundaries, three actors,
scripted stop/link/request races and controlled timer inputs. Record actor
instance, message identity and lifecycle action; repeat seeds, compare digests
and shrink failures. This is delivery simulation, not full async schedule replay:
arbitrary awaits, native thread races, I/O and wall-clock reads remain uncontrolled.
Ordinary Rust messages can stay in memory. A trace digest detects divergence; it
does not reconstruct missing inputs. Cost: medium for a harness, high for a full
runtime.

## 3. Causal context propagated with sampled envelopes

[OpenTelemetry messaging conventions](https://opentelemetry.io/docs/specs/semconv/messaging/messaging-spans/)
explain producer/consumer context propagation and links for fan-out/batching.
Rust [tracing::Instrument](https://docs.rs/tracing/latest/tracing/trait.Instrument.html)
enters spans during future polling, avoiding a thread-local guard across awaits.

Queue and handler metrics reveal delay but not which ingress request caused an
actor chain. Test optional origin/causation context on ingress → router → two
workers → aggregator, across existing backends. Verify sampling propagation,
parent/link relationships and drop paths with a bounded in-memory collector.
Measure envelope size and disabled/enabled overhead. Avoid unbounded retained
spans, treating fan-out as one linear call chain, or confusing tracing with replay.
Cost: low–medium; complements the optional metrics.

## 4. Actor-owned helper completion scopes

Tokio's [TaskTracker](https://docs.rs/tokio-util/0.7.20/tokio_util/task/task_tracker/struct.TaskTracker.html)
can wait for tracked tasks and future destructors to finish. Closing its tracker
does not reject new registration, so admission needs a separate gate.
[Child cancellation tokens](https://docs.rs/tokio-util/0.7.20/tokio_util/sync/struct.CancellationToken.html)
propagate cancellation downward without cancelling parents.

Spawned already cancels framework timers/listeners, but actor completion does not
join every helper, and arbitrary spawned work is not actor-owned. Prototype an
explicit optional scope completion for framework helpers first; race helper
registration against shutdown and retain drop probes until completion. Keep
existing join semantics until a stronger guarantee is designed. Helpers awaiting
messages from the stopped owner can deadlock a join, and native blocking work
cannot be forcibly interrupted. Cost: medium.

## 5. Credits before payload allocation

[Pekko consumer-driven flow control](https://pekko.apache.org/docs/pekko/current/typed/reliable-delivery.html#only-flow-control)
grants production demand; [Tokio channel permits](https://docs.rs/tokio/latest/tokio/sync/mpsc/struct.Sender.html#method.reserve)
reserve admission before sending and return unused capacity on drop.

Beyond a bounded message count, obtain credits before decoding/allocating large
payloads and carry a permit across a pipeline until final completion. Start as an
adapter/example with ordinary messages. Measure live bytes and blocked producers
under bursts, panic, cancellation and stopped recipients. Specify whether credits
cover queue occupancy, active handling or completion; add byte budgets if needed.
Watch credit leaks/hoarding, head-of-line blocking and cyclic dependencies. Cost:
medium. Do not silently replace unbounded sends.

## 6. Durable outbox/idempotency before transparent restarts

[Pekko typed persistence](https://pekko.apache.org/docs/pekko/current/typed/persistence.html#effects-and-side-effects)
separates replayed event handling from external effects.
[At-least-once delivery](https://pekko.apache.org/docs/pekko/current/persistence.html#at-least-once-delivery)
requires persisted send intent/confirmation and permits duplicates and reordering.

Before a generic persistence subsystem, build an application-owned example with
serializable events, stable operation IDs, a durable outbox and receiver dedup.
Inject crashes around commit, send, effect and acknowledgement; assert the domain
invariant after explicit restart. Dedup must commit with the effect or use a
receiver's idempotency contract. Arbitrary boxed Rust envelopes are not a durable
format; replay must not repeat external I/O. Cost: low for a recipe, high for a
library subsystem. Useful once a client recovery need is identified.

The [deeper Commonware review](commonware.md) adds policy-based overload,
completion watermarks, explicit acknowledgement, lifecycle-bound work ownership,
scoped metrics and library capabilities. It also records
Seastar/CAF/cancellation-safety follow-ups. See [testing](../TESTING.md) for the
implemented reliability work; research proposals are not implemented features.
