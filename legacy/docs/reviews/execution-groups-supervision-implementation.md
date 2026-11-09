# Execution groups and OneForOne: implementation review

Reviewed 2026-11-08 at the user's request. This implements the initial
[ownership/recovery contract](../design/execution-groups-supervision.md), with
[usage guidance](../EXECUTION-GROUPS.md) and two asserted examples. It remains
unreleased. This record covers parked async groups and OneForOne only.

**Final AGY verdict: PASS — Ready to merge. No Critical, High, or Medium issues remain.**
AGY reviewed supplied source/diff packets without running tools or tests. Codex
ran the separate validation below. A static review is not proof of freedom from
bugs, a performance result, or approval of a production rollout.

## Findings addressed

- Observe actor cancellation independently of terminal completion, so a stuck
  `stopped()` hook starts a cleanup deadline. Retain actual actor/helper ownership
  and never start an overlapping replacement.
- Readiness waits for the real startup outcome instead of fabricating an exit
  cause from an earlier cancellation. Preserve actual startup exits/identities.
- Keep retry classification for a startup timeout consistent across factory and
  `started()` stages. A self-requested normal pre-ready exit remains a failed
  Transient/Temporary startup. Zero retries fail without waiting for backoff.
- Order supervisor units and standalone actors together by admission ID. Separate
  admission closure from ordered stop. At grace expiry signal remaining nested
  siblings and scopes before retaining/joining unfinished cleanup.
- Serialize shutdown snapshot initialization, including concurrent zero-deadline
  callers. Wake replacement lookup waiters when admission closes.
- Freeze ready uptime before every cleanup path; slow `stopped()` cleanup must
  not reset a child's exponential-backoff history.
- Reject supervision placement into a different system's group; compare registry
  ownership nonces so stale guards cannot remove replacement registrations.

The approved contract closes all helper/timer/monitor/registration admission at
shutdown start, including earlier actors awaiting their stop turn. Those actors
can still serve ordinary requests and handle helper rejection. AGY accepted this
explicit stop-and-join contract; an admission-preserving drain policy is separate.
The rolling restart budget is an attempt-rate limit, not a lifetime retry cap.
Backoff-spaced attempts outside the window may continue by design.

The remaining low observation is diagnostic snapshot lag: immediately after a
stop signal a snapshot can briefly still say Running. `current()` already rejects
lookup when admission closes. The usage guide explains this distinction.

## Validation

The evidence directory records exact source hashes, gate commands, toolchain
notes, outputs and resource limits. Final gates are recorded in
[validation.json](execution-supervision-implementation-evidence/validation.json).

- 33 focused integration tests: 14 placement/helper/shutdown tests and 19 recovery
  tests. They include startup payload/reply discard, generated protocols,
  ambient unmanaged actors, helper panic/abort/native completion, constructor
  cancellation/rollback, restart modes/budgets, startup timeouts, stale references,
  registration guards, shutdown order, concurrent reports and stuck cleanup.
- Pure policy tests cover rolling-window boundaries and reproducible capped
  backoff/jitter. The metrics integration test checks conservation across managed
  crash/replacement/shutdown as well as existing modes.
- Full workspace tests run with default and all features on Rust 1.93.0.
- Strict workspace Clippy checks run for all targets with default/all features.
- Eight asserted capability programs include execution_groups and
  supervision_recovery; CI runs both new programs too.
- Repeated lifecycle-race execution and its exact count are in
  [stress.json](execution-supervision-implementation-evidence/stress.json).

Heavy validation uses the shared fede host, CPUs 2–5, nice 10, one build job and
four Rust test threads. A process-group watchdog samples summed RSS against a
2 GiB ceiling and a 30-minute per-command limit. These are correctness checks,
not isolated performance measurements or exhaustive delivery-schedule simulation.

## Review evidence and boundaries

[Initial review](execution-supervision-implementation-evidence/initial-review.json)
requested changes. [Revised review](execution-supervision-implementation-evidence/revised-review.json)
was conditional. [Final follow-up](execution-supervision-implementation-evidence/final-followup.json)
returned the PASS above after fixes and explicit contract clarification. The
[review record](execution-supervision-implementation-evidence/review-record.json)
identifies packet hashes and reviewed source. Only a test-handle expression was
changed to explicit `drop(...)` after that pass to satisfy Clippy; runtime source
was unchanged, and the final test source is recorded separately.

Native blocking work remains cooperative and can retain a closed runtime
indefinitely. Arbitrary raw spawns remain unmanaged. References are instance-bound;
there is no state restoration, message replay or exactly-once effect guarantee.
Dedicated/pinned group profiles, nested supervisor child specs, OneForAll,
RestForOne, Tokio-free builds and bounded/draining mailboxes remain separate work.
A full Ethlambda/Ethrex canary/replay is still needed before production performance
claims; no release or production deployment is performed by this change.
