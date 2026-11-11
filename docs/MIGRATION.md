# Migration guide

Upgrade notes per release, newest first. The [CHANGELOG](../CHANGELOG.md) lists
every change; this guide covers what you need to edit when upgrading.

## Unreleased

### Exit signals, startup and runtime drops as in OTP (ranks 5e, 5f, 5g, targets 0.8, in progress)

Design: [otp-exit-signals.md](design/otp-exit-signals.md) (its migration
table is the full list). What to edit:

- **`match` on `ExitReason`**: add a wildcard arm (`#[non_exhaustive]`, new
  `Killed` and `NoProc`).
- **`max_restarts` with OneForAll/RestForOne counts rounds** (rank 5f): one
  recovery round takes one grant, however many children it rebuilds (v0.7
  charged each rebuilt child). A cohort supervisor now tolerates more
  failures for the same `max_restarts`; if you had raised it to cover
  collateral rebuilds (for example `failures × children`), lower it to the
  number of failures you mean to allow. OneForOne is unchanged.
- **Kill from outside**: `ChildHandle::kill()` is the untrappable kill; the
  actor ends `Killed` and `stopped()` is skipped unless already running.
  `stop_with_reason(Kill)` stays an administrative stop ending `Kill`.
- **`stopped()` no longer always runs.** It is skipped when the exit is begun
  by a kill, an untrapped exit signal or link notice (a linked peer's
  abnormal death), or a runtime drop (decision 2, as OTP skips `terminate`).
  Cleanup that must run on every exit belongs in `Drop`. **To notice a
  skip**, enable `debug` logs for `spawned_concurrency`: every skipped
  `stopped()` logs `event = "stopped_skipped"` with the actor id and type,
  `cause` (`kill`, `exit_signal`, `runtime_drop`) and `reason`. It is logged
  for every skip, also for actors that do not override `stopped()`. The
  remedy for a supervised actor is `ctx.trap_exit(true)` in `started()`
  (below).

