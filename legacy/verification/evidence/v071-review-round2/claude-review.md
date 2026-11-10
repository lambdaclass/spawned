# Review: PR209 uncommitted lifecycle fixes (read-only)

## Context
Review of the uncommitted v0.7.1 lifecycle patch on `fix/v0.7.1-lifecycle` for correctness regressions in four areas: the compact Arc async sender, primary-panic preservation, the historical supervisor startup guard, and link-delivery panic containment. No files edited, no tests run. The Ethrex RPC 2MB debug rerun is still pending externally and is not claimed here.

## Verdict on the four focus areas: no correctness regressions found

1. **One-word Arc sender (`mailbox.rs` `tasks::Sender`/`SendState`)**
   - The receiver holds only `raw::Receiver` + `failures`, never a sender. EOF still happens when the last `Arc<SendState>` drops.
   - `Receiver::drop` is the same `close()` + `block_on_drop(unconstrained(recv loop))` as before, so drain-through-EOF is unchanged.
   - No `downgrade`/`is_closed`/`closed()` uses of the raw sender exist that the wrapper could break.
   - The size check is 1 word vs 1 word.

2. **Primary panic vs actor `Drop` panic (`actor_cleanup.rs::finish_actor_state`, `tasks/actor.rs` `run_actor`, `threads/actor.rs` spawn closure)**
   - Handler and `stopped` panics surface as `Ok(ExitReason::Panic)`, which `is_abnormal`. A lifecycle unwind surfaces as `Err`. Both keep primary.
   - State drop now happens outside the lifecycle `catch_unwind`.
   - Ordering is correct on both backends: the state drop, then `drop(rx)`, then `complete`/`complete_secondary`. In tasks this is `run_actor` returning before `drop(rx)` at `tasks/actor.rs` ~1046.
   - Tests `handler_panic_remains_primary_when_state_drop_panics` and `native_state_destructor_panic_is_secondary_to_handler_panic` cover this.

3. **Historical startup guard (`supervision.rs` `SupervisorBuilder::start`, `StartupGuard::drop`, `Core::cancel_unready_startup`, `system.rs` `Admission::close_unless`)**
   - `ready` is only set inside `gate.if_open`, under the admission mutex.
   - `close_unless` checks `ready` under the same mutex, so there is no TOCTOU between readiness and the rollback close.
   - The lock order (admission, then watch) matches the `if_open` path.
   - `close()` still cancels unconditionally after `close_with` returns false, so there are no duplicate trace events.
   - `trace::atomic` is reentrant (depth counter).
   - `Supervisor` has no `Drop`, so leaving a ready tree running when `start()` is dropped is consistent with system ownership.

4. **Link delivery containment (`link.rs::deliver`)**
   - The callback payload is disposed via `dispose_panic`, which catches a payload-destructor panic and forgets the secondary payload.
   - The `tracing::error!` call is wrapped, and the subscriber panic payload is disposed the same way.
   - The test asserts the call does not unwind and that both payload destructors ran.

## Unaddressed items / concerns (not correctness regressions)

1. **CHANGELOG `Unreleased` not updated.** The Unreleased section has no entries for this patch, which CLAUDE.md requires. Changes that need an entry:
   - The historical `start()` readiness.
   - State-destructor reclassification: `Normal` becomes `Panic("actor state destructor: …")`, while abnormal outcomes are kept.
   - The exit message rename from `"actor lifecycle destructor"` to `"actor lifecycle unwound"` (`tasks/actor.rs` `run_actor`, `threads/actor.rs` spawn closure).
   - Link delivery containment.

   The `docs/GUIDE.md` and `docs/MIGRATION.md` diffs also do not mention the `start()` readiness semantics.

2. **Behavior change in `SupervisorBuilder::start`.**
   - If the tree escalates or fails *after* readiness but before the caller polls, `start()` now returns `Ok(Supervisor)` instead of `Err(Failed(msg))`. Callers must check `tree.failure()`.
   - Dropping the `start()` future after readiness no longer rolls back the tree. It now lives until system shutdown with no handle.
   - Both are intended ("historical, like `ActorRef::ready()`") but are undocumented.
   - There is no test for the "ready, then failed before observe" case. The new test only covers the system-admission-close case.

3. **Threads mailbox stays open longer (`threads/actor.rs` spawn closure).** `rx` now outlives `finish_actor_state`. External `send()` calls keep returning `Ok` during the state destructor, and those messages are then drained and discarded. This is benign but widens the window; pending `request()` replies fail later rather than sooner.

4. **Test gap.** I found no assertion on the new `"actor state destructor: "` message for the `Normal` + state-drop-panic path (grep only finds the source string).

## Limitations
- I did not compile or run any tests (plan mode).
- I did not check whether the TLA+ trace model or validation expects a `CClose` when `start()` is dropped after readiness.
- Containment is not total:
  - `self.metrics.finish(&reason)` in `ActorCleanup::drop` calls a user `metrics` recorder uncontained (`metrics` feature).
  - A global panic hook that re-enters a panicking subscriber can still abort; that is unavoidable.
- The Ethrex RPC 2MB debug rerun is pending externally and has not been verified.

## Verification (if acting on items)
- `cargo test -p spawned-concurrency --all-features` and without `metrics`, including the `future_layout`, `mailbox_drop_panic` and `native_mailbox_drop` tests and the new supervision `policy_tests`.
- Add a test where a ready supervisor fails before `start()` is polled, and assert the chosen semantics.


## Maintainer disposition

This was a focused static packet review, not a client run or release approval.
The packet omitted documentation, so its missing-doc comments were based on
incomplete context: CHANGELOG, GUIDE and MIGRATION were updated. It found no
correctness regression in the four requested areas. A new regression additionally
covers ready-then-failed-before-start-is-observed; the native Normal/state-drop
case now asserts the exact destructor reason. Existing committed Shutdown
protection remains covered. The noted sender-admission window retains discard
semantics; native receiver lifetime through state destruction is deliberate.
The reviewer referred to conceptual test names; actual tests are
`handler_panic_survives_state_and_queued_destructor_panics`,
`native_mailbox_drop_panics_are_contained_before_completion`, and
`startup_completion_or_drop_after_readiness_preserves_system_order`.

The review's suggested uncontained user metrics recorder does not apply to this
implementation: `metrics::ActorObservation::finish` only updates internal atomic
counters, with no external recorder callback. This was checked in source.
