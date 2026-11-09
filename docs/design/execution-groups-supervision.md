# Execution groups plus supervision: MVP contract

Status: **owned async/native/pinned groups and nested supervision strategies available in v0.7.0**. Updated 2026-11-09.
See the [usage guide](../GUIDE.md#execution-groups-and-supervision) for actual signatures and supported limits.
This document records the design contract; illustrative API shapes below are not copyable code.
v0.7.0 also includes the dedicated-runtime worker-budget correction. Ordinary handlers, protocols and `.start()` remain usable.

The combined goal is straightforward: an application chooses where its actors
run, and the framework can reconstruct a failed child in that same execution
group with predictable resource, reference and shutdown behavior.

## Delivery order

| Increment | Deliverable | Runnable example / gate |
|---|---|---|
| 1a | Optional ActorSystem owning named execution groups, fallible group construction, placement, registered helper scopes and explicit shutdown | Critical router and background work on separate parked async pools; messages cross groups; partial construction unwinds; actors/helpers stop before group teardown. |
| 1b | ChildSpec factories, OneForOne, restart modes, restart budget/backoff/jitter, readiness and internal lifecycle control | Fail one child, reconstruct it in its original group, reject an old instance reference, resolve the replacement explicitly, then shut down during restart backoff. |
| Implemented follow-up | Sized persistent native actor pools and parked Linux pinned shards; nested supervisors, OneForAll and RestForOne | Resource/capacity contracts, explicit shards, ordered cohort barriers, retained subtree cleanup and partial-construction rollback tests. |

Design 1a and 1b together before freezing the API. Ship each working increment
with asserted examples and failure tests. Do not expose placeholder backend
variants; the usage guide distinguishes implemented profiles from later increments.

## Ownership and proposed API shape

| Proposed type | Responsibility |
|---|---|
| ActorSystem | Own groups, admitted actors, supervisor trees and registered helper scopes; reject admission after shutdown starts. |
| ExecutionGroup | Opaque placement handle with a name, worker budget, ownership state and fallible startup; no mandatory public Tokio type. |
| ChildSpec<A, F> | Reusable factory for a fresh actor, group placement, restart mode, readiness/startup limit and shutdown policy. |
| FactoryContext | Target-group spawn capability, attempt-generation token, cancellation and registered construction-helper scope before an ActorId exists. |
| Supervisor | Own child generations, restart accounting, timers and ordered lifecycle transitions. OneForOne initially. |
| SupervisedChild<A> | Explicitly resolve the current ready ActorRef or report starting, restarting, failed or stopped. It is not a transparently rebound ActorRef. |
| ShutdownReport | Record actual exits, incomplete work and resources still running when a deadline expires. |

Illustrative shape only; names and signatures need implementation review:

```text
system = ActorSystem(...)
critical = system.async_group("critical", workers = 1)
general = system.async_group("general", workers = 3)

spec = ChildSpec("importer", factory = make_fresh_importer)
       .on(critical)
       .restart(Transient)
       .shutdown(stop_then_join_with_deadline)

supervisor = system.supervise(OneForOne, spec, restart_budget, backoff)
current = supervisor.child("importer").current_ready_instance()
system.shutdown(deadline)
```

The framework invokes and polls the reusable factory in the target group's
execution context, including its async construction/IO, rather than in the
supervisor's or caller's ambient runtime. It supplies an unstarted actor; the
framework controls placement, generation observation and readiness.
Allocate a child-slot attempt token before invoking the factory and associate
its eventual ActorId/readiness with that token. FactoryContext exposes the
registered construction scope; arbitrary spawns remain unmanaged.

For managed actors, Context retains a gated group spawn capability. Framework
timers, monitor waiters and registered helpers use that capability even when
the method is called from another runtime or a plain native thread. Group
closure rejects new registration/placement, with the helper API's documented
error or cancelled/inert-handle outcome rather than an ambient-runtime fallback.

Existing unsupervised `.start()` remains ambient. Calling it inside a managed
actor does not enroll the new actor in its supervision tree or ordered cleanup,
even if it happens to run on the group's runtime. Treat it like unregistered
application `tokio::spawn`: group teardown may abort it without an async
`stopped()` hook. Managed children must use explicit system/group/supervisor
admission. This distinction also applies to arbitrary native threads.

Begin with owned parked async groups. Keep synchronization/ownership interfaces
compatible with the now-implemented Tokio-free native build boundary. Additional profiles need
explicit contracts: blocking actors currently occupy a blocking worker for their
whole lifetime; dedicated actors consume root threads and helper capacity;
pinning needs platform support and fallible affinity setup. A worker count is
not a promise about total process threads or OS-level CPU isolation.

Group construction must unwind partial worker startup and admit actors only
after success. ActorSystem owns the runtime/worker teardown state. Public group,
ActorRef and Context handles are weak or admission-gated capabilities, not
strong Runtime owners; keeping stale handles must not retain a successfully
shut-down runtime. Retention of unfinished managed work uses the explicit
pending-shutdown ownership described below, not an actor/runtime reference cycle.

Closing a group closes its admission gate and notifies affected supervisors.
They suppress further placement into that group and report/escalate the group
failure rather than endlessly retrying it. Group reconstruction or automatic
migration to a different group is outside the MVP.

## OneForOne recovery contract

Restart one failed child without directly restarting healthy siblings. Existing
application links still have their documented propagation behavior; supervision
does not silently disconnect those links. A replacement always gets a new
ActorId and mailbox.

Keep at most one live generation per child slot. Construct its replacement only
after the previous actor task/native thread and registered owned cleanup have
terminated. The current exit-reason notification alone does not prove arbitrary
helper or dedicated-runtime teardown completed. If a cleanup deadline expires,
mark the slot stuck/failed and escalate; never launch an overlapping generation
while its old actor/helper can still mutate the same resources.

| Mode while the supervisor is running | Normal / Shutdown exit | Panic / Kill exit |
|---|---|---|
| Permanent | Restart | Restart |
| Transient | Keep stopped | Restart |
| Temporary | Keep stopped | Keep stopped |

Once supervisor/system shutdown or explicit child removal begins, **all modes
suppress restart**. Sending a stop to a Permanent child while its supervisor is
still running does not remove it: its restart is intentional policy behavior.
Current `ExitReason::is_abnormal()` is useful for Transient classification, not
for deciding whether Permanent children restart. Managed shutdown now produces Shutdown.
The new supervisor stop primitive commits Shutdown before cancelling through
the existing wakeup path, without replacing an already committed cause. Current
`normal_stop` already preserves an earlier cause; it is not an unconditional
overwrite. Actual handler/cleanup panics remain reported as failures.

Shutdown is clean for Transient restart classification, but is not the same as
Normal link propagation: non-trapping linked peers are cancelled by Shutdown;
trapping peers observe it as an exit signal. Kill remains untrappable and
cooperative. Do not derive link behavior from `is_abnormal()` alone.

Fallible async construction belongs in the factory's Result in the MVP; it
does not add a fallible `try_started` hook or change existing Actor methods.
Readiness is a new barrier after the existing `started()` hook completes and
the generation's admission gate is still open. Install exit observation before
running the actor. Publish readiness and permit user-message dispatch only after
that barrier wins against stop/deadline. A reference can enqueue self-messages
during startup, but no ordinary handler runs before readiness; failed startup
discards those queued messages with the existing reply/drop behavior.

| Startup outcome | Hook/cleanup contract |
|---|---|
| Factory fails/panics before producing an actor | No actor hooks run; cancel/join registered construction work and release partial resources. |
| Factory produces an actor, but admission closes before `started()` | Drop the unstarted instance and its owned construction work; no `stopped()` hook. |
| `started()` panics | Preserve the existing panic behavior: no `stopped()` hook; cancel/join owned startup helpers and perform terminal cleanup. |
| Stop or startup deadline while `started()` is running | Request cooperative stop. If the hook returns, run `stopped()` and cleanup without publishing ready or dispatching user work. If it cannot return/clean up within policy, report stuck work and do not start a replacement. |
| `started()` completes while admission remains open | Commit readiness for that generation, then allow ordinary dispatch. |

Factories/startup futures run as tracked child operations, not awaited inside
the supervisor's serial control handler. Its control loop must remain responsive
to shutdown; cancelling an attempt closes its admission before any late result
can start/publish a child. Blocking Rust in a factory/hook remains cooperative.

Initial children start in declaration order. If startup cannot complete within
policy, stop already-started children in reverse order and report the failure;
never report a partly ready tree as ready. Non-readiness is a startup failure
even if the underlying stop reason is Normal; do not confuse it with an already
ready Transient child finishing normally.

Enforce a **supervisor-wide** retry-attempt maximum in a rolling monotonic-time
window, across all its children. A child's first startup attempt is not a
restart; after a failure, reserve a budget entry before invoking each retry's
factory. A failed factory/readiness retry consumes that entry too. Permanent
children's normal-exit replacements also consume entries. Exhaustion fails the
supervisor, stops remaining children in reverse order and escalates explicitly.

Track capped exponential backoff and jitter **per child slot**. Its stable-running
reset timer starts only at readiness and resets that child's exponent only if
the same generation remains ready/alive for the configured interval. Another
sibling's healthy lifetime cannot reset it; a never-ready generation cannot
earn a stability reset. Group closure or cleanup-stuck failure bypasses retries.
Inject a clock and jitter source in policy tests so timing failures are replayable.
Cancellation of a restart timer and shutdown admission use the same state gate.

## References, work and effects

| Item at instance failure | MVP behavior |
|---|---|
| Actor state | Destroy the failed instance and construct fresh state. Recovery from storage belongs in the user factory/application. |
| Old ActorRef / Recipient | Continue to refer to the old stopped instance. Never silently redirect to its replacement. |
| Queued messages not dispatched | Discard them with normal drop/accounting behavior. Do not move them into the replacement's mailbox. |
| Requests without a completed reply | Fail when their reply owner/channel disappears, unless the caller already timed out. No automatic retry. |
| Effects already performed | Remain possible even if the caller gets an error. Supervision is neither rollback nor exactly-once delivery. |
| SupervisedChild lookup | Return the current ready generation or an explicit unavailable state. Resolving then sending can still race with a subsequent crash. |
| Registrations and timers | Tie owned registrations/timers to an instance generation. Old cleanup must not remove a replacement's registration or act on its generation. |

Current queued reply senders can remain alive until `stopped()` completes and
the mailbox is discarded; a queued caller can time out during that cleanup.
Do not promise immediate ActorStopped for all requests at the first exit signal.
Context-bound timers already target an old instance's sender/token, not the
replacement mailbox. Track/join them as owned helpers. New lifecycle-owned
registry guards remove only a matching `(name, ActorId)` registration before
cleanup is considered complete; the existing unconditional `unregister(name)`
is not a safe replacement for that operation.

A non-restartable initial child failure fails startup; retries of restartable
children use the policy above. No helper from a cancelled attempt may publish
readiness or install a registration for a newer generation.
Explicit state recovery, durable outboxes, idempotency and logical proxy routing
are separate application capabilities, not implied by ChildSpec.

## Lifecycle control and shutdown

Use a framework-internal control path for terminal child events, stop and
supervisor state changes. It must not sit indefinitely behind an application's
mailbox backlog. It still cannot interrupt a running non-cooperative handler.
Observe generation completion/readiness state directly alongside the supervisor's
control commands, not through ordinary FIFO `Handler<Down>`/trapped-exit messages.
Existing stop cancellation already prevents further queued user dispatch; the
native-thread sentinel is an idle-receive wakeup, not a requirement to process
all earlier user handlers. This milestone adds reliable supervisor observation
without replacing those stop semantics with general message priorities.

Bound control memory by admitted children/generations and command capacity.
Terminal state must remain observable under saturation: store/coalesce it per
generation with a wake notification rather than dropping the only exit event.
Reject duplicate/stale generation events, bound administrative requests and
specify fairness between lifecycle control and ordinary messages. General
user-defined message priorities are outside the MVP.

System shutdown closes new actor/helper admission and cancels restart/startup
activity first. Stop supervised children in reverse declaration order, finish
their registered cleanup, then tear down execution groups. The initial policy
is explicit stop-and-join with existing queued-work discard semantics; bounded
draining is an additional opt-in policy, not a silent change to ordinary stop.
Standalone scopes currently stop in reverse admission order. There is no declared
dependency graph. A future graph API must reject unknown dependencies and cycles,
define ownership across supervisor boundaries, and preserve deadline signalling
and cleanup ownership; it needs model/trace coverage before changing ordering.

Track framework-owned and explicitly registered helpers. Arbitrary application
`tokio::spawn`/native threads are not magically owned. Define helper admission,
cancellation and joining with the API, including helpers waiting on their owner.
After its helper scope closes, cleanup awaits existing helpers or performs I/O
inline; it cannot register fresh background work behind the shutdown gate.
At a deadline, return an **incomplete** ShutdownReport with an owned pending-work
handle. It retains the affected groups' closed teardown state and can be awaited
again; admission remains closed. Do not tear down an affected runtime underneath
unfinished registered cleanup, block the caller forever in Runtime::drop, or
declare shutdown complete while that work remains alive. Runtime destruction
belongs on its teardown owner, outside an async caller's executor thread.

Rust cannot forcibly kill a blocking thread. Explicitly dropping the system and
pending-work handle is best-effort nonblocking disposal: async work may be
aborted and native/blocking work may survive. That is distinct from successful
awaited shutdown. Preserve actual cleanup failures in the report rather than
relabeling every exit as successful Shutdown.

## Acceptance and examples

- Build two named groups, communicate across them, then stop them cleanly;
  reject construction/placement failures without leaking workers or actors.
- Crash a child and fail its constructor/readiness. Check mode decisions,
  restart placement, new identity, old-reference failure and sibling continuity.
- Prove intensity limits and bounded backoff/jitter; race shutdown/removal with
  crash, readiness and an expiring restart timer. No child starts after closure.
- Keep factories and external Context-helper calls on the assigned group;
  exercise calls from another runtime and a plain thread. Verify unmanaged
  `.start()` behavior and that stale handles cannot retain completed runtimes.
- Hold startup/owned cleanup past a deadline: report incomplete work, keep
  admission closed, prevent overlapping generations and allow later joining.
- Reject stale exit/registration events from an old generation. Check queued
  payload destruction, failed replies, permit/accounting cleanup and helper exit.
- Flood ordinary work while lifecycle control progresses, with bounded control
  memory and ordinary-work fairness. Cover cooperative blocking limits honestly.
- Ship examples for runtime placement, OneForOne recovery, each restart mode,
  stale-reference resolution, restart storms and partial-startup shutdown.
- Exercise supported async/native-thread modes with the same policy contract.
  A profile is supported only when its placement/lifecycle adapter is tested.

The [local Engine-client studies](../../legacy/research/client-observations/2026-11-06-ethlambda-ab/server-probe/one-worker/README.md)
support explicit placement and worker budgets. A real client canary/replay is
still needed before claiming a production performance benefit. Recovery policy
has its own correctness gates; performance gains do not establish recovery safety.

## Implemented follow-up contracts

The [usage guide](../GUIDE.md#execution-groups-and-supervision) defines the current resource models,
cohort policies and subtree API. Native pools lease one persistent worker per live
scope; per-handler leasing remains research. Pinned groups are parked Linux
reactors, with no spin policy. All actual cleanup precedes replacement; each retry
factory (including collateral children) consumes the rolling restart budget.
Nested adapters retain an uncancelled ownership lease through descendant cleanup.
Thread-only native actors omit Tokio, while ActorSystem/supervision remain async.