> **Supervised and system-owned actors: `stopped()` is skipped on shutdown
> unless the actor traps exits.** Supervisor and system shutdown send the
> parent's `Shutdown` exit signal (OTP `exit(Child, shutdown)`). A
> non-trapping child ends `Shutdown` at once (after its current handler)
> **without `stopped()`**, as a gen_server that does not trap exits skips
> `terminate`. An actor that needs `stopped()` for cleanup on shutdown must
> trap exits, as in OTP; it then finishes the messages queued before the
> shutdown and runs `stopped()`. A child that has not finished within its
> `shutdown_timeout` is killed (`Killed`, `stopped()` skipped if it has not
> begun); cleanup counts as stuck only after a second `shutdown_timeout`.
> Unmanaged `.start()` actors have no parent and are unaffected.
>
> ```rust
> #[actor(protocol = StoreProtocol)]
> impl Store {
>     #[started]
>     async fn started(&mut self, ctx: &Context<Self>) {
>         // v0.8: needed for stopped() to run on supervisor/system shutdown.
>         ctx.trap_exit(true);
>     }
>
>     #[stopped]
>     async fn stopped(&mut self, _ctx: &Context<Self>) {
>         self.flush_to_disk().await;
>     }
> }
> ```
>
> A trapping actor also receives linked peers' deaths as `Exit` messages
> (`exit_received`) instead of dying with them; trap only where that is
> intended, or move the cleanup into `Drop`.
>
> A trapping actor still handles the messages queued before the shutdown. To
> refuse that late work, check the new `ctx.exit_pending()` (both modes): it
> is `true` once the actor's exit has begun or the parent's exit is queued
> behind the current message (a peer's trapped `Exit` does not count). It is
> advisory: a signal can arrive right after it returns `false`.

- **Runtime drop** reports `Killed` (was `Kill` in tasks mode,
  `Panic("unexpected framework panic")` in threads mode).
- **Late link or monitor**: to an actor past its death point, `monitor`
  delivers `Down { reason: NoProc }` and `link` delivers `NoProc` (a
  non-trapping caller exits `NoProc`). Use `ctx.try_link` to get
  `Err(NoProc)` instead, and `ChildHandle::exit_reason()` for the real
  reason. `link` from an actor whose exit already began is inert.
- **A late signal during exit**: a non-`Normal` link notice or exit signal
  that reaches a non-trapping actor while it exits (for example during its
  `stopped()`) now replaces its reason. The regression test
  `late_link_failure_cannot_relabel_an_already_requested_normal_stop` was
  inverted deliberately into
  `late_link_failure_relabels_an_already_requested_normal_stop`.
- **Panic in `stopped()`** after an abnormal reason now becomes the reason
  (`Panic("panic in stopped(): ..")`), except after a kill.
- **Destructor panics** (actor state, queued messages) after the death point
  no longer change the exit reason: a `Normal` exit stays `Normal` and
  `ChildHandle::cleanup_failure()` reports the failure
  (`mailbox_cleanup_failure()` is a deprecated alias). A Transient child
  restarts on an abnormal reason **or** a cleanup failure, also after
  `Shutdown`; `ChildSnapshot::last_error` holds the failure.
- **`start_linked` children that trap exits** end when their parent ends,
  with the parent's reason (`Normal` included), through `stopped()`;
  `exit_received` no longer sees the parent's exit. Use `ctx.link` instead
  of `start_linked` to observe the parent's exit as an `Exit`.
- **`spawned-test` `faults::exit`** now sends the exact exit signal
  (`ChildHandle::exit`): `Normal` is ignored by a non-trapping target,
  `Panic(msg)` ends it with exactly `Panic(msg)`, `Kill` ends it `Killed`, a
  trapping target receives an `Exit`.
- **Supervisor start (rank 5f):** any first-start failure of a child (factory
  error or panic, panic or stop in `started()`, `startup_timeout`, placement)
  fails `SupervisorBuilder::start()`, whatever its restart type, with no
  opt-in retry (decision 7), as `SupervisorError::FailedToStartChild { child,
  cause, rollback_failures }`; match on it (or keep treating any `Err` as a
  failed start). Replacements still retry within the restart budget. A ready
  child that exits while later siblings start is no longer restarted at once
  (OneForOne) or fatal (OneForAll/RestForOne): its exit is processed after
  the initial start completes. `start()` now returns its error after the
  rollback, so it can take up to two `shutdown_timeout`s per started child; a
  child whose cleanup is stuck is reported `Failed` and its work stays owned
  by the system. **ethrex**:
  the Transient discovery child's factory (`DiscoveryServer::build`) can fail
  at boot when pinging a bootnode fails; v0.7 retried it, v0.8 fails
  `start()` and `init_network` panics on `.expect("Network starts")`. Fix in
  ethrex: make the bootnode ping best-effort (log and continue) so the
  factory does not fail on it, or retry `start()` at application level.
- **Caller errors carry the reason (with rank 11, decision 4).**
  `ActorError::ActorStopped` is now `ActorStopped(ExitReason)`: the actor's
  final exit reason, the one `wait_exit`, `Down` and linked actors see
  (`Killed` after `kill()`, the signal's reason after an untrapped exit
  signal, `Panic(..)` after a crash, `Normal`/`Shutdown` after a stop). A
  `send` or `request` to an actor past its death point returns the new
  `ActorError::NoProc` instead; one sent while the actor is still exiting
  (for example during its `stopped()`) is admitted and answered at the death
  point. Patterns change from `Err(ActorError::ActorStopped)` to
  `Err(ActorError::ActorStopped(_))` (or a specific reason); code that
  treated `ActorStopped` as "the actor is gone" for sends should also handle
  `NoProc`. A request whose handler panicked, or a deferred token dropped
  while the actor exits, is no longer answered at once: it is answered at the
  death point (after `stopped()`) with the final reason, or by the caller's
  own deadline first. With `metrics`, rejected requests count in the new
  `requests_noproc_total`. `ActorError` stays `#[non_exhaustive]`; instead
  of an exhaustive match, classify with `target_gone()` (`ActorStopped(_)`,
  `NoProc`), `is_timeout()` (`RequestTimeout`), `is_transient()`
  (`RequestTimeout`, `Overloaded`: retrying the same target may succeed;
  not `ReplyDropped`) and `exit_reason()` (the reason of `ActorStopped`).
  A variant added later is classified by these helpers. **ethrex**:
  `peer_handler.rs:1251` matches `ActorError` exhaustively on purpose, so it
  no longer compiles: add arms for `ActorStopped(_)` and `NoProc`, or replace
  the match with the helpers above; two tests that construct
  `ActorError::ActorStopped` must pass a reason (for example
  `ActorStopped(ExitReason::Normal)`). **ethlambda**: one test mock
  constructs `ActorStopped`; pass a reason the same way. A `spawned-test`
  `Probe` closed with `close()` now returns `NoProc`, and a probe without a
  reply answers `ActorStopped(NoProc)`.

### Deferred replies (rank 11, targets 0.8, in progress)

This is a breaking release. The notes below grow with the implementation:

- `ActorError` is `#[non_exhaustive]` and gains `ReplyDropped` and
  `Overloaded`; add a wildcard arm to exhaustive matches.

**Handlers.** `#[actor]` code is unchanged. A manual `impl Handler<M>` now
takes the request's reply obligation and returns `()`:

```rust
// before
impl Handler<GetCount> for Counter {
    async fn handle(&mut self, _: GetCount, _: &Context<Self>) -> u64 { self.count }
}
// after
impl Handler<GetCount> for Counter {
    async fn handle(&mut self, _: GetCount, reply: PendingReply<'_, u64>, _: &Context<Self>) {
        reply.reply(self.count);
    }
}
```

A handler that returns without answering a request now gives the caller
`ReplyDropped` (a panic still gives `ActorStopped`, with the final reason at
the actor's death point). Sends reach the same
`handle` with a detached `PendingReply`. To call a manual handler directly in
a test, use `spawned_test::reply_pair()`.

**Timeouts.** The request timeout starts at send, not at first poll.
`request_raw` gains the 5 s default; use `request_raw_until(msg,
Deadline::Never)` for the old unbounded wait.

**Raw API.** `request_raw` returns a `ReplyReceiver<R>` (a future in tasks
mode, `recv()` in threads mode) yielding `Result<R, ActorError>`.
`Response::from_with_timeout` is removed: build the deadline first,
`Response::from(actor.request_raw_until(msg, Deadline::after(t)))`.
`Receiver<M>` implementors implement `request_raw_until` (`request_raw` is
provided). A `recv_timeout` on the old native receiver has no counterpart:
the deadline is fixed when the request is sent.

**Metrics.** `metrics::Snapshot` gains `requests_reply_dropped_total` and
`requests_overloaded_total`, the `deferred_*` and `jobs_*` fields (see the
GUIDE's Metrics section) and the `deferred_reply_latency` histogram. Code
that builds a `Snapshot` with a struct literal must add them.

**Jobs (API refinements).**

- `JobOutcome`, `JobError` and `PipeErrorKind` are `#[non_exhaustive]`: add
  a wildcard arm, or match `outcome.into_result()` with an `Err(_)` arm.
- `tasks` mode: `pipe_blocking_with` / `pipe_reply_blocking_with` run on
  Tokio's blocking pool. Remove a `pipe_with(.., async { spawn_blocking(f)
  .await })` workaround (and its `resume_unwind` of the join error):
  `pipe_blocking_with(opts, move |_job| f(), done)` reports a panic as
  `JobError::Panicked`. **Size the native budget:** the default is
  `min(4, available_parallelism)` blocking jobs per actor, easy to outgrow
  (one client port needed 352), and under load its only symptom is
  `PipeErrorKind::NativeBudget` refusals (the first one per actor is logged
  as `native_budget_exhausted`). Raise it with
  `#[actor(max_native_jobs = N)]` (or `ChildSpec::max_native_jobs(N)` for a
  supervised child) instead of avoiding native jobs. A Tokio runtime being
  dropped now waits for blocking jobs still running, as for any
  `spawn_blocking`.
