# Follow-up review: v0.7.1 lifecycle patch (round 2)

**Scope:** static reading of the supplied source, docs, tests and diff only. Nothing was compiled or run. The Ethrex RPC 2 MiB debug rerun is still pending externally and is not assessed. Nothing here authorizes a merge, tag or release.

## Verdict

**No confirmed correctness defects.** One item needs confirmation because it falls outside the supplied code. Three low-severity notes follow it.

## Prior review items, rechecked

| Prior item | Status |
|---|---|
| CHANGELOG, GUIDE and MIGRATION missing | **Resolved.** All three now describe historical readiness, the ready-then-dropped startup future, `failure()` after `Ok`, pending-reservation capacity and the full-jitter `initial` cap. They match the code (`SupervisorBuilder::start_owned` reads `core.ready`; `StartupGuard::drop` calls `cancel_unready_startup`). |
| No test for ready → failed → `start()` observed | **Resolved** by `startup_success_remains_historical_after_unobserved_ready_then_failure`. Even when the `select` returns via `cancelled`, the result comes from `*core.ready.borrow()`. This makes the `Ok` + `failure().contains("budget exhausted")` assertion deterministic. |
| No exact assertion for Normal + state-drop panic | **Resolved.** `native_drop_child` now asserts `Panic("actor state destructor: native state destructor")` exactly. |
| Native `rx` kept open through state destruction | Accepted as deliberate. Ordering is still state drop → `drop(rx)` drain → `complete`/`complete_secondary`. |
| `metrics.finish` called outside containment | Still uncontained in `ActorCleanup::drop`. This is unchanged scope, not a regression. |

## Rechecked focus areas: no defects

- **`close_unless` / `cancel_unready_startup`**
  - `ready` is only set inside `gate.if_open`, and `close_unless` reads it under the same admission mutex. Either readiness commits first and rollback is skipped, or the gate closes first and readiness can never commit.
  - When the gate was already closed, `close_unless` returns `true`. The following `close()` then only cancels; the trace events are not duplicated.
- **Panic ordering when the actor state is destroyed**
  - **Tasks:** `actor` is moved into `finish_actor_state` before the `rx` parameter drops at the end of `run_actor`. So the state destructor runs, then the drain, then the caller's completion.
  - **Native:** the same order, made explicit with `drop(rx)`.
  - **Primary outcome:** a handler, `started` or `stopped` panic arrives as `Ok(Panic)`, which is abnormal, so it is kept. A lifecycle unwind arrives as `Err` and is kept. A committed non-Normal cause still wins through `complete_secondary`.
- **`dispose_panic` in `deliver`**
  - Both the callback payload and the subscriber payload are disposed of under `catch_unwind`.
  - The test's expected count of 2 is correct.
  - Tracing's dispatcher re-entry guard resets during unwind.
- **Compact async sender**
  - EOF still depends only on the last `Arc<SendState>`; the receiver retains no sender.
  - The drain runs `close()` and then `recv` until it returns `None`, so it does not wait for senders to drop.

## Needs confirmation (medium)

**Nested or factory-driven supervisor startup dropped after readiness.**

`StartupGuard::drop` now keeps any tree whose readiness has committed. That suits a top-level `start()`, where the ActorSystem orders shutdown. The risk is where a *parent* owns the startup future, for example:

- `start_owned(false)` for a nested tree, or
- a `ChildSpec` factory that awaits a supervisor's `start()` under the spec's startup timeout.

If the parent drops that future in the narrow window after the nested readiness commits but before it resumes:

1. The parent records a startup failure and may retry.
2. The first nested tree remains live without a handle.

That would break "at most one live generation per child slot", and the orphan would last until system shutdown.

Before this round's change, the guard closed such a tree. Please confirm one of the following:

- (a) the `owned=false` path cannot reach this guard behavior, or
- (b) parent-driven cancellation still rolls back.

If neither holds, keep `close()` for non-system-owned startups and add a test that fires a nested startup timeout after readiness.

## Low-severity notes

1. **`future_layout.rs` uses exact equalities.** Assertions like `size_of_val(&send(actor)) == size_of_val(&send(baseline))` depend on the compiler's coroutine layout. `ActorRef` contains an `Arc` with a niche and `Baseline([usize; N])` has none. If rustc starts placing coroutine discriminants in niches, the real futures would become *smaller* and the test would fail with no regression. Since the test guards against growth, use `<=`.

2. **Asymmetry in what a destructor panic may override.**
   - `finish_actor_state` overrides any primary that is `!is_abnormal()`.
   - A queued-destructor failure overrides only `Normal`.
   - If `is_abnormal(Shutdown)` is false, an *uncommitted* `Ok(Shutdown)` would become `Panic("actor state destructor: …")`. A queued-destructor failure would leave it alone.

   I can't tell whether an uncommitted `Shutdown` is reachable. Committed causes are protected by `complete_secondary`. Consider matching on `Normal` for consistency.

3. **Observable panic-message rename is undocumented.** `"actor lifecycle destructor"` became `"actor lifecycle unwound"` in both backends, and the string is visible through `ExitReason::Panic`. Neither the CHANGELOG diff nor MIGRATION mentions it. Also confirm that no remaining test or doc asserts the old string.

## Not verified

- Compilation or any test run, with or without `metrics`. CI must exercise both `BASELINE_WORDS` variants.
- That `core.ready` is never reset to `false` after a failure. The historical-readiness logic assumes this.
- Doctests in `testing/README.md` (the `Mock` / `join_within` API) and the `#testing-your-actors` anchor.
- The TLA+ trace expectations for the close events that are now skipped when a startup future is dropped after readiness.
- The Ethrex client run.

## Maintainer disposition

- Nested ownership confirmed in `supervision/nested.rs`: `SupervisorActor::started`
  registers a tracked lifecycle helper **before** awaiting `start_owned(false)`.
  It closes the inner core on parent cancellation and waits for inner completion;
  the parent scope retains that helper until it finishes. The explicit parent
  cancellation branch also calls `core.close()`. Thus a dropped ready nested
  startup remains owned and cannot outlive parent cleanup into a replacement.
  Existing nested cancellation/failed-startup regressions pass on this patch.
  Calling the public `start()` from an arbitrary factory instead registers an
  independent ActorSystem root; it does not create a nested child. Documented
  historical readiness applies to that independent root as well. Use `add_supervisor`
  for parent-owned nested recovery.
- Size comparisons now permit smaller futures (`<=`) to avoid failing on a
  compiler layout improvement. Measured data remains unchanged.
- State-destructor failure now escalates only Normal. Even an uncommitted
  Shutdown is retained; a direct unit test covers Normal/Shutdown/Kill/handler
  Panic/lifecycle unwind without relying on a shared committed-cause slot.
- The migration guide now documents the diagnostic text distinction/rename.
- The metrics comment does not describe a user callback here: terminal actor
  observations update internal atomic counters only.
- `Core::ready` is initialized false and only set true; per-round readiness
  channels are separate. No reset invalidates the historical commitment.

These are bounded static-review dispositions, not Ethrex client validation or
release approval. The maintainer still needs the actual 2 MiB RPC rerun.
