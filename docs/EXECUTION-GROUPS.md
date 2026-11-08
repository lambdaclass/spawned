# Execution groups and OneForOne supervision

Available on main, unreleased. These are opt-in capabilities for async actors;
ordinary `.start()`, message types, handlers and protocols retain their behavior.
The initial profiles are parked async pools and OneForOne recovery. A supervisor
owns one ordered child set; nested supervisor child specs are not implemented. Pinned or
spinning groups, sized blocking-actor pools, OneForAll and RestForOne remain planned.

## Choose an owner and a worker budget

```rust
use spawned_concurrency::{system::ActorSystem, tasks::ActorStart};
use std::time::Duration;

// MyActor is your application-defined async actor.
let system = ActorSystem::new()?;
let critical = system.async_group("critical", 1)?;
let background = system.async_group("background", 3)?;
let actor = MyActor::new().start_on(&critical)?;
actor.ready().await?;
let report = system.shutdown(Duration::from_secs(5)).await;
assert!(report.complete);
```

Groups own separate Tokio runtimes. ActorRef messages and requests cross those
runtimes normally; moving a reference does not move an actor or an I/O resource.
Use a small number of subsystem groups, rather than creating a pool per actor.
Construct groups during application setup: construction is synchronous and
fallible, and no actor is admitted to a failed group.

The worker budget counts async workers, not all process threads. Each system
also has a control thread and each group a runtime-owner thread. Tokio blocking
operations can create extra threads. A group is neither CPU affinity nor an OS
CPU reservation; blocking a worker or a handler still requires cooperation.
The [backend studies](BACKENDS.md) explain the workload-specific evidence.

ActorSystem owns managed lifetimes. Cloned group/reference/context handles do
not retain live workers after successful system shutdown. Dropping ActorSystem
requests shutdown without blocking its calling thread; await `shutdown` when
cleanup completion matters. Keeping an incomplete report lets you await again:

```rust
let report = system.shutdown(Duration::from_millis(50)).await;
if !report.complete {
    eprintln!("unfinished groups: {:?}", report.unfinished_groups);
    let final_report = report.wait(Duration::from_secs(10)).await;
    // Report/escalate unfinished work if it is still incomplete.
}
```

Timeout closes admission and reports unfinished groups/actor scopes and observed exits.
An exited actor can still have an unfinished scope while registered helpers clean
up, so its ID may appear in both fields. System shutdown orders supervisor trees
and standalone actors together by reverse admission. The first shutdown deadline
bounds these serial waits; at expiry it signals the remaining targets and retains
all actual cleanup. Drop uses a five-second ordering grace. Later report waits do
not reopen admission or extend that initial ordering grace.
It does not kill native code or abandon runtime ownership. The cleanup coordinator
keeps closed runtimes alive until tracked work actually finishes, even if an
incomplete report is dropped. An uncooperative operation can therefore keep
threads/resources alive indefinitely; the host decides process-level escalation.

## Construct fresh children after failure

```rust
use spawned_concurrency::supervision::{ChildSpec, RestartMode, SupervisorConfig};

let mut tree = system.supervisor(SupervisorConfig::default())?;
let service = tree.add(
    ChildSpec::new("service", &critical, |factory_ctx| async move {
        // This factory is invoked and polled on the assigned execution group.
        // Return a fresh, unstarted actor; loading durable state is application work.
        Ok::<_, String>(MyActor::new())
    }).restart(RestartMode::Transient)
)?;
let supervisor = tree.start().await?;
let current = service.current()?;
// After failure, explicitly resolve a generation greater than a saved snapshot.
let generation = service.snapshot().generation;
// let replacement = service.wait_for_generation(generation).await?;
let report = supervisor.shutdown(Duration::from_secs(5)).await;
```

A factory may return an error or panic. Registered construction helpers are
cancelled and joined before retry, including when the factory never returns an
actor. Existing `started()` and `stopped()` hooks keep their signatures. Readiness
commits after `started()` returns and admission remains open; no ordinary handler
runs before that barrier. Enqueued startup messages are discarded if startup fails.
Calling `ready()` later reports that historical barrier, not current liveness.
Failed readiness waits for the actual startup outcome, so a non-returning hook
can keep it pending; use a timeout. A startup deadline is a failed attempt and can retry under Permanent/Transient
policy after cleanup, whether it expired in the factory or in `started()`.
A self-requested normal stop before readiness is still a
startup failure: a Transient/Temporary spec fails the tree rather than claiming
a ready service; a Permanent spec can retry within its budget.

A supervisor accepts only groups owned by its ActorSystem. ActorRefs can still
communicate between independent systems. Children start in declaration order
and shut down in reverse order. Startup
future cancellation requests rollback of the partially started tree. Each spec
has a startup and cleanup timeout (default five seconds). A stuck actor, startup
hook or registered helper fails the supervisor and suppresses replacements until
actual cleanup completes. There is at most one live generation per child slot.
Stop observation uses internal tokens/completion state, independently of ordinary
mailbox traffic. It does not interrupt a running handler. If a supervisor report deadline expires,
remaining siblings are also signalled before returning the incomplete report;
actual completion still waits for the stuck work.