- To keep "exactly one result" when a job is refused, reserve first instead
  of cloning ahead: `let Ok(permit) = ctx.reserve_blocking_job() else {
  /* report with the values you still own */ };
  permit.pipe_blocking_with(opts, work, done);` (`reserve_job` for async
  jobs). Spawning from a permit cannot fail.
- A job that re-scheduled its messages with `send_after` (unfenced) can use
  `job.emit_at(at, msg)` / `emit_at_blocking`: fenced with the job, counted
  against its emit credits, and the terminal message waits for it.
- `JobOptions::default().deadline_after(d)` replaces
  `JobOptions { deadline: Some(spawned_rt::time::Instant::now() + d), .. }`;
  use `spawned_concurrency::Instant` instead of `spawned_rt::time::Instant`.
  Job deadlines are hard (later emits fail, the result is dropped); for a
  soft deadline, check the time in the work (see the GUIDE).

**Send-only manual handlers.** A manual `impl Handler<M>` whose `Result` is
`()` no longer needs the `_reply: PendingReply<'_, ()>` parameter: put
`#[send_handler]` (from `spawned_concurrency`) on the impl and write
`handle(&mut self, msg, ctx)`. Without it, a handler that drops its `()`
reply gives a `request` caller `ReplyDropped`; with it, the caller gets
`Ok(())`.

**Tests.** Replace test-only "run this closure in the actor" messages and
`cfg(test)` bypasses of handlers with `spawned_test::tasks::in_actor(&actor,
|state, ctx| ..)` (`threads::in_actor` in threads mode). `in_actor` runs in
a turn, so only after `started()` returned: it cannot replace a bypass
inside `started()` itself. If `started()` does work a test must skip (for
example `if handshake().await.is_err() { ctx.stop() }`), keep that work
injectable instead: pass the handshake (or its outcome) in the actor's
constructor, or move it to a message the test does not send, so the test
builds the post-handshake state directly.

**Lifecycle.** At death the registry closes (open deferred tokens answer
`ActorStopped`), then the mailbox is disposed, then actor state is dropped:
a state destructor that sends to its own actor now fails (with `NoProc`
once the exit-signals change above is in) instead of enqueueing into a
mailbox about to be dropped.

## 0.7.3

**Behaviour change (supervision).** A Transient child's replacement (any generation after the child was first
ready) that stops `Normal` or `Shutdown` from `started()`, or is shut down
before readiness, no longer fails its supervisor. It is a failed restart,
retried within `max_restarts` like a failed factory (factory errors are
retried too). Under OneForAll a failed replacement now starts a new recovery
round, so the whole cohort is stopped and rebuilt again, as in OTP; before,
only the failed child was retried. If you relied on a failing child to stop a
tree, stop it explicitly with `Supervisor::stop` or `Supervisor::shutdown`:
budget exhaustion is not a reliable stop, since an expiring
`restart_window` can keep granting retries. The first startup is unchanged.

Users of the simulation backend need `RUSTFLAGS="--cfg spawned_sim --cfg tokio_unstable"`;
normal builds are unaffected.

## 0.7.2

No API changes; update the version. Two inputs the macros used to accept now
fail to compile with a clear error, because they were already wrong:

- `#[actor(protocol = A protocol = B)]` without a comma between entries. Add
  the comma.
- `#[actor]` on a trait impl such as `impl Default for X {}`, which silently
  generated an `Actor` impl. Put `#[actor]` on the actor's inherent impl block.

Macro error messages are clearer and point at the offending tokens; code that
compiled before (apart from the two cases above) compiles unchanged.
`#[actor(protocol = P)]` on a generic actor now compiles.

## 0.7.1

`ActorId` Display now prints a bare decimal number, for example `3` instead of
`ActorId(3)`. Debug (`{:?}`) retains the tagged `ActorId(3)` representation.

Upgrade all Spawned dependencies together when adopting 0.7.1. This release
also introduces `spawned-test` as a published dev-dependency. Existing
default-feature and Tokio-free builds retain their feature selection.

