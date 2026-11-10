# Review: PR209 / PR217 subscriber-panic containment fixes

**Verdict: no blocking correctness bugs in the shown diff.** The change is narrow and does what it claims. There are residual gaps below, and I couldn't verify several of them because I only had the diff (no source reads).

## What's correct

- **`log_safely`**
  - `catch_unwind` + `dispose_panic` is the right shape.
  - `msg` is computed before the closure, so a subscriber panic can't affect the exit reason.
  - The closures only borrow `msg`, `spec`, `previous` and `backoff`, so `AssertUnwindSafe` is sound here.
  - tracing-core's global-dispatch path holds no thread-local re-entrancy state. The scoped path restores `can_enter` through a drop guard on unwind. A caught subscriber panic therefore doesn't wedge later dispatch on that thread.
- **Lifecycle sites (tasks and threads):**
  - All six `tracing::error!` sites in `started`, cancelled-before-loop `stopped`, handler and final `stopped` are wrapped symmetrically.
  - Ordering is preserved: `cancellation_token.cancel()` still runs after the started log, and `exit_reason` assignment still follows the log.
  - Abnormal-reason precedence in final `stopped` (`!exit_reason.is_abnormal()`) is unchanged, so "handler panic + stopped panic" still keeps the handler reason.
- **Supervisor:**
  - Both restart info events are wrapped independently, so a panic in the first doesn't skip the second.
  - `view.changed.send_modify` and factory invocation now always run.
- **Coordinator test:**
  - The refresh loop is bounded by `LIMIT`.
  - It fails with `expect` instead of hanging, provided `ShutdownReport::wait(1ms)` actually yields. It almost certainly does, since it arms a tokio timer.
  - The outer `report` is moved into the `async move` block and then reassigned, which is valid.
  - The reasoning (a report is a snapshot, not an acknowledgement) is sound. No production change.
- **Test harness:**
  - `spawn` returning `Option<Outcome>` keeps the "never hang CI" property.
  - `--test-threads=1` correctly serialises global-subscriber/hook installs within a child.
  - Startup-panic expectations match the existing contract: `stopped()` is not run, checked via `stopped == startup_panic`.

## Residual risks / scope limitations (not confirmed bugs)

1. **Only `Subscriber::event` is hostile in the test.** `enabled`, `register_callsite`, `new_span`, `enter`, `exit` and `record` never panic.
   - If any actor or supervisor future is wrapped in a span (`.instrument(...)`, `span.enter()`, or whatever `scope.set_name` does), a panic in `enter` would fire on every poll, outside `log_safely`.
   - Those calls would escape the lifecycle entirely.
   - Worth one more child variant that panics in `enter`/`new_span`, or confirming there are no spans on these paths.

2. **Other supervisor/system logs are still unwrapped**, assuming they exist (the diff doesn't show them):
   - child-exit observed
   - restart-intensity exceeded / escalation
   - shutdown-ordering logs
   - `ActorCleanup` / link/monitor delivery logs

   The strict restart test only covers the happy path (`max_restarts: 10`, zero backoff). The intensity-exceeded and escalation paths run under a panicking subscriber with no coverage. The CHANGELOG wording ("supervision restart events") is accurate, but a reader may take it as broader. I'd grep `supervision.rs`, `system.rs` and `actor_cleanup.rs` for remaining `tracing::` calls on recovery paths.

3. **Any tracing call in a `Drop` that can run during unwinding** turns a subscriber panic into an abort (panic while panicking), and `log_safely` can't help there. This is not touched by the diff, but if `ActorCleanup::drop` logs, that is the one place the containment claim doesn't hold.

4. **The original lifecycle panic payload is still dropped normally.** `panic` falls out of scope at the end of each `if let Err(panic)` block rather than going through `dispose_panic`. A user `panic_any(T)` where `T: Drop` panics would re-panic in `run_actor_lifecycle` after the log. This is pre-existing and may be handled by an outer layer I can't see. If it isn't, it's the same class of hostile-payload bug this PR fixes for subscriber payloads.

5. **Restart coverage is 2 of 4 placements.** `supervised_restart` returns `true` for `Ambient`/`Native`. That's intentional, since supervisors need a group, but `restart_gaps` can never fire for those placements.

6. **Docs nit:** the `sh` block under the `docs/TESTING.md` table still lists only `--test cross_backend --test cancellation_safety --test panic_safety`. Add `--test foreign_panics` so the documented command matches the new row.

7. **Test cost:** all three subscriber parents each spawn the full `subscriber_child` across 4 placements. That's fine, but you could merge them into one child run with both strict areas checked and gaps reported separately.

## Scope

- Reviewed from the provided diff, the full `foreign_panics.rs`, `drop_subprocess.rs` and `dispose_panic` only.
- I did not inspect the remaining `tracing::` call sites, span usage, `ShutdownReport::wait` semantics or `wait_for_generation` semantics.
- I relied on your report that full macOS lib and foreign-panic suites pass under default and `metrics+diagnostics`.
- No merge, tag or publish action implied; keep the PR as draft.

## Disposition

No blocking correctness finding. Scope clarification: there are eight lifecycle
error-log sites, four per mode. The audit found no production span instrumentation
on these lifecycle/restart paths. Other supervisor failure, state/mailbox cleanup
and link diagnostics already have catch_unwind + dispose_panic boundaries; system
shutdown has no tracing calls. A local catch inside a destructor prevents the
subscriber unwind from escaping it, so the generic Drop concern does not invalidate
those boundaries. The final refresh loop explicitly sleeps before a zero-deadline report refresh,
so even an immediately returning/closed-watch regression cannot starve its timeout; the macOS failure was reproduced and corrected in
full-library runs, without global serialization or a production change.

The normal diagnostic command now includes foreign_panics. Restart assertions
apply to the two managed group placements; ambient/native cases exercise actor
lifecycle handling, not automatic supervision. Startup-only and shutdown-only
panics supplement the upstream cases. Duplicate parent executions preserve the
upstream regression names as requested. The broader original panic_any-payload
destructor question is pre-existing and is not changed by this tracing-only fix;
this record makes no universal foreign-panic containment claim.
