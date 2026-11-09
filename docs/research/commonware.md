# Commonware and the next library capabilities

Reviewed 2026-11-06 against Spawned v0.6.0 and Commonware main at
[`b24eff769a8e61f92be9fc41dbdb45d17707ccaf`](https://github.com/commonwarexyz/monorepo/tree/b24eff769a8e61f92be9fc41dbdb45d17707ccaf).
This is a focused source review, not a performance comparison or adoption of its
runtime. Recommendations below are our inference from the linked primary sources.
Spawned's runtime module already mentions Commonware as deterministic-runtime
prior art; the earlier [framework research](concurrency-ideas.md) covered that idea.
No actual Ethrex/Ethlambda performance run has been completed in this research.
Client profiling should begin in parallel with the first implementation milestone.

## Library capabilities suggested by this review

This page is reference material; it does not rank. Order and priority live only
in [idea priorities](idea-priorities.md) and the [roadmap](../ROADMAP.md).
Execution groups and OneForOne supervision (the first two rows) have since been
implemented on main. Related ownership, shutdown and control-event contracts need
design before their APIs are frozen.

| Capability | Improvement for applications | First design boundary |
|---|---|---|
| Actor systems and execution groups | Own runtime resources, separate CPU/latency/general workloads, isolate registries and apply group budgets through a supported API. | Start with a parked pool, bounded workers, failure-atomic construction and explicit stop/join ownership; preserve existing start/reference behavior. Affinity, namespaces and general resource scheduling can graduate separately. |
| Supervision and recovery | Factories, restart policies, backoff, restart limits and escalation manage application failures. | Define new actor identity, stale references, state recovery and pending requests. No automatic retry of indeterminate effects. |
| End-to-end resource admission | Bound in-flight bytes and work, including producers and active processing, before allocating expensive payloads. | Credits must work independently of optional metrics. Define ownership/release, oversize rejection, deadline behavior and cyclic dependencies. A queue count alone is insufficient. |
| Typed streaming protocols | Framework-owned demand, bounded buffering, completion and cancellation for long actor pipelines. | Add opt-in stream protocols; ordinary requests keep existing semantics. Define slow-subscriber policy, bounded completion watermarks and explicit successful acknowledgement. |
| Generated dispatch optimization | Investigate avoiding framework boxing/allocations for closed protocols while preserving the flexible message path. | Profile a client first. Enum dispatch requires codegen/transport bridging and retained manual Handler support; payload and reply allocation costs remain separate. It is not automatically zero-allocation. |
| Explicit batching/coalescing | Amortize dispatch or replace obsolete state updates. | Per-protocol opt-in; transactional requests must not silently disappear or be acknowledged without their defined effect. |

Actor systems and supervision are capabilities, not scheduler micro-optimizations.
They can keep business methods ordinary Rust while the wiring layer selects
execution/recovery/resource policy. This is a design direction, not an existing
v0.6.0 API. Tokio remains the default. Lower-overhead optional metrics (for example
bounded per-worker aggregation) are another hypothesis to profile; the measured
metrics overhead does not isolate shared atomics as its sole cause.

Runtime construction must unwind partial worker startup and avoid readiness
barriers that wait for workers that failed to start; actor admission begins only
after successful construction. Keep Tokio-specific public handles behind the
async feature so the first milestone does not obstruct thread-only builds.
The benchmark driver's FD-exhaustion startup deadlock motivates this acceptance
criterion; it is not evidence of a production Spawned library bug.

## Additional Commonware ideas worth transferring

### Policy-based overload handling

Commonware's [mailbox](https://github.com/commonwarexyz/monorepo/blob/b24eff769a8e61f92be9fc41dbdb45d17707ccaf/actor/src/mailbox.rs)
has a bounded ready queue and policy-managed overflow, whose storage need not be
bounded. Policies can retain, replace/coalesce or consider work already satisfied;
an unreliable variant can reject. `Backoff` means semantically handled under the
policy, not necessarily processed as an individual message. The source explicitly
warns about re-entry/destructor deadlocks and mutation panics under the overflow
lock.

For Spawned, prototype latest-state updates with a bounded key set and explicit
superseded/rejected accounting. Keep unbounded ordinary mailboxes available. Do
not copy arbitrary policy callbacks under an internal lock, or apply coalescing
to transactional request/reply by default. This differs from a strict memory cap.

### Completion watermarks and explicit success acknowledgement

The [tracked channel](https://github.com/commonwarexyz/monorepo/blob/b24eff769a8e61f92be9fc41dbdb45d17707ccaf/utils/src/channel/tracked.rs)
tracks batches and a contiguous completion watermark; a guard is released after
all its clones are dropped. [Exact acknowledgement](https://github.com/commonwarexyz/monorepo/blob/b24eff769a8e61f92be9fc41dbdb45d17707ccaf/utils/src/acknowledgement.rs)
requires explicit acknowledgement by every clone; dropping one cancels its waiter.
These are different contracts: ownership release is not successful business work
or durable commit.

A pipeline could acknowledge all required branches before advancing a checkpoint.
Prototype fan-out with delayed, failed and abandoned branches. Bound the issued
sequence window: a retained early guard can prevent the watermark advancing while
later completions accumulate bookkeeping. Do not interpret an empty mailbox or a
successful reply-channel send as durable completion.

### Task ownership, cancel/join separation and safe teardown order

Commonware [spawn supervision](https://github.com/commonwarexyz/monorepo/blob/b24eff769a8e61f92be9fc41dbdb45d17707ccaf/runtime/src/lib.rs#L357-L451)
aborts descendants when a parent task finishes. Its
[handles](https://github.com/commonwarexyz/monorepo/blob/b24eff769a8e61f92be9fc41dbdb45d17707ccaf/runtime/src/utils/handle.rs)
distinguish owning spawned work from merely awaiting completion; aborting a
completion handle does not cancel the underlying operation. Abort-on-drop groups
signal cancellation before destroying owned outputs, and explicit abort can join.

Use this to shape opt-in actor-owned helper scopes and application shutdown.
Design registration closure, cancel/join separation and unfinished-work reporting
with the first runtime ownership API. Begin with stop-and-join; full bounded drain
can follow as an explicit policy rather than changing current stop behavior.
Do not make ordinary child lifetime depend on parent normal exit by default:
Spawned's links and normal-exit rules differ. Native blocking Rust remains
cooperative. A helper awaiting its stopped owner can deadlock a naive scope join.

### Determinism includes the environment and crash recovery

Commonware's [deterministic runtime](https://github.com/commonwarexyz/monorepo/blob/b24eff769a8e61f92be9fc41dbdb45d17707ccaf/runtime/src/deterministic.rs)
controls seeded task selection and time, audits interactions, supports storage
faults and recovers from checkpoints. A checkpoint retains simulated runtime/
storage state, not arbitrary live Rust futures. External processes complicate
reproduction. The runtime also has production Tokio and Linux io_uring paths.

Start Spawned with controlled delivery/handler progress, then virtual timers and
mocked dependencies if needed. Gate-only replay is not full scheduler/I/O replay.
Keep runtime policy plumbing hidden from business handlers instead of requiring
Commonware-style trait bounds throughout application code.

### Scoped observability and compatibility fixtures

Commonware's [context identity and metrics](https://github.com/commonwarexyz/monorepo/blob/b24eff769a8e61f92be9fc41dbdb45d17707ccaf/runtime/src/lib.rs#L265-L350)
separate static role prefixes from attributes. Registered metric handles unregister
on drop; dynamic attributes still increase cardinality. Spawned could add a
strictly bounded set of role/group/cohort aggregates alongside its existing global
metrics, with explicit cardinality limits, gauge lifetime and retained global
historical counters. Admission accounting must not depend on enabling metrics.

[Conformance fixtures](https://github.com/commonwarexyz/monorepo/blob/b24eff769a8e61f92be9fc41dbdb45d17707ccaf/conformance/src/lib.rs)
commit deterministic outputs across seeds into reviewed hashes. For Spawned,
consider normalized lifecycle traces for selected stable contracts. Normalize
actor identities and omit wall time, but retain causal/per-sender constraints;
do not freeze incidental interleavings or hide a behavior change by regenerating
fixtures. Existing generated tests and Loom remain necessary.

### I/O ownership before another reactor backend

Commonware's [io_uring implementation](https://github.com/commonwarexyz/monorepo/blob/b24eff769a8e61f92be9fc41dbdb45d17707ccaf/runtime/src/iouring/mod.rs)
combines task polling and per-worker rings. Registered operations can remain tied
to a worker even after a task moves; writes/syncs and reads/network cancellation
have different lifetimes, and shutdown drains registered I/O. These ownership
contracts are as important as queue throughput.

Any future Spawned reactor experiment needs pending-I/O drop, buffer ownership,
worker shutdown and cancellation tests. The existing I/O benchmarks do not
establish that a full runtime replacement is ready. The utility futures pools are
not admission-bounded by themselves; reservation helpers also retain already-built
values. Borrow ideas, not automatic memory-bound claims.

## Additional research beyond Commonware

- [Seastar resource semaphores](https://docs.seastar.io/master/tutorial.html#limiting-resource-use)
  motivate byte-based admission and rejecting requests larger than a budget.
  [Scheduling groups](https://docs.seastar.io/master/classseastar_1_1scheduling__group.html)
  motivate weighted service classes that can use spare capacity. Compare these
  against the existing fixed-runtime split. Handler elapsed time includes awaits
  and cannot be charged as CPU time; long polls still must cooperate.
- [CAF flows](https://actor-framework.readthedocs.io/en/stable/core/DataFlows.html)
  supply consumer demand and actor-owned subscriptions, with explicit buffer
  overflow strategies. Research slow-subscriber isolation for typed streams.
- [CAF delegation and response promises](https://actor-framework.readthedocs.io/en/stable/core/MessagePassing.html)
  motivate deferred replies. Delegation
  transfers reply responsibility; deferred replies retain it while processing
  other messages. [Pekko pipe-to-self](https://pekko.apache.org/docs/pekko/current/typed/interaction-patterns.html#send-future-result-to-self)
  maps helper results into actor messages. Bound pending operations and define
  late-result, deadline and owner-exit behavior without general reentrancy.
- Design a minimal fair lifecycle-control lane with supervision: define startup,
  monitor and trapped-exit ordering, boundedness and shutdown interactions. A
  generic public priority API can wait; queued priority cannot interrupt a
  running cooperative handler.
- [Tokio cancellation safety](https://docs.rs/tokio/latest/tokio/macro.select.html#cancellation-safety)
  distinguishes restarting a pending receive from cancelling a partially completed
  read/write. Audit future stream/I/O adapters accordingly; a deadline is not
  rollback. [Cooperative scheduling](https://docs.rs/tokio/latest/tokio/task/coop/index.html)
  does not forcibly interrupt arbitrary synchronous handler code.

These are capabilities to evaluate against one client workload, not instructions
to adopt every framework abstraction or launch more scheduler variants.