A Kill received over a link is now trappable. Only a direct Kill (for example,
`child.stop_with_reason(ExitReason::Kill)`) bypasses `trap_exit`; this prevents a
direct kill from killing an entire graph of trapping observers. No exit-reason
variant was added. Committed abnormal/Shutdown causes survive queued-message
and actor-state destructor failures during cleanup.
Destructor diagnostics now distinguish `actor state destructor: ...` from
`actor lifecycle unwound: ...` (previously `actor lifecycle destructor: ...`).
Match the `ExitReason::Panic` variant rather than relying on its message text.

Restart capacity is reserved before backoff. Exhaustion fails immediately;
interrupted reservations are refunded, and only actual grants enter the rolling
window. Charging and retry-generation admission are atomic; a placement or
supersession that prevents admission refunds its reservation. A constructor or
`started()` failure after admission still consumes that retry grant. Cohort
recovery retains v0.7's per-child accounting: collateral OneForAll/RestForOne
replacements also consume grants (six replacements need six grants).
Pending backoffs now reserve capacity as well: with `max_restarts=1`, a one-second
window and a three-second backoff, a second child failing while the first waits
can exhaust the supervisor. Pending reservations do not expire with the rolling
window, so concurrency is capped even when `backoff.maximum > restart_window`.

Once admission closes, `SupervisedChild::current()` and `wait_for_generation()`
can now return `ChildUnavailable(Failed)` or `ChildUnavailable(Stopped)` for a
terminal owner/child, rather than always `ChildUnavailable(Stopping)` as in
0.7.0. Handle these terminal states as well as in-progress stopping.

Readiness is a historical commitment: if system admission closes after startup
commits readiness but before `start()` returns, startup still succeeds and the
tree waits for its ordered shutdown turn. Dropping that already-ready startup
future does not roll back the tree; ActorSystem still owns it.
Likewise, failure after readiness but before the caller resumes does not undo
startup success. Inspect `Supervisor::failure()`/the shutdown report for later
failure; `Ok(Supervisor)` does not guarantee continued liveness.

With jitter enabled, `Backoff::initial` is the first delay cap, not a minimum:
`initial=1s` allows any first delay from zero through one second. Disable jitter
to use the capped exponential delay exactly.

Give-up logs identify the originating child once. `restart_scheduled`
now reports a granted replacement after backoff/placement checks.

Embedded libraries can use `metrics::configure_composable` after actors start;
the smallest explicitly requested nonzero timing interval wins. A zero request
never disables another library's timings. Legacy `configure` retains its
first-initializer contract. For incremental managed ownership on an existing
Tokio runtime, use `system.async_group_on(name, &handle)` and keep the runtime
alive and driven through shutdown. Ambient `.start()` stays unmanaged.