| Restart mode while supervisor is running | Normal / Shutdown | Panic / Kill |
|---|---|---|
| Permanent (default) | Restart | Restart |
| Transient | Stay stopped | Restart |
| Temporary | Stay stopped | Stay stopped |

System/supervisor shutdown suppresses every restart mode. Stopping an individual
Permanent child while its supervisor is open intentionally restarts it. Use
supervisor shutdown to close the tree. `ChildHandle::shutdown()` commits Shutdown
before cooperative cancellation while preserving a previously committed cause;
actual handler or cleanup panics remain failures. Existing link propagation still
applies and can affect siblings explicitly linked by the application.

The default shared budget permits five retry attempts per rolling 30-second
window. Initial construction does not consume a retry; every subsequent attempt
does, including failed factories and normal replacements of Permanent children.
A zero retry budget fails immediately without waiting for backoff. Otherwise
retry accounting is stamped immediately before factory admission, after the
cancellable delay; retries spaced beyond the rolling window are permitted by
design. This is a rate limit, not a lifetime retry cap. Exhaustion fails the whole supervisor and stops its remaining children. Backoff
is capped exponential with optional jitter, independently per child; a stable
ready lifetime resets that child's exponent. Cleanup time is not healthy uptime.
Use `Backoff::seed` for reproducible policy tests, not security randomness.

## References and effects

A restarted actor has fresh state, a fresh mailbox and a fresh ActorId. Old
ActorRef/Recipient values remain attached to the stopped instance. `current()`
returns a ready instance or an unavailable state; `wait_for_generation(n)` waits
for a ready generation strictly newer than `n`. Either can race with a later
failure, so delivery errors remain normal outcomes. Snapshot generations count
construction attempts, including failed attempts before an ActorId exists.
Snapshots are asynchronous diagnostics and may briefly show Running after a stop
signal; `current()` already rejects lookup when supervisor admission closes.

Queued messages are discarded, incomplete request channels fail when their owner
is dropped, and caller timeouts can occur earlier. No messages are replayed.
Effects already performed are not rolled back. Supervision supplies neither
automatic durable state recovery nor exactly-once effects.

## Owned helpers and registrations

Managed `Context` and `FactoryContext` offer `spawn_helper` and
`spawn_blocking_helper`. Dropping the returned join handle does not detach scope
ownership. Async helpers are cooperatively cancelled on actor stop; running
blocking helpers cannot be aborted and retain cleanup ownership until their
native closure returns. Helpers report cancellation/panic via `HelperError`.
Rejected helper admission returns an error. System/supervisor shutdown closes
helper/timer/monitor/registration admission for the whole managed tree immediately,
including earlier actors awaiting their stop turn. Those actors may still serve
ordinary requests, but must handle helper rejection. Prepare shutdown resources
in advance; this is stop-and-join, not an admission-preserving drain mode. Never synchronously wait for a helper
on the sole async worker it needs in order to make progress.

Framework timers, stream listeners, deferred message helpers and monitor waiters
use the managed actor's group even when registered from another runtime or a
plain thread. Their existing infallible APIs return inert/cancelled work after
closure, with no ambient fallback. Aborting their task handle releases its scope
ownership after payload destruction. Legacy timers remain tied to the old
instance; they never retarget a replacement.

`ctx.register_name(name)` owns a registry guard until that generation's actor and
registered helpers finish cleanup. Guard removal checks an ownership nonce so
an old generation cannot delete a newer registration. Named registry instances
and transparent reference rebinding are not implemented.

Arbitrary `tokio::spawn`, nested blocking/native work and ambient `.start()` do not
automatically enroll in a scope. Track them explicitly or accept their unmanaged
lifetime. An ambient actor created inside a group may be aborted by runtime
teardown without running `stopped()`. Actor `join()` still waits for that actor;
use supervisor/system shutdown to await registered helpers and runtime teardown.

## Run the examples

- [Execution groups](../examples/capabilities/src/bin/execution_groups.rs): router/worker cross-group request, target-group helper and owned shutdown.
- [Supervision recovery](../examples/capabilities/src/bin/supervision_recovery.rs): crash, new identity/fresh state, stale reference failure and explicit replacement lookup.

```sh
cargo run --locked -p spawned-capability-examples --bin execution_groups
cargo run --locked -p spawned-capability-examples --bin supervision_recovery
```

The [design contract](design/execution-groups-supervision.md) records the fuller
boundaries. [Failure tests](TESTING.md#managed-execution-and-recovery-main-unreleased)
exercise readiness, cancellation, rollback, restart limits and incomplete cleanup.
These APIs do not establish a full-client performance improvement; measure actual
application workloads with equal resource budgets.
