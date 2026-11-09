# v0.7 supervision and execution-group review

Review of `be81a329` / `5273a5fa`, 2026-11-09. The nine requested fixes are
implemented; release preparation and the Ethlambda canary remain separate gates.

| Review item | Resolution and regression coverage |
|---|---|
| Native slot inheritance | `Scope::group()` clears exclusive native-slot selection while retaining explicit pinned placement. A native actor on slot 0 starts a peer on free slot 1; exhaustion still rejects. Linux pinned actors start peers on the same CPU. |
| Missing terminal traces | Both early retirement paths record `Finish` inside the publication section. Cohort tapes assert exactly one `Finish` per controller, including Temporary and normally retired Transient children. |
| Native integration tests gated away | Gate async actors, variants and match arms inside shared suites. Native scenarios, controlled failures, histories, delivery simulation, link macros, timers and payload stress run without async. CI checks seven required integration-test names. |
| Envelope documentation | Move ExitEnvelope's documentation back onto its declaration. |
| Lifecycle helper documentation | Document dispatch and uncancelled lifecycle tracking at their own methods. |
| Construction rollback | Reuse `rollback()` if the join coordinator cannot be created. |
| Worker validation | Reuse `ExecutionProfile::workers()`. |
| Unconfigurable helper budget | Expose `thread_group_with_helpers`; validate actor and helper budgets independently in 1..=1024. Preserve the one-helper-worker default. |
| OneForOne recovery watcher | Do not subscribe to `recovery_stopping()` for OneForOne backoff. |

The fresh AGY review also identified reliance on the optional trace section for
production gate/failure decisions. `Admission::close_if_open` now commits a child
failure and closes admission under the actual production mutex; readiness is
published inside `if_open`. Cleanup failures still propagate after ordinary stop.
A unit regression exercises stop winning over a late failure and readiness.
Stable generations reset their failure streak before supersession, and an
interrupted backoff no longer increments the streak for an unattempted retry.

Nested handle lookup waits through inner-gate closure until an actual replacement
or terminal state. Cancellation during nested startup does not manufacture a
panic. Genuine startup errors remain synchronous: the suggested alternative of
enqueuing a failure was tried, and the existing integration test caught false
readiness before the queued failure ran. That alternative was removed.

A normally retired Transient slot remains retired. Concurrent assignment and
retirement have a defined linearization under the slot mutex: assignment that
wins before terminal publication can include the controller. Temporary children
are never reconstructed. State and queued work are not replayed, references remain
instance-bound, handlers remain serial, and shutdown cannot preempt blocking code.

## Formal and implementation checks

The [model documentation](../../../verification/tla/README.md) defines the bounds
and abstractions. The existing OneForOne model is retained. New cohort models
cover both strategy ranges, cleanup before reconstruction, retired slots, stale
notifications and superseding rounds. The ownership model covers dynamic nested
supervisors and parent completion only after descendant completion, with explicit
fair cancellation/cleanup assumptions.

Coordinator bookkeeping can finish after admission closes. This is represented
explicitly; starting or publishing a replacement after closure remains forbidden.
Real traces exposed this distinction before release.

On `fede`, under CPU mask 2,3,4,5, nice 10, one build job and a sampled 2 GiB RSS
limit:

- Production-feature tests (metrics enabled, tracing disabled), native-only
  integration/metrics tests, and strict Clippy for both configurations passed.
- Native payload stress passed with 128 producers and 256 messages each, testing
  both normal delivery and queued-payload destruction on stop.
- Twenty repeated production-mode runs of supervision, strategies, nested trees
  and execution profiles passed (960 test executions). Tracing's global section was disabled.
- Real OneForOne, cohort and nested tapes passed TLC replay. The final tape set
  includes retirement, supersession, three-level mixed-strategy replacement and
  a native helper retained beyond a shutdown deadline.
- Four invalid trace edits were rejected; six deliberately broken model variants
  produced counterexamples. An additional invalid-final-event control verifies
  safety takes precedence over the full-trace completion sentinel.
- All fast configurations passed, including the existing OneForOne safety/action/
  liveness configuration and the new strategy, supersession and nested models.

AGY's final review and confirmation returned **PASS**, with zero remaining
reported actionable issues. The CLI transport reported an interrupted stream
after delivering those responses; the raw verdicts and transport status are
preserved rather than described as clean CLI completions.

[Evidence](../../verification/evidence/v07-review/) includes raw test/model logs,
real JSON tapes, review packets/responses and source hashes compared with the
server's files. GitHub CI and ThreadSanitizer run after push; server verification
does not replace those checks. These bounded models and sampled runs do not
constitute a proof of every Rust/Tokio schedule or a client performance claim.

## Release boundary

Do not publish v0.7 until protocol/method request timeouts and per-call override,
the clients' shutdown migration guide, missing examples and the Ethlambda
validation gate are complete. Deferred replies and pipe-to-self follow the release.
Tokio-free ActorSystem/execution groups/supervision remain future work.