- **0.7:** [timeout configuration and client shutdown migration](#preparing-for-07).
- **0.6:** no breaking API changes; existing handler/protocol signatures are
  unchanged. See the [CHANGELOG 0.6.0 section](../CHANGELOG.md#060--2026-11-06).

<a id="preparing-for-07"></a>

## Upgrading to 0.7

These APIs are available in v0.7.0. Keep ordinary `.start()`
and the default Tokio backend unless adopting explicit ownership/placement.
A dependency upgrade alone does not place existing actors in an `ActorSystem`.

### Direct 0.5 to 0.7 upgrade for Ethrex and Ethlambda

Both audited clients pin Spawned 0.5.0. Upgrade `spawned-concurrency` and
`spawned-rt` together to v0.7.0. This path includes v0.6's lifecycle corrections as well as the new
v0.7 capabilities. Ordinary 0.5 protocol/handler signatures stay in place; the
older enum-to-protocol rewrite under [Upgrading to 0.5](#upgrading-to-05) is not
needed again.

1. Keep their existing default features and backend initially. Other consumers
   that disabled defaults must apply the explicit-`async` change below.
2. Recheck shutdown assumptions: after stop is observed, queued work can be
   discarded and pending requests can return `ActorStopped`. Do not rely on a
   previously racing handler completing, retrying safely, or an arbitrary sleep
   proving cleanup. Timeouts still do not roll back effects.
3. v0.6 adds identities, monitors, links and trapped exits; they are optional.
   They do not automatically restart actors or recover persisted state.
   Abrupt future/process destruction still cannot guarantee async `stopped()`.
4. Adopt system-owned startup/shutdown and supervision subsystem by subsystem
   using the client recipe below. Replace hand-wired signals only after their
   snapshot, persistence and dependency ordering are explicit.
5. Set per-method timeouts from application requirements, then run matched
   baseline/candidate node or actor-backed workloads and fault-injected recovery.
   An isolated compile check alone does not validate client behavior.

The [0.6 changelog](../CHANGELOG.md#060--2026-11-06) records the intervening fixes.
The rest of this section provides the Cargo, timeout and shutdown details for
this complete 0.5 to 0.7 path.

### Cargo features and protocol attributes

**The no-default configuration is breaking in v0.7.** v0.6 had no default feature
set and always exported Tokio APIs. Both libraries now enable `async` by default;
disabling defaults removes `tasks` and async-only exports, including the async
`Response` constructors. Existing no-default users needing those APIs must
explicitly enable `async` on the dependency, or restore default features:

```toml
spawned-concurrency = { version = "0.7", default-features = false, features = ["async"] }
spawned-rt = { version = "0.7", default-features = false, features = ["async"] }
```

These settings select the published v0.7 line.
Thread-only applications intentionally leave `async` disabled. Optional `metrics`
does not enable it. The audited Ethrex `1dfaa09ec` and Ethlambda `c3f5f566` manifests
both use default features with Spawned 0.5.0; they do not need this Cargo edit.

v0.6 silently ignored `#[protocol(...)]` arguments. Remove obsolete arguments;
v0.7 accepts `timeout = DurationExpression` and rejects unknown or duplicate
options. Put `#[request_timeout(...)]` directly on a request method. Conditional
`#[cfg_attr(..., request_timeout(...))]`, including nested forms and false
predicates, is rejected with a specific diagnostic.

Protocols requiring `Debug` work with ordinary and timeout-configured handles.
Other custom supertraits do not become automatic implementations on
`RequestOptions`: their existing ordinary ActorRef implementations remain valid,
while configured calls require the wrapper to satisfy the protocol's supertraits.

The global registry retains `RegistryError::AlreadyRegistered`, including
exhaustive matches written against v0.6. The new managed
`Context::register_name` returns `ManagedRegistrationError`: `Registry(error)`
for a name collision, or `OwnerClosed` for an unmanaged or closing owner.
Manual `registry::register_owned` still returns `RegistryError`; its guard
needs to be retained by its caller.

### Protocol request timeouts

The default remains five seconds. Choose a protocol default, override individual
request methods, or configure one cloned concrete handle:

```rust
#[protocol(timeout = std::time::Duration::from_secs(30))]
trait ChainProtocol: Send + Sync {
    fn import_block(&self, bytes: Vec<u8>) -> Response<()>;
    #[request_timeout(std::time::Duration::from_secs(2))]
    fn status(&self) -> Response<Status>;
}

// actor is a concrete ActorRef implementing ChainProtocol.
let status = actor.with_request_timeout(std::time::Duration::from_secs(5))
    .status().await?;
```

Precedence is handle override, method attribute, protocol attribute, five-second
default. Configure before type erasure:
`actor.with_request_timeout(limit).to_chain_ref()`. A previously erased `ChainRef`
does not expose concrete-handle configuration. Existing handwritten protocol
fakes need no additional trait methods.

The synchronous API uses the same settings and blocks at the protocol method
call, returning `Response::ready`; extract with `.unwrap()`/`.expect()` or inspect
the result. A timeout still leaves the outcome of accepted work indeterminate:
queued or running handlers may execute after the caller returns. It is not a
cancellation or rollback mechanism. Retrying a state-changing operation still
needs an application-level idempotency decision.

### Ethrex and Ethlambda: own startup and shutdown

The [client audit](../ROADMAP.md#client-sweep-production-pain) pins the inspected
revisions: Ethrex `1dfaa09ec`, Ethlambda `c3f5f566`. Verify deployment revisions
before applying a recipe. This guide describes a migration, not a claim that
either client has already adopted these APIs.

| Existing pattern | Migration |
|---|---|
| Independent Ctrl-C listeners in several Ethrex actors | Keep one application signal owner. Stop ingress/producers, then invoke one system shutdown. Preserve SIGTERM handling for containers as well as SIGINT. Spawned does not install OS signal handlers. |
| Cancel the main token, query peer state after actors may have stopped, or save an empty list on `ActorStopped` | Query/snapshot and persist required state while dependencies are still live. Treat a failed snapshot explicitly; an empty fallback can overwrite useful peer data. |
| Fixed sleeps to allow shutdown | Await `system.shutdown(deadline)`, inspect the report and retain it if cleanup is incomplete. A sleep is not a join. |
| Ethlambda's hand-wired actor stops with no common deadline | Declare dependencies once, stop dependents before their dependencies, and choose a process/container grace period that includes snapshot persistence and cleanup. |
| Sequential startup that logs a failed child and continues with a partly initialized subsystem | Put fallible initialization in `ChildSpec` factories, await tree startup, and propagate failure. Failed partial startup rolls back owned resources. |
| Detached workers or listeners that outlive an actor generation | Use managed helpers where appropriate; keep existing session/generation fencing for late external results. Arbitrary raw spawns do not become owned automatically. |

A migration can proceed subsystem by subsystem:

1. Keep one `ActorSystem` alive at the application root. Build a small number of
   named `async_group`s with explicit worker budgets. Both clients depend on
   Tokio; a Tokio-free build is not this migration.
2. Admit standalone managed actors with `start_on(&group)` or declare a
   supervisor tree with `ChildSpec` factories. Declare dependencies before
   dependents and resolve dependencies again inside each fresh factory.
3. Start with OneForOne for independent children. If a dependent retains an
   instance-bound reference to an earlier child, use RestForOne or explicitly
   re-resolve the dependency after failure. Rebuilding only the upstream child
   does not rebind the downstream actor's old reference.
4. Await readiness before opening external ingress. Choose restart mode, shared
   retry intensity/window and backoff from the subsystem's recovery contract.
   A restart creates fresh state; storage recovery is application code.
5. On shutdown, stop admission at the application boundary and join producers.
   Take required snapshots, flush application work and await persistence.
   Only then close the system. Ordinary framework stop discards queued work;
   it does not automatically drain mailboxes.
6. Await the report. An incomplete report retains cleanup ownership and can be
   waited on again. Do not drop the owner and assume a stuck blocking helper was
   killed: handlers and native closures must return cooperatively.

The runnable [coordinated shutdown example](../examples/capabilities/src/bin/coordinated_shutdown.rs)
asserts a two-group dependency tree, a joined finite producer, a snapshot before
closure and reverse shutdown. Its FIFO fence relies on all producers already
having joined and does not acknowledge disk durability. The
[named registration example](../examples/capabilities/src/bin/registrations_and_fakes.rs)
shows replacement lookup and automatic managed-name cleanup.

Set restart budgets and container termination budgets separately. A shutdown
report deadline bounds how long the caller waits for a report; it cannot bound
how long arbitrary blocking application code takes to release resources.
Use `ctx.spawn_helper`/`spawn_blocking_helper` for scoped work and await its
resource cleanup. The framework does not infer an application's persistence
dependencies or retry safety.

### Validation before rollout

v0.7.0 passed the library checks and an isolated Ethlambda compile check.
A running-node canary with fault injection has not yet been completed; keep it
as the application rollout gate rather than inferring runtime results from compilation.

Pin the same client source/configuration and compare the same replay or canary
workload with only the Spawned dependency changed. Record startup, ready/stop
outcomes, unexpected restarts, queue wait, handler time, request timeouts, CPU,
RSS and OS thread count. Separate warmup from steady state and match imported
blocks; unrelated EngineClient or storage differences are not framework evidence.

Ethlambda's current [import replay implementation](https://github.com/lambdaclass/ethlambda/blob/c3f5f566cb3a61e3d388c66ce21794c18a6618ce/bin/ethlambda/src/benchmark/import/replay.rs)
calls the import function directly, bypassing actor mailboxes, the tick loop and
P2P. `replay-era` similarly targets the state transition. They are useful
component checks; neither alone validates the actor runtime. Use a real node
canary or an actor-backed replay path for application rollout validation.

The library's tests, bounded models and examples are necessary checks. Full-client
replay or a same-node Ethlambda canary remains the application rollout gate. Do not promote
synthetic backend speedup ratios to client claims. Pinned/native groups remain
opt-in; profile the client before changing its placement.

Exercise failure recovery in a canary subsystem that actually adopts supervision;
upgrading the dependency alone cannot validate automatic restarts. Inject a
controlled actor failure, verify a fresh identity/generation becomes ready and
dependents resolve its replacement, and check pending requests fail cleanly.
Then exercise restart-budget exhaustion and shutdown during backoff, confirming
the client reports failure and releases owned resources. Keep these hooks out of
ordinary production ingress. Preserve client revisions, configuration, inputs,
logs and matched baseline measurements with the results before promoting the client migration.

## Upgrading to 0.5

Spawned 0.5 replaces the enum-based `Actor` trait with protocol macros. The new API eliminates message enum boilerplate and manual dispatch — you define a trait, implement handlers, and call methods directly on actor references.

### Concept Mapping

| 0.4 (old) | 0.5 (new) | Notes |
|-----------|-----------|-------|
| `type Request = MyEnum` | `#[protocol]` trait | Protocol trait methods become message structs |
| `type Message = MyEnum` | `#[protocol]` trait (no return) | Send-only methods |
| `type Reply = MyEnum` | Method return types | `Response<T>` (request) or `Result<(), ActorError>` (send) |
| `type Error = E` | *(removed)* | Errors returned per-handler, not per-actor |
| `handle_request()` | `#[request_handler]` methods | One handler per message, no `match` |
| `handle_message()` | `#[send_handler]` methods | One handler per message |
| `init()` | `#[started]` | Lifecycle hook |
| `teardown()` | `#[stopped]` | Lifecycle hook |
| `RequestResponse::Reply(val)` | Return the value directly | Handler return type = reply |
| `RequestResponse::Stop(val)` | `ctx.stop()` + return value | Explicit stop via context |
| `MessageResponse::NoReply` | *(implicit)* | Send handlers don't reply |
| `Actor::start()` | `ActorStart::start()` | Same call, different trait |
| `actor_ref.request(enum)` | `actor_ref.method(args)` | Direct method call |
| `actor_ref.send(enum)` | `actor_ref.method(args)` | Direct method call, sync for sends |
| `handle.cast(msg).await` | `actor_ref.method(args)` | Send is now sync — no `.await` needed |
| `send_after(dur, handle, msg)` | `send_after(dur, ctx, msg)` | Takes `Context<A>`, not `ActorRef<A>` |
| `send_interval(dur, handle, msg)` | `send_interval(dur, ctx, msg)` | Takes `Context<A>`, not `ActorRef<A>` |
| `GenServerHandle<A>` | `ActorRef<A>` | Renamed |
| `Unused` | *(removed)* | No placeholder types needed |

### Before/After: Messages

**0.4** — Hand-written enums for requests and replies:

```rust
#[derive(Debug, Clone)]
pub enum NameServerInMessage {
    Add { key: String, value: String },
    Find { key: String },
}

#[derive(Debug, Clone, PartialEq)]
pub enum NameServerOutMessage {
    Ok,
    Found { value: String },
    NotFound,
    Error,
}
```

**0.5** — A protocol trait; message structs are generated:

```rust
use spawned_concurrency::Response;
use spawned_concurrency::protocol;

#[derive(Debug, Clone, PartialEq)]
pub enum FindResult {
    Found { value: String },
    NotFound,
}

#[protocol]
pub trait NameServerProtocol: Send + Sync {
    fn add(&self, key: String, value: String) -> Response<()>;
    fn find(&self, key: String) -> Response<FindResult>;
}
// Generates: name_server_protocol::Add, name_server_protocol::Find
```

`Response<T>` works in both execution modes — use `.await.unwrap()` in tasks mode or `.unwrap()` directly in threads mode. A single protocol definition works for both runtimes.

#### Generated module naming

`#[protocol]` generates a submodule containing the message structs. The naming rules:

- **Module name**: trait name converted to `snake_case` — e.g., `trait FooBarProtocol` → `mod foo_bar_protocol`
- **Message structs**: method name converted to `PascalCase` — e.g., `fn do_thing(...)` → `DoThing`
- **Type-erased ref**: strips `Protocol` suffix → `{Base}Ref` — e.g., `FooBarProtocol` → `FooBarRef = Arc<dyn FooBarProtocol>`

Import message structs from the generated module:

```rust
use crate::protocols::foo_bar_protocol::{DoThing, GetStatus};
```

#### Mixed send + request protocols

A single protocol can have both request (awaitable) and send (fire-and-forget) methods. The return type determines the kind:

```rust
#[protocol]
pub trait WorkerProtocol: Send + Sync {
    /// Request — caller awaits the reply
    fn get_status(&self) -> Response<Status>;

    /// Send — fire-and-forget, no reply
    fn notify(&self, event: Event);
}
```

On the actor side, annotate each handler to match:

```rust
#[actor(protocol = WorkerProtocol)]
impl Worker {
    #[request_handler]
    async fn handle_get_status(&mut self, _msg: GetStatus, _ctx: &Context<Self>) -> Status {
        self.status.clone()
    }

    #[send_handler]
    async fn handle_notify(&mut self, msg: Notify, _ctx: &Context<Self>) {
        tracing::info!("event: {:?}", msg.event);
    }
}
```

### Before/After: Actor Implementation

**0.4** — Single `handle_request` with `match`:

```rust
use spawned_concurrency::tasks::{Actor, ActorRef, RequestResponse};

impl Actor for NameServer {
    type Request = InMessage;
    type Message = Unused;
    type Reply = OutMessage;
    type Error = std::fmt::Error;

    async fn handle_request(
        &mut self,
        message: Self::Request,
        _handle: &ActorRef<Self>,
    ) -> RequestResponse<Self> {
        match message {
            InMessage::Add { key, value } => {
                self.inner.insert(key, value);
                RequestResponse::Reply(OutMessage::Ok)
            }
            InMessage::Find { key } => match self.inner.get(&key) {
                Some(value) => RequestResponse::Reply(OutMessage::Found { value: value.clone() }),
                None => RequestResponse::Reply(OutMessage::NotFound),
            },
        }
    }
}
```

**0.5** — One handler per message, return values directly:

```rust
use spawned_concurrency::tasks::{Actor, Context, Handler};
use spawned_concurrency::actor;

#[actor(protocol = NameServerProtocol)]
impl NameServer {
    pub fn new() -> Self {
        NameServer { inner: HashMap::new() }
    }

    #[request_handler]
    async fn handle_add(&mut self, msg: Add, _ctx: &Context<Self>) {
        self.inner.insert(msg.key, msg.value);
    }

    #[request_handler]
    async fn handle_find(&mut self, msg: Find, _ctx: &Context<Self>) -> FindResult {
        match self.inner.get(&msg.key) {
            Some(value) => FindResult::Found { value: value.clone() },
            None => FindResult::NotFound,
        }
    }
}
```

#### Handler attributes

The `#[actor]` macro supports three handler attributes:

| Attribute | Use for |
|-----------|---------|
| `#[request_handler]` | Messages that expect a reply (`Response<T>`) |
| `#[send_handler]` | Fire-and-forget messages (no return or `-> ()`) |
| `#[handler]` | Generic — works for either kind |

All three generate the same `impl Handler<M>` code. The distinction is semantic: use `#[request_handler]` and `#[send_handler]` to document intent, or `#[handler]` when you don't want to commit to a specific kind.

Handler signature: `fn name(&mut self, msg: MessageType, ctx: &Context<Self>) -> ReturnType`

### Before/After: Calling an Actor

**0.4** — Static methods wrapping `actor_ref.request(enum)`:

```rust
let mut ns = NameServer::new().start();

// Static method that constructs enum + calls request
let result = NameServer::add(&mut ns, "Joe".into(), "At Home".into()).await;
assert_eq!(result, NameServerOutMessage::Ok);

let result = NameServer::find(&mut ns, "Joe".into()).await;
assert_eq!(result, NameServerOutMessage::Found { value: "At Home".into() });
```

**0.5** — Call protocol methods directly on ActorRef:

```rust
let ns = NameServer::new().start();

// Method call on ActorRef — no enum, no static wrapper
ns.add("Joe".into(), "At Home".into()).await.unwrap();

let result = ns.find("Joe".into()).await.unwrap();
assert_eq!(result, FindResult::Found { value: "At Home".into() });
```

#### Send methods are now synchronous

In 0.4, `handle.cast(msg).await` was async. In 0.5, send-only protocol methods return `Result<(), ActorError>` synchronously — no `.await` needed:

```rust
// 0.4:
handle.cast(CastMessage::Notify(data)).await;

// 0.5:
actor_ref.notify(data); // returns Result<(), ActorError>, no .await
```

This means callers that only send messages no longer need `&mut self` or `async`:

```rust
// 0.4:
pub struct MyService { handle: GenServerHandle<MyServer> }
impl MyService {
    pub async fn do_thing(&mut self, data: String) {
        let _ = self.handle.cast(MyMsg::DoThing(data)).await;
    }
}

// 0.5:
pub struct MyService { handle: ActorRef<MyServer> }
impl MyService {
    pub fn do_thing(&self, data: String) {
        let _ = self.handle.do_thing(data);
    }
}
```

### Before/After: Timers

Timer functions (`send_after`, `send_interval`) now take `Context<A>` instead of `ActorRef<A>`.

**0.4:**
```rust
use spawned_concurrency::tasks::send_after;

// Inside a handler — clone the handle
send_after(duration, handle.clone(), CastMessage::Tick);
```

**0.5:**
```rust
use spawned_concurrency::tasks::send_after;

// Inside a handler — ctx is available directly
send_after(duration, ctx.clone(), Tick);

// Outside a handler — convert ActorRef to Context
send_after(duration, actor_ref.context(), Tick);
```

`ActorRef::context()` converts an `ActorRef<A>` to a `Context<A>`. Use it when scheduling timers from outside a handler (e.g., in a `spawn()` block or from `main()`).

#### Self-rescheduling tick pattern

A common pattern is an actor that reschedules itself with variable delays:

**0.4:**
```rust
CastMessage::Tick => {
    self.do_work();
    let next_delay = self.compute_delay();
    send_after(next_delay, handle.clone(), CastMessage::Tick);
    CastResponse::NoReply
}
```

**0.5:**
```rust
#[send_handler]
async fn handle_tick(&mut self, _msg: Tick, ctx: &Context<Self>) {
    self.do_work();
    let next_delay = self.compute_delay();
    send_after(next_delay, ctx.clone(), Tick);
}
```

### Before/After: Lifecycle Hooks

**0.4:**
```rust
impl Actor for MyActor {
    // ...
    async fn init(&mut self, _handle: &ActorRef<Self>) { /* setup */ }
    async fn teardown(&mut self) { /* cleanup */ }
}
```

**0.5:**
```rust
#[actor]
impl MyActor {
    #[started]
    async fn on_start(&mut self, _ctx: &Context<Self>) { /* setup */ }

    #[stopped]
    async fn on_stop(&mut self, _ctx: &Context<Self>) { /* cleanup */ }
}
```

**Panic behavior:** If `#[started]` panics, the panic is caught, logged via `tracing::error!`, and the actor exits immediately — subsequent sends will receive `ActorError::ActorStopped`. The `#[stopped]` hook is *not* called in this case. This applies to both tasks and threads modes.

### Before/After: Stopping an Actor

**0.4** — Return `RequestResponse::Stop`:
```rust
RequestResponse::Stop(MyReply::Goodbye)
```

**0.5** — Call `ctx.stop()` and return normally:
```rust
#[request_handler]
async fn handle_shutdown(&mut self, _msg: Shutdown, ctx: &Context<Self>) {
    ctx.stop();
}
```

### New: Type-Erased Protocol References

Each `#[protocol]` trait generates an `XRef` type alias — e.g., `NameServerRef = Arc<dyn NameServerProtocol>`. This lets actors hold references to protocol implementors without knowing the concrete type:

```rust
pub struct Logger {
    target: NameServerRef, // any actor implementing NameServerProtocol
}
```

Convert an `ActorRef<A>` to an `XRef` via the generated `ToXRef` trait:

```rust
let ns: ActorRef<NameServer> = NameServer::new().start();
let ns_ref: NameServerRef = ns.to_name_server_ref();
```

### New: Actor Registry

A global, name-keyed registry for discovering actors at runtime:

```rust
use spawned_concurrency::registry;

// Register
registry::register("main_server", ns.clone()).unwrap();

// Look up
let found: Option<ActorRef<NameServer>> = registry::whereis("main_server");

// List all names
let names: Vec<String> = registry::registered();

// Unregister
registry::unregister("main_server");
```

### Import Changes

| 0.4 | 0.5 |
|-----|-----|
| `spawned_concurrency::tasks::Actor` | `spawned_concurrency::tasks::{Actor, ActorStart}` |
| `spawned_concurrency::tasks::ActorRef` | *(same)* |
| `spawned_concurrency::messages::Unused` | *(removed)* |
| `spawned_concurrency::tasks::RequestResponse` | *(removed — return values directly)* |
| `spawned_concurrency::tasks::MessageResponse` | *(removed)* |
| *(n/a)* | `spawned_concurrency::tasks::{Context, Handler}` |
| *(n/a)* | `spawned_concurrency::Response` |
| *(n/a)* | `spawned_concurrency::{actor, protocol}` |
| *(n/a)* | `spawned_concurrency::registry` |

### spawned-rt Changes

`spawned-rt` has **no breaking API changes** in 0.5. Both `spawned_rt::tasks` and `spawned_rt::threads` retain the same public exports (`spawn`, `sleep`, `CancellationToken`, `mpsc`, `oneshot`, etc.). Update the version in your `Cargo.toml` but no code changes are needed for `spawned-rt` imports.

### Escape Hatches

If the macros don't fit your use case:

- **Manual `Message` impls** — Define message structs and implement `Message` by hand instead of using `#[protocol]`.
- **Manual `Handler<M>` impls** — Skip `#[actor]` and implement `Handler<M>` for each message type by hand.
- **`Recipient<M>`** (`Arc<dyn Receiver<M>>`) — Type-erased reference for a single message type, available via `ctx.recipient::<M>()`.
