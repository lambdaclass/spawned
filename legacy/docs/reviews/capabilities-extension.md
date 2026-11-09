# Tokio-free builds, supervision trees and execution profiles

Implementation targets v0.7; no release or client deployment is implied.
Based on `1c450780` (#192), then rebased over #193's documentation-only
reorganization, preserving its verification harness and
`forbid(unsafe_code)` in concurrency, rt and macros.

## Scope

- Async defaults remain enabled. Disabling them on both crates gives a genuinely
  Tokio-free native dependency graph; typed protocols select code from dependency
  features rather than a similarly named consumer feature.
- Nested supervisor child specs, OneForAll and RestForOne join actual affected
  cleanup before ordered reconstruction. Temporary collateral children retire;
  fresh constructors consume the shared rolling restart budget. Old references
  stay attached to the old actor/tree instance.
- Owned parked Linux pinned shards and fixed native actor-thread pools. Native
  scopes reserve one persistent slot and share one helper runtime. Full admission
  rejects rather than queues. Construction rolls back started owners on failure;
  shutdown completion waits for actual worker joins and TLS/runtime teardown.

No mailbox/reply transport change, busy-spin default, transparent state recovery,
replay or full-client speedup is claimed. Native-only actors use the separate
synchronous API; ActorSystem/supervision still require async.

## Review-driven corrections

AGY reviewed supplied source, not independent executions. Initial and follow-up
reviews required changes; the final supervision review passed. Transcripts,
compressed source packets and validation metadata are in
[the evidence directory](capabilities-extension-evidence/).

- Recovery coverage epochs absorb old-generation failure notifications before
  per-child assignments can race with them. New upstream failures supersede an
  in-progress reconstruction; parked controllers leave obsolete resume waits.
- Temporary retirement commits terminal state under the assignment lock before
  acknowledging recovery. A later round either skips it or is serviced first.
- Closed helper admission during nested startup returns cancellation without
  inventing a panic. Permanently aborted initial startup immediately rolls back,
  even while a later system-shutdown target is blocked.
- Nested observers retain ownership through actual descendant completion. A
  separate first cleanup cause preserves late/secondary timeouts without turning
  ordinary nested startup failures into fatal parent cleanup failures.
- During OneForAll/RestForOne initial startup, a restartable exit of an already-ready
  dependency fails and rolls back the tree. It never silently replaces that actor
  underneath still-initializing dependents. A parent can retry the whole subtree.

The final review notes a readiness-commit boundary where a ready supervisor handle
can be returned immediately before initial-tree failure closes it. Readiness does
not promise continued liveness. Coordinated-strategy early exits do not yet emit
all model trace events. #192's formal model/trace validation remains OneForOne;
cohort/nested correctness is covered by tests/review, not claimed formally verified.

The native blocking bridge uses a wake permit per invocation, so nested waits
cannot consume each other's wakeups; it creates no Tokio driver or helper thread.

## Validation

The recorded commands use server `fede`, CPUs 2–5, nice 10, one build job,
four test threads, a sampled 2 GiB process-group RSS cap and 30-minute check
limits. Reduced debug information avoids a compiler-only all-feature memory spike.
Rust tests/docs use 1.93.0; strict Clippy uses matched rustc/Clippy 1.98.1 without
warning exemptions. GitHub CI uses 1.93.0.

All 19 feature-matrix/documentation/example checks and five strict Clippy
configurations passed. Twenty stress repetitions passed (47 test cases per
repetition, including 1,280 explicit recovery/shutdown race iterations overall).
TLC accepted all 20 recorded OneForOne traces. The new native nested-wait test
fails the old executor and passes the corrected implementation.

Final source hashes and raw logs are recorded with the evidence. Coverage includes default/all-feature workspace tests, native feature combinations,
Tokio-free graph/fixture checks, strict documentation and Clippy, finite examples,
resource-failure injection, repeated lifecycle stress, and retained OneForOne traces.
Production Ethlambda/Ethrex replay/canary and expanded formal coverage remain
separate validation work.
